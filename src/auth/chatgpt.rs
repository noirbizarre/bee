//! Official OSS/local Sign in with ChatGPT, isolated from API-key authentication.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Audience, Credential, CredentialStore, FileStore, Secret, unix_now};
use crate::error::{Error, Result};
use crate::http::LoginHttp;
use crate::login::Pkce;

const ISSUER: &str = "https://auth.openai.com";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const DIRECT: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

pub(crate) fn failure(reason: &'static str) -> Error {
    Error::ChatGpt { reason }
}

pub(super) fn store_state(credential: &Credential, now: u64) -> super::StoreState {
    let r = credential.oauth.as_ref().expect("OAuth record");
    if credential.secret.expose().is_empty() {
        super::StoreState::NotLoggedIn
    } else if !r.scopes.iter().any(|s| s == DIRECT) {
        super::StoreState::PlanDisabled
    } else if credential.is_expired(now) && r.refresh.is_some() {
        super::StoreState::Refreshable
    } else if credential.is_expired(now) {
        super::StoreState::Expired
    } else {
        super::StoreState::LoggedIn
    }
}

/// A turn pins registration identity while allowing its token set to rotate.
#[derive(Clone)]
pub struct Session {
    store: FileStore,
    profile: String,
    identity: (String, String),
    http: std::sync::Arc<dyn LoginHttp>,
}

impl Session {
    /// Bind an authenticated account, independent of its current access token.
    pub fn new(
        store: FileStore,
        profile: String,
        credential: &Credential,
        http: std::sync::Arc<dyn LoginHttp>,
    ) -> Result<Self> {
        let registration = credential
            .oauth
            .as_ref()
            .ok_or_else(|| failure("missing ChatGPT registration"))?;
        Ok(Self {
            store,
            profile,
            identity: (registration.client_id.clone(), registration.subject.clone()),
            http,
        })
    }

    pub(crate) async fn credential(&self) -> Result<Secret> {
        let credential = resolve(&self.store, &self.profile, self.http.as_ref()).await?;
        let r = credential
            .oauth
            .as_ref()
            .ok_or_else(|| failure("registration no longer exists"))?;
        if (&r.client_id, &r.subject) != (&self.identity.0, &self.identity.1) {
            return Err(failure(
                "selected ChatGPT account changed; start a new turn",
            ));
        }
        Ok(credential.secret)
    }
}

/// Registration identity survives logout; tokens never leave protected storage/HTTP.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Registration {
    client_id: String,
    subject: String,
    refresh: Option<Secret>,
    id_token: Option<Secret>,
    scopes: Vec<String>,
    earliest_refresh_at: Option<u64>,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGptRegistration(<redacted>)")
    }
}

impl Registration {
    pub(super) fn same_discovery_account(&self, other: &Self) -> bool {
        // Token rotation changes neither registration identity nor granted visibility.
        self.client_id == other.client_id
            && self.subject == other.subject
            && self.scopes == other.scopes
    }
    pub(super) fn plan_enabled(&self) -> bool {
        self.scopes.iter().any(|s| s == DIRECT)
    }
    pub(super) fn parse(value: &Value) -> Result<Self> {
        let text = |key| {
            value[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| failure("invalid stored registration; sign in again"))
        };
        if value["issuer"] != ISSUER {
            return Err(failure("invalid stored issuer; sign in again"));
        }
        let optional = |key| match value.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.is_empty() => Ok(Some(Secret::new(s))),
            _ => Err(failure("invalid stored token metadata")),
        };
        Ok(Self {
            client_id: text("client_id")?.into(),
            subject: text("subject")?.into(),
            refresh: optional("refresh_token")?,
            id_token: optional("id_token")?,
            earliest_refresh_at: value
                .get("earliest_refresh_at")
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| failure("invalid stored renewal time"))
                })
                .transpose()?,
            scopes: value["scopes"]
                .as_array()
                .ok_or_else(|| failure("invalid stored scopes"))?
                .iter()
                .map(|s| {
                    s.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| failure("invalid stored scopes"))
                })
                .collect::<Result<_>>()?,
        })
    }

    pub(super) fn json(&self) -> Value {
        json!({"issuer":ISSUER,"client_id":self.client_id,"subject":self.subject,"refresh_token":self.refresh.as_ref().map(Secret::expose),"id_token":self.id_token.as_ref().map(Secret::expose),"scopes":self.scopes,"earliest_refresh_at":self.earliest_refresh_at})
    }

    pub(super) fn check(&self, credential: &Credential) -> Result<()> {
        if credential.secret.expose().is_empty() {
            return Err(Error::AuthNotLoggedIn {
                profile: "ChatGPT".into(),
            });
        }
        if !self.scopes.iter().any(|s| s == DIRECT) {
            return Err(failure(
                "ChatGPT plan use is disabled; repeat browser login to enable it",
            ));
        }
        Ok(())
    }
}

fn random() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::AuthRandomness)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("https://localhost").expect("literal URL");
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    url.query().unwrap_or_default().to_owned()
}

async fn lock(store: &FileStore) -> Result<std::fs::File> {
    store.lock_async().await
}

fn host(store: &FileStore) -> Result<String> {
    let mut document = store.load()?;
    if let Some(id) = document.host_id {
        return Ok(id);
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::AuthRandomness)?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    );
    document.host_id = Some(id.clone());
    store.save(&document)?;
    Ok(id)
}

/// Prepared turn-owned listener. Its URL intentionally contains no ID-token hint.
pub struct SignIn {
    /// The user-facing authorization URL, containing no OAuth tokens.
    pub url: String,
    listener: tokio::net::TcpListener,
    redirect: String,
    state: Secret,
    nonce: Secret,
    pkce: Pkce,
    previous: Option<Credential>,
    revision: u64,
}

impl SignIn {
    /// Persist host identity before presenting the browser registration.
    pub async fn prepare(store: &FileStore, profile: &str) -> Result<Self> {
        let guard = lock(store).await?;
        let host = host(store)?;
        let previous = store.get(Audience::Provider(profile))?;
        let revision = store.load()?.revision(profile);
        if previous.as_ref().is_some_and(|c| c.oauth.is_none()) {
            return Err(failure(
                "this profile holds an API key; use a separate ChatGPT profile",
            ));
        }
        drop(guard);
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| failure("cannot bind browser callback; check local networking"))?;
        let redirect = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener
                .local_addr()
                .map_err(|_| failure("cannot inspect callback address"))?
                .port()
        );
        let state = Secret::new(random()?);
        let nonce = Secret::new(random()?);
        let pkce = Pkce::generate()?;
        let registration = previous.as_ref().and_then(|c| c.oauth.as_ref());
        let client = registration.map_or("dynamic_agent_client", |r| r.client_id.as_str());
        let mut url = reqwest::Url::parse("https://auth.openai.com/api/accounts/authorize")
            .expect("literal URL");
        url.query_pairs_mut().extend_pairs([
            ("client_id", client),
            ("ext_agent_host_id", &host),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", state.expose()),
            ("nonce", nonce.expose()),
            ("code_challenge_method", "S256"),
            ("code_challenge", pkce.challenge()),
        ]);
        if registration.is_none() {
            url.query_pairs_mut().append_pair("agent_name_hint", "Bee");
        } else if registration.is_some_and(|r| !r.scopes.iter().any(|s| s == DIRECT)) {
            // Consent is requested only for an explicit retry of disabled plan use.
            url.query_pairs_mut().append_pair("prompt", "consent");
        }
        Ok(Self {
            url: url.into(),
            listener,
            redirect,
            state,
            nonce,
            pkce,
            previous,
            revision,
        })
    }

    /// Wait for a bounded callback, exchange and verify identity without committing.
    pub async fn complete(&self, http: &dyn LoginHttp) -> Result<Credential> {
        let (code, client) =
            tokio::time::timeout(crate::login::DEFAULT_TIMEOUT, self.callback_code())
                .await
                .map_err(|_| failure("browser sign-in timed out; retry login"))??;
        let body = form(&[
            ("grant_type", "authorization_code"),
            ("client_id", &client),
            ("code", code.expose()),
            ("code_verifier", self.pkce.verifier().expose()),
            ("redirect_uri", &self.redirect),
            ("resource", RESOURCE),
        ]);
        let response = http
            .post_form(TOKEN, body)
            .await
            .map_err(|_| failure("token exchange unavailable; retry browser login"))?;
        // Identity discovery must not extend the access token's reported lifetime.
        let received_at = unix_now();
        let value = token_response(response)?;
        let metadata = discovery(http).await?;
        let jwks = get_json(http, trusted(&metadata, "jwks_uri")?).await?;
        let subject = verify(&value, &jwks, &client, self.nonce.expose())?;
        if self
            .previous
            .as_ref()
            .and_then(|c| c.oauth.as_ref())
            .is_some_and(|r| r.subject != subject)
        {
            return Err(failure(
                "the signed-in account differs from the selected registration",
            ));
        }
        tokens_at(
            &value,
            Registration {
                client_id: client,
                subject,
                refresh: None,
                id_token: None,
                scopes: Vec::new(),
                earliest_refresh_at: None,
            },
            received_at,
            false,
        )
    }

    async fn callback_code(&self) -> Result<(Secret, String)> {
        loop {
            let (mut stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|_| failure("browser callback failed"))?;
            // An idle or disconnected local socket must not exclude the browser callback.
            let request = tokio::time::timeout(Duration::from_secs(2), async {
                let mut bytes = Vec::new();
                while bytes.len() < 16 * 1024 {
                    bytes.push(stream.read_u8().await.ok()?);
                    if bytes.ends_with(b"\r\n\r\n") {
                        return String::from_utf8(bytes).ok();
                    }
                }
                None
            })
            .await;
            let Ok(Some(request)) = request else {
                continue;
            };
            let Some(target) = request
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("GET "))
                .and_then(|l| l.strip_suffix(" HTTP/1.1"))
            else {
                continue;
            };
            if !target.starts_with("/auth/callback?") {
                continue;
            }
            let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
                continue;
            };
            let states: Vec<_> = url
                .query_pairs()
                .filter(|(key, _)| key == "state")
                .collect();
            if states.len() != 1 || states[0].1 != self.state.expose() {
                continue;
            }
            // Only a callback bound to this attempt can terminate it, including a denial.
            let result = self.callback(target);
            let response = if result.is_ok() {
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nReturn to Bee to finish signing in.".as_slice()
            } else {
                b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nSign-in did not complete. Return to Bee.".as_slice()
            };
            // A browser disconnect cannot invalidate an already validated authorization code.
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(response)).await;
            return result;
        }
    }

    fn callback(&self, target: &str) -> Result<(Secret, String)> {
        // URL parsers accept malformed percent escapes; reject them before decoding.
        let bytes = target.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'%'
                && (i + 2 >= bytes.len()
                    || !bytes[i + 1].is_ascii_hexdigit()
                    || !bytes[i + 2].is_ascii_hexdigit())
            {
                return Err(failure("malformed callback encoding"));
            }
        }
        if !target.starts_with("/auth/callback?") {
            return Err(failure("unexpected callback path"));
        }
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
            .map_err(|_| failure("invalid callback URI"))?;
        let mut params = BTreeMap::new();
        for (k, v) in url.query_pairs() {
            if v.contains('\u{fffd}') || params.insert(k.into_owned(), v.into_owned()).is_some() {
                return Err(failure("ambiguous callback parameters"));
            }
        }
        if params.get("state").map(String::as_str) != Some(self.state.expose()) {
            return Err(failure("callback state did not match; restart login"));
        }
        if params.contains_key("error") {
            return Err(failure("authorization was declined; retry when ready"));
        }
        let previous = self.previous.as_ref().and_then(|c| c.oauth.as_ref());
        let client = match (previous, params.get("client_id")) {
            (Some(r), None) => r.client_id.clone(),
            (Some(r), Some(id)) if id == &r.client_id => id.clone(),
            (None, Some(id)) if !id.is_empty() && id != "dynamic_agent_client" => id.clone(),
            _ => return Err(failure("missing or mismatched issued client identity")),
        };
        let code = params
            .get("code")
            .filter(|c| !c.is_empty())
            .ok_or_else(|| failure("callback has no authorization code"))?;
        Ok((Secret::new(code), client))
    }

    /// Compare-and-commit prevents another login/logout from being overwritten.
    pub async fn commit(
        &self,
        store: &FileStore,
        profile: &str,
        credential: &Credential,
    ) -> Result<()> {
        self.commit_guarded(store, profile, credential, || Some(()))
            .await
    }

    pub(crate) async fn commit_guarded<G>(
        &self,
        store: &FileStore,
        profile: &str,
        credential: &Credential,
        current: impl FnOnce() -> Option<G>,
    ) -> Result<()> {
        let _guard = lock(store).await?;
        // Hold the session gate through the synchronous commit, after async lock acquisition.
        let _current = current().ok_or_else(|| failure("sign-in cancelled"))?;
        let mut document = store.load()?;
        if document.revision(profile) != self.revision
            || document.providers.get(profile) != self.previous.as_ref()
        {
            return Err(failure("selected registration changed during login; retry"));
        }
        document
            .providers
            .insert(profile.into(), credential.clone());
        document.changed(Audience::Provider(profile));
        store.save(&document)
    }
}

async fn get_json(http: &dyn LoginHttp, url: &str) -> Result<Value> {
    let response = http
        .get(url)
        .await
        .map_err(|_| failure("OpenAI identity service unavailable; retry later"))?;
    if response.status != 200 {
        return Err(failure("OpenAI identity service rejected the request"));
    }
    if response.body.len() > crate::http::MAX_EVENT {
        return Err(failure("identity service response too large"));
    }
    serde_json::from_str(&response.body).map_err(|_| failure("malformed identity service response"))
}

async fn discovery(http: &dyn LoginHttp) -> Result<Value> {
    let value = get_json(
        http,
        "https://auth.openai.com/.well-known/openid-configuration",
    )
    .await?;
    if value["issuer"] != ISSUER {
        return Err(failure("unexpected OIDC issuer"));
    }
    Ok(value)
}

fn trusted<'a>(metadata: &'a Value, key: &str) -> Result<&'a str> {
    let text = metadata[key]
        .as_str()
        .ok_or_else(|| failure("OIDC metadata is incomplete"))?;
    let url = reqwest::Url::parse(text).map_err(|_| failure("invalid OIDC endpoint"))?;
    if url.scheme() != "https"
        || url.host_str() != Some("auth.openai.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(failure("untrusted OIDC endpoint"));
    }
    Ok(text)
}

fn verify(value: &Value, jwks: &Value, client: &str, nonce: &str) -> Result<String> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
    let failed = || failure("ID token verification failed; restart login");
    let token = value["id_token"].as_str().ok_or_else(failed)?;
    let header = decode_header(token).map_err(|_| failed())?;
    // Never let a token select a symmetric algorithm or untrusted key material.
    if header.alg != Algorithm::RS256 {
        return Err(failed());
    }
    let keys: JwkSet = serde_json::from_value(jwks.clone()).map_err(|_| failed())?;
    let key = keys
        .find(header.kid.as_deref().ok_or_else(failed)?)
        .ok_or_else(failed)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[client]);
    validation.set_issuer(&[ISSUER]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 0;
    let claims = decode::<Value>(
        token,
        &DecodingKey::from_jwk(key).map_err(|_| failed())?,
        &validation,
    )
    .map_err(|_| failed())?
    .claims;
    if claims["nonce"] != nonce {
        return Err(failed());
    }
    // OIDC requires an authorized-party check when the ID token has multiple audiences.
    if claims.get("azp").is_some_and(|party| party != client)
        || (claims["aud"].as_array().is_some_and(|aud| aud.len() > 1) && claims["azp"] != client)
    {
        return Err(failed());
    }
    claims["sub"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(failed)
}

fn token_response(response: crate::http::HttpResponse) -> Result<Value> {
    if response.body.len() > crate::http::MAX_EVENT {
        return Err(failure("token response too large"));
    }
    let value: Value =
        serde_json::from_str(&response.body).map_err(|_| failure("malformed token response"))?;
    if response.status != 200 {
        return Err(failure(
            "token request rejected; sign in again or retry later",
        ));
    }
    Ok(value)
}

#[cfg(test)]
fn tokens(value: &Value, registration: Registration) -> Result<Credential> {
    tokens_at(value, registration, unix_now(), false)
}

fn tokens_at(
    value: &Value,
    mut registration: Registration,
    received_at: u64,
    rotating: bool,
) -> Result<Credential> {
    let text = |key| {
        value[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| failure("incomplete token response"))
    };
    if !text("token_type")?.eq_ignore_ascii_case("Bearer") {
        return Err(failure("unsupported OAuth token type"));
    }
    let access = Secret::new(text("access_token")?);
    registration.earliest_refresh_at = value
        .get("earliest_refresh_at")
        .filter(|v| !v.is_null())
        .map(|v| v.as_u64().ok_or_else(|| failure("invalid renewal time")))
        .transpose()?;
    if let Some(id) = value.get("id_token") {
        registration.id_token = Some(Secret::new(
            id.as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| failure("invalid ID token"))?,
        ));
    }
    if let Some(scope) = value.get("scope") {
        registration.scopes = scope
            .as_str()
            .ok_or_else(|| failure("invalid granted scopes"))?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
    }
    registration.refresh = match value.get("refresh_token") {
        Some(Value::String(token)) if !token.is_empty() => Some(Secret::new(token)),
        // Offline access is a separate grant; a verified identity remains useful without it.
        None if !rotating
            && !registration
                .scopes
                .iter()
                .any(|scope| scope == "offline_access") =>
        {
            None
        }
        _ => return Err(failure("incomplete rotating token response")),
    };
    let seconds = value["expires_in"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| failure("missing token expiry"))?;
    Ok(Credential {
        secret: access,
        expires_at: Some(received_at.saturating_add(seconds)),
        oauth: Some(registration),
    })
}

/// Renew at the request boundary under the same lock used by every store writer.
pub async fn resolve(store: &FileStore, profile: &str, http: &dyn LoginHttp) -> Result<Credential> {
    let _guard = lock(store).await?;
    let mut document = store.load()?;
    let credential = document
        .providers
        .get(profile)
        .ok_or_else(|| Error::AuthNotLoggedIn {
            profile: profile.into(),
        })?;
    let registration = credential
        .oauth
        .as_ref()
        .ok_or_else(|| failure("this profile requires ChatGPT browser sign-in"))?;
    registration.check(credential)?;
    if !credential.is_expired(unix_now().saturating_add(30)) {
        return Ok(credential.clone());
    }
    if registration
        .earliest_refresh_at
        .is_some_and(|at| at > unix_now())
    {
        if credential.is_expired(unix_now()) {
            return Err(failure("token renewal is not yet allowed; retry later"));
        }
        return Ok(credential.clone());
    }
    let refresh = registration
        .refresh
        .as_ref()
        .ok_or_else(|| Error::AuthExpired {
            profile: profile.into(),
        })?;
    let body = form(&[
        ("grant_type", "refresh_token"),
        ("client_id", &registration.client_id),
        ("refresh_token", refresh.expose()),
        ("resource", RESOURCE),
    ]);
    let response = http
        .post_form(TOKEN, body)
        .await
        .map_err(|_| failure("renewal unavailable; retry later"))?;
    let received_at = unix_now();
    if response.status != 200 {
        let value: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        if matches!(
            value["error"].as_str(),
            Some(
                "invalid_grant"
                    | "invalid_refresh_token"
                    | "token_expired"
                    | "refresh_token_expired"
                    | "refresh_token_invalidated"
                    | "refresh_token_reused"
            )
        ) {
            clear(
                document
                    .providers
                    .get_mut(profile)
                    .expect("existing registration"),
            );
            document.changed(Audience::Provider(profile));
            store.save(&document)?;
            return Err(Error::AuthExpired {
                profile: profile.into(),
            });
        }
        return Err(failure(if value["error"] == "invalid_client" {
            "invalid OAuth client; check registration configuration"
        } else {
            "renewal rejected or temporarily unavailable; retry later"
        }));
    }
    let renewed = tokens_at(
        &token_response(response)?,
        registration.clone(),
        received_at,
        true,
    )?;
    document.providers.insert(profile.into(), renewed.clone());
    document.changed(Audience::Provider(profile));
    // No await between receiving replacement tokens and committing rotation.
    store.save(&document)?;
    renewed
        .oauth
        .as_ref()
        .expect("OAuth renewal")
        .check(&renewed)?;
    Ok(renewed)
}

fn clear(credential: &mut Credential) {
    credential.secret = Secret::new("");
    credential.expires_at = None;
    if let Some(r) = &mut credential.oauth {
        r.refresh = None;
        r.id_token = None;
        r.earliest_refresh_at = None;
    }
}

/// Returns whether remote revocation was confirmed; local logout always clears tokens.
pub async fn logout(store: &FileStore, profile: &str, http: &dyn LoginHttp) -> Result<bool> {
    let _guard = lock(store).await?;
    let mut document = store.load()?;
    let Some(credential) = document.providers.get_mut(profile) else {
        // Even an empty logout invalidates any initial browser attempt already open.
        document.changed(Audience::Provider(profile));
        store.save(&document)?;
        return Ok(true);
    };
    let r = credential
        .oauth
        .as_ref()
        .ok_or_else(|| failure("this is not a ChatGPT registration"))?;
    let mut confirmed = r.refresh.is_none();
    if let Some(refresh) = &r.refresh
        && let Ok(metadata) = discovery(http).await
        && let Ok(endpoint) = trusted(&metadata, "revocation_endpoint")
    {
        let body = form(&[
            ("token", refresh.expose()),
            ("token_type_hint", "refresh_token"),
            ("client_id", &r.client_id),
        ]);
        // Bounded retry is safe for revocation, unlike rotating refresh.
        for attempt in 0..2 {
            if let Ok(response) = http.post_form(endpoint, body.clone()).await {
                if response.status == 200 {
                    confirmed = true;
                    break;
                }
                if response.status < 500 {
                    break;
                }
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    clear(credential);
    document.changed(Audience::Provider(profile));
    store.save(&document)?;
    Ok(confirmed)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::http::{HttpResponse, Unreachable};
    use futures::future::BoxFuture;
    use rsa::{pkcs1::DecodeRsaPrivateKey, traits::PublicKeyParts};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    const KEY: &str = include_str!("../../tests/fixtures/oidc-test-key.pem");
    const SENTINEL: &str = "secret-token-must-not-escape";

    fn signed(mut claims: Value) -> (String, Value) {
        let private = rsa::RsaPrivateKey::from_pkcs1_pem(KEY).unwrap();
        let jwks = json!({"keys":[{"kty":"RSA","kid":"test","use":"sig","alg":"RS256","n":URL_SAFE_NO_PAD.encode(private.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(private.e().to_bytes_be())}]});
        if claims.get("exp").is_none() {
            claims["exp"] = (unix_now() + 3600).into();
        }
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("test".into());
        (
            jsonwebtoken::encode(
                &header,
                &claims,
                &jsonwebtoken::EncodingKey::from_rsa_pem(KEY.as_bytes()).unwrap(),
            )
            .unwrap(),
            jwks,
        )
    }

    fn claims(nonce: &str) -> Value {
        json!({"iss":ISSUER,"aud":"issued-client","sub":"account","nonce":nonce})
    }

    fn registration() -> Registration {
        Registration {
            client_id: "issued-client".into(),
            subject: "account".into(),
            refresh: None,
            id_token: None,
            scopes: vec![DIRECT.into()],
            earliest_refresh_at: None,
        }
    }

    fn token_json(id: &str) -> Value {
        json!({"access_token":SENTINEL,"refresh_token":"rotating-refresh-secret","id_token":id,"scope":SCOPES,"token_type":"Bearer","expires_in":3600})
    }

    #[test]
    fn identity_requires_a_valid_signature_issuer_audience_expiry_and_nonce() {
        let (token, jwks) = signed(claims("nonce"));
        assert_eq!(
            verify(&json!({"id_token":token}), &jwks, "issued-client", "nonce").unwrap(),
            "account"
        );
        for (key, value) in [
            ("iss", json!("https://untrusted.example")),
            ("aud", json!("another-client")),
            ("nonce", json!("other")),
            ("exp", json!(1)),
        ] {
            let mut bad = claims("nonce");
            bad[key] = value;
            let (token, _) = signed(bad);
            assert!(
                verify(&json!({"id_token":token}), &jwks, "issued-client", "nonce").is_err(),
                "{key}"
            );
        }
        let mut bytes = token.into_bytes();
        let last = bytes.len() - 5;
        bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
        assert!(
            verify(
                &json!({"id_token":String::from_utf8(bytes).unwrap()}),
                &jwks,
                "issued-client",
                "nonce"
            )
            .is_err()
        );
    }

    struct Fake {
        response: Mutex<(u16, Value)>,
        jwks: Value,
        forms: Mutex<Vec<String>>,
        calls: AtomicUsize,
        record: Option<std::path::PathBuf>,
    }

    impl Fake {
        fn new(response: Value, jwks: Value) -> Self {
            Self {
                response: Mutex::new((200, response)),
                jwks,
                forms: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
                record: None,
            }
        }
    }

    impl LoginHttp for Fake {
        fn post_json<'a>(
            &'a self,
            _: &'a str,
            _: String,
        ) -> BoxFuture<'a, std::result::Result<HttpResponse, Unreachable>> {
            Box::pin(async { panic!("OAuth must not use JSON exchange") })
        }
        fn post_form<'a>(
            &'a self,
            _: &'a str,
            body: String,
        ) -> BoxFuture<'a, std::result::Result<HttpResponse, Unreachable>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some(path) = &self.record {
                    // A process-shared counter verifies that OS locking covers token rotation.
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .unwrap();
                    std::io::Write::write_all(&mut file, b"x").unwrap();
                }
                self.forms.lock().unwrap().push(body);
                tokio::task::yield_now().await;
                let response = self.response.lock().unwrap();
                Ok(HttpResponse {
                    status: response.0,
                    body: response.1.to_string(),
                })
            })
        }
        fn get<'a>(
            &'a self,
            url: &'a str,
        ) -> BoxFuture<'a, std::result::Result<HttpResponse, Unreachable>> {
            Box::pin(async move {
                Ok(HttpResponse { status:200, body:if url.ends_with("openid-configuration") { json!({"issuer":ISSUER,"jwks_uri":"https://auth.openai.com/keys","revocation_endpoint":"https://auth.openai.com/revoke"}) } else { self.jwks.clone() }.to_string() })
            })
        }
    }

    fn store() -> (tempfile::TempDir, FileStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("credentials.json"));
        (dir, store)
    }

    fn install(store: &FileStore, expired: bool) {
        let mut credential = tokens(&token_json("retained-id-token"), registration()).unwrap();
        if expired {
            credential.expires_at = Some(1);
        }
        store
            .put(Audience::Provider("account"), &credential)
            .unwrap();
    }

    #[tokio::test]
    async fn callbacks_validate_state_before_denial_and_bind_issued_clients() {
        let (_dir, store) = store();
        let login = SignIn::prepare(&store, "account").await.unwrap();
        let state = login.state.expose();
        assert!(
            login
                .callback("/auth/callback?state=wrong&error=access_denied")
                .unwrap_err()
                .to_string()
                .contains("state")
        );
        for query in [
            format!("state={state}&code=x"),
            format!("state={state}&code=x&client_id=dynamic_agent_client"),
            format!("state={state}&state={state}&code=x&client_id=issued-client"),
            format!("state={state}&code=%QQ&client_id=issued-client"),
        ] {
            assert!(login.callback(&format!("/auth/callback?{query}")).is_err());
        }
        assert_eq!(
            login
                .callback(&format!(
                    "/auth/callback?state={state}&code={SENTINEL}&client_id=issued-client"
                ))
                .unwrap()
                .1,
            "issued-client"
        );
        install(&store, false);
        let returning = SignIn::prepare(&store, "account").await.unwrap();
        assert!(
            returning
                .callback(&format!(
                    "/auth/callback?state={}&code=x&client_id=other",
                    returning.state.expose()
                ))
                .is_err()
        );
        assert!(
            returning
                .callback(&format!(
                    "/auth/callback?state={}&code=x",
                    returning.state.expose()
                ))
                .is_ok()
        );
        assert!(!returning.url.contains("id_token"));
        assert!(!returning.url.contains(SENTINEL));
    }

    #[tokio::test]
    async fn idle_disconnected_and_unrelated_connections_do_not_exclude_the_browser() {
        for unrelated in ["idle", "eof", "unrelated"] {
            let (_dir, store) = store();
            let login = SignIn::prepare(&store, "account").await.unwrap();
            let address = login.listener.local_addr().unwrap();
            let mut first = Some(tokio::net::TcpStream::connect(address).await.unwrap());
            if unrelated == "eof" {
                first.as_mut().unwrap().write_all(b"GET ").await.unwrap();
                drop(first.take());
            } else if unrelated == "unrelated" {
                first
                    .as_mut()
                    .unwrap()
                    .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap();
            }
            let callback = login.callback_code();
            tokio::pin!(callback);
            // Poll the accepted connection before advancing its per-connection deadline.
            tokio::select! {
                biased;
                _ = &mut callback => panic!("an unrelated connection terminated login"),
                _ = tokio::task::yield_now() => {}
            }
            if unrelated == "idle" {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(3)).await;
                tokio::time::resume();
            }
            let browser = async {
                let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
                socket.write_all(format!("GET /auth/callback?state={}&code={SENTINEL}&client_id=issued-client HTTP/1.1\r\nHost: localhost\r\n\r\n",login.state.expose()).as_bytes()).await.unwrap();
                let mut response = Vec::new();
                socket.read_to_end(&mut response).await.unwrap();
                assert!(response.starts_with(b"HTTP/1.1 200"));
            };
            let (result, ()) = tokio::join!(callback, browser);
            let (code, client) = result.unwrap();
            assert_eq!(code.expose(), SENTINEL);
            assert_eq!(client, "issued-client");
        }
    }

    #[tokio::test]
    async fn browser_login_exchanges_pkce_and_reuses_registration_across_restarts() {
        let (_dir, store) = store();
        let login = SignIn::prepare(&store, "account").await.unwrap();
        let (token, jwks) = signed(claims(login.nonce.expose()));
        let fake = Fake::new(token_json(&token), jwks);
        let browser = async {
            let mut socket = tokio::net::TcpStream::connect(login.listener.local_addr().unwrap())
                .await
                .unwrap();
            socket.write_all(format!("GET /auth/callback?state={}&code={SENTINEL}&client_id=issued-client HTTP/1.1\r\nHost: localhost\r\n\r\n",login.state.expose()).as_bytes()).await.unwrap();
            let mut response = Vec::new();
            socket.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200"));
        };
        let (credential, ()) = tokio::join!(login.complete(&fake), browser);
        let credential = credential.unwrap();
        login.commit(&store, "account", &credential).await.unwrap();
        {
            let forms = fake.forms.lock().unwrap();
            assert!(forms[0].contains("grant_type=authorization_code"));
            assert!(
                forms[0].contains(&format!("code_verifier={}", login.pkce.verifier().expose()))
            );
        }
        let reopened = FileStore::new(store.path());
        let next = SignIn::prepare(&reopened, "account").await.unwrap();
        assert!(next.url.contains("client_id=issued-client"));
        let query = |url: &str| {
            reqwest::Url::parse(url)
                .unwrap()
                .query_pairs()
                .find(|(k, _)| k == "ext_agent_host_id")
                .unwrap()
                .1
                .into_owned()
        };
        assert_eq!(query(&login.url), query(&next.url));
        assert_eq!(
            reopened.get(Audience::Provider("account")).unwrap(),
            Some(credential.clone())
        );
        assert!(!format!("{credential:?}").contains(SENTINEL));
    }

    #[tokio::test]
    async fn exchange_and_identity_failures_leave_a_previous_sign_in_untouched() {
        let (_dir, store) = store();
        install(&store, false);
        let previous = store.get(Audience::Provider("account")).unwrap();
        for failure_stage in ["exchange", "subject", "keys"] {
            let login = SignIn::prepare(&store, "account").await.unwrap();
            let mut identity = claims(login.nonce.expose());
            if failure_stage == "subject" {
                identity["sub"] = "different-account".into();
            }
            let (token, mut keys) = signed(identity);
            if failure_stage == "keys" {
                keys = json!({"keys":[]});
            }
            let fake = Fake::new(token_json(&token), keys);
            if failure_stage == "exchange" {
                *fake.response.lock().unwrap() = (
                    400,
                    json!({"error":"invalid_grant","error_description":SENTINEL}),
                );
            }
            let browser = async {
                let mut socket =
                    tokio::net::TcpStream::connect(login.listener.local_addr().unwrap())
                        .await
                        .unwrap();
                socket.write_all(format!("GET /auth/callback?state={}&code={SENTINEL} HTTP/1.1\r\nHost: localhost\r\n\r\n",login.state.expose()).as_bytes()).await.unwrap();
                let mut response = Vec::new();
                socket.read_to_end(&mut response).await.unwrap();
            };
            let (result, ()) = tokio::join!(login.complete(&fake), browser);
            let error = result.unwrap_err();
            assert!(!format!("{error:?} {error}").contains(SENTINEL));
            assert_eq!(store.get(Audience::Provider("account")).unwrap(), previous);
        }
    }

    #[tokio::test]
    async fn concurrent_refresh_rotates_once_and_preserves_unrelated_credentials() {
        let (_dir, store) = store();
        install(&store, true);
        let fake = Fake::new(token_json("new-id"), Value::Null);
        let (a, b) = tokio::join!(
            resolve(&store, "account", &fake),
            resolve(&store, "account", &fake)
        );
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        store
            .put(
                Audience::Provider("key"),
                &Credential::new(Secret::new("unrelated")),
            )
            .unwrap();
        let persisted = FileStore::new(store.path())
            .get(Audience::Provider("account"))
            .unwrap()
            .unwrap();
        assert_eq!(persisted.secret().expose(), SENTINEL);
        assert!(fake.forms.lock().unwrap()[0].contains("grant_type=refresh_token"));
        assert!(!fake.forms.lock().unwrap()[0].contains("scope="));
    }

    #[tokio::test]
    async fn a_turn_session_renews_between_requests_and_stops_after_logout() {
        let (_dir, store) = store();
        install(&store, false);
        let credential = store.get(Audience::Provider("account")).unwrap().unwrap();
        let fake = std::sync::Arc::new(Fake::new(token_json("new-id"), Value::Null));
        let session =
            Session::new(store.clone(), "account".into(), &credential, fake.clone()).unwrap();
        session.credential().await.unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        install(&store, true);
        session.credential().await.unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        logout(&store, "account", fake.as_ref()).await.unwrap();
        assert!(session.credential().await.is_err());
    }

    #[tokio::test]
    async fn a_child_refreshes_the_shared_registration() {
        let Some(path) = std::env::var_os("BEE_OAUTH_TEST_STORE") else {
            return;
        };
        let mut fake = Fake::new(token_json("new-id"), Value::Null);
        fake.record = std::env::var_os("BEE_OAUTH_TEST_MARKER").map(Into::into);
        resolve(&FileStore::new(path), "account", &fake)
            .await
            .unwrap();
    }

    #[test]
    fn separate_processes_cannot_race_a_rotating_refresh_token() {
        let (dir, store) = store();
        install(&store, true);
        let marker = dir.path().join("refresh-count");
        let spawn = || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "auth::chatgpt::tests::a_child_refreshes_the_shared_registration",
                ])
                .env("BEE_OAUTH_TEST_STORE", store.path())
                .env("BEE_OAUTH_TEST_MARKER", &marker)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let mut second = spawn();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        assert_eq!(std::fs::read(marker).unwrap(), b"x");
    }

    #[tokio::test]
    async fn earliest_refresh_time_defers_proactive_renewal_without_using_expired_access() {
        let (_dir, store) = store();
        let mut value = token_json("id");
        value["expires_in"] = 20.into();
        value["earliest_refresh_at"] = (unix_now() + 60).into();
        let mut credential = tokens(&value, registration()).unwrap();
        store
            .put(Audience::Provider("account"), &credential)
            .unwrap();
        let fake = Fake::new(Value::Null, Value::Null);
        resolve(&store, "account", &fake).await.unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        credential.expires_at = Some(1);
        store
            .put(Audience::Provider("account"), &credential)
            .unwrap();
        assert!(resolve(&store, "account", &fake).await.is_err());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_remote_revocation_still_clears_local_tokens() {
        let (_dir, store) = store();
        install(&store, false);
        let fake = Fake::new(Value::Null, Value::Null);
        *fake.response.lock().unwrap() = (503, json!({"error_description":SENTINEL}));
        assert!(!logout(&store, "account", &fake).await.unwrap());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
        assert!(
            store
                .get(Audience::Provider("account"))
                .unwrap()
                .unwrap()
                .secret
                .expose()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn terminal_refresh_errors_clear_tokens_but_temporary_errors_preserve_them() {
        let (_dir, store) = store();
        install(&store, true);
        let before = store.get(Audience::Provider("account")).unwrap();
        let fake = Fake::new(Value::Null, Value::Null);
        *fake.response.lock().unwrap() = (503, json!({"error":SENTINEL}));
        let error = resolve(&store, "account", &fake).await.unwrap_err();
        assert!(!format!("{error:?} {error}").contains(SENTINEL));
        assert_eq!(store.get(Audience::Provider("account")).unwrap(), before);
        *fake.response.lock().unwrap() = (
            400,
            json!({"error":"invalid_grant","error_description":SENTINEL}),
        );
        assert!(matches!(
            resolve(&store, "account", &fake).await,
            Err(Error::AuthExpired { .. })
        ));
        let record = store.get(Audience::Provider("account")).unwrap().unwrap();
        assert!(record.secret.expose().is_empty());
        assert_eq!(record.oauth.unwrap().client_id, "issued-client");
    }

    #[tokio::test]
    async fn logout_revokes_and_retains_registration_while_invalidating_pending_login() {
        let (_dir, store) = store();
        install(&store, false);
        let login = SignIn::prepare(&store, "account").await.unwrap();
        let credential = store.get(Audience::Provider("account")).unwrap().unwrap();
        let fake = Fake::new(Value::Null, Value::Null);
        assert!(logout(&store, "account", &fake).await.unwrap());
        assert!(fake.forms.lock().unwrap()[0].contains("token_type_hint=refresh_token"));
        assert!(login.commit(&store, "account", &credential).await.is_err());
        let next = SignIn::prepare(&store, "account").await.unwrap();
        assert!(next.url.contains("client_id=issued-client"));
        assert!(matches!(
            resolve(&store, "account", &fake).await,
            Err(Error::AuthNotLoggedIn { .. })
        ));
    }

    #[tokio::test]
    async fn cancelled_commit_and_plan_disabled_grants_cannot_enable_inference() {
        let (_dir, store) = store();
        let login = SignIn::prepare(&store, "account").await.unwrap();
        let mut value = token_json("id");
        value["scope"] = "openid profile".into();
        value.as_object_mut().unwrap().remove("refresh_token");
        let credential = tokens(&value, registration()).unwrap();
        assert!(credential.oauth.as_ref().unwrap().refresh.is_none());
        assert_eq!(
            store_state(&credential, unix_now()),
            super::super::StoreState::PlanDisabled
        );
        // An actual refresh still requires its replacement token, regardless of granted scopes.
        assert!(tokens_at(&value, registration(), unix_now(), true).is_err());
        assert!(
            login
                .commit_guarded(&store, "account", &credential, || None::<()>)
                .await
                .is_err()
        );
        assert!(store.get(Audience::Provider("account")).unwrap().is_none());
        login.commit(&store, "account", &credential).await.unwrap();
        let fake = Fake::new(Value::Null, Value::Null);
        assert!(
            resolve(&store, "account", &fake)
                .await
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn identity_verification_delay_does_not_extend_access_token_expiry() {
        let received_at = unix_now().saturating_sub(120);
        let credential = tokens_at(&token_json("id"), registration(), received_at, false).unwrap();
        assert_eq!(credential.expires_at(), Some(received_at + 3600));
        assert!(credential.is_expired(received_at + 3600));
    }

    #[tokio::test]
    async fn independent_profile_logins_can_commit_without_overwriting_each_other() {
        let (_dir, store) = store();
        let first = SignIn::prepare(&store, "first").await.unwrap();
        let second = SignIn::prepare(&store, "second").await.unwrap();
        let credential = tokens(&token_json("id"), registration()).unwrap();
        first.commit(&store, "first", &credential).await.unwrap();
        second.commit(&store, "second", &credential).await.unwrap();
        assert_eq!(
            store.get(Audience::Provider("first")).unwrap(),
            Some(credential.clone())
        );
        assert_eq!(
            store.get(Audience::Provider("second")).unwrap(),
            Some(credential)
        );
    }

    #[tokio::test]
    async fn logout_during_initial_registration_prevents_a_late_first_login_commit() {
        let (_dir, store) = store();
        let login = SignIn::prepare(&store, "account").await.unwrap();
        let credential = tokens(&token_json("id"), registration()).unwrap();
        let fake = Fake::new(Value::Null, Value::Null);
        logout(&store, "account", &fake).await.unwrap();
        assert!(login.commit(&store, "account", &credential).await.is_err());
        assert!(store.get(Audience::Provider("account")).unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelled_lock_wait_has_no_late_commit() {
        let (_dir, store) = store();
        let login = SignIn::prepare(&store, "account").await.unwrap();
        let guard = store.try_lock().unwrap().unwrap();
        let credential = tokens(&token_json("id"), registration()).unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                login.commit(&store, "account", &credential)
            )
            .await
            .is_err()
        );
        drop(guard);
        tokio::task::yield_now().await;
        assert!(store.get(Audience::Provider("account")).unwrap().is_none());
    }

    #[test]
    fn untrusted_discovery_endpoints_and_unverified_algorithms_are_rejected() {
        for endpoint in [
            "http://auth.openai.com/keys",
            "https://evil.example/keys",
            "https://user@auth.openai.com/keys",
        ] {
            assert!(trusted(&json!({"jwks_uri":endpoint}), "jwks_uri").is_err());
        }
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims("nonce"),
            &jsonwebtoken::EncodingKey::from_secret(b"test"),
        )
        .unwrap();
        assert!(
            verify(
                &json!({"id_token":token}),
                &json!({"keys":[]}),
                "issued-client",
                "nonce"
            )
            .is_err()
        );
    }
}
