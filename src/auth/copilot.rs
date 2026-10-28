//! GitHub.com device authorization and direct Copilot user-token requests.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::{Audience, Credential, FileStore, Secret};
use crate::error::{Error, Result};
use crate::http::LoginHttp;
use crate::provider::{CredentialSource, Profile};

/// Existing public client used by OpenCode, not a Bee-owned app registration.
pub const CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
/// Safe default when account endpoint discovery is temporarily unavailable.
pub const DEFAULT_TARGET: &str = "https://api.githubcopilot.com";
const DEVICE: &str = "https://github.com/login/device/code";
const TOKEN: &str = "https://github.com/login/oauth/access_token";
const USER: &str = "https://api.github.com/copilot_internal/user";
const REJECTED: &str = "GitHub rejected authorization; replace the token or repeat login";
const NO_CHAT: &str =
    "this account has no Copilot chat access; activate Copilot or request an organization seat";

pub(crate) fn unauthorized(error: &Error) -> bool {
    matches!(error, Error::Copilot { reason } if matches!(*reason, REJECTED | NO_CHAT))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::auth::CredentialStore;
    use crate::http::{HttpResponse, Unreachable};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Device {
        replies: Mutex<VecDeque<Value>>,
        polls: Mutex<Vec<tokio::time::Instant>>,
        account: Mutex<(u16, Value)>,
    }

    impl Device {
        fn new(replies: Vec<Value>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                polls: Mutex::new(Vec::new()),
                account: Mutex::new((
                    200,
                    json!({"chat_enabled":true,"endpoints":{"api":"https://api.business.githubcopilot.com"}}),
                )),
            }
        }
    }

    impl LoginHttp for Device {
        fn post_json<'a>(
            &'a self,
            url: &'a str,
            body: String,
        ) -> futures::future::BoxFuture<'a, std::result::Result<HttpResponse, Unreachable>>
        {
            Box::pin(async move {
                let body: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["client_id"], CLIENT_ID);
                let value = if url == DEVICE {
                    assert_eq!(body["scope"], "read:user");
                    json!({"verification_uri":"https://github.com/login/device","device_code":"fixture-device-secret","user_code":"ABCD-EFGH","expires_in":900,"interval":5})
                } else {
                    assert_eq!(url, TOKEN);
                    assert_eq!(body["device_code"], "fixture-device-secret");
                    assert_eq!(
                        body["grant_type"],
                        "urn:ietf:params:oauth:grant-type:device_code"
                    );
                    self.polls.lock().unwrap().push(tokio::time::Instant::now());
                    self.replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(json!({"error":"authorization_pending"}))
                };
                Ok(HttpResponse {
                    status: 200,
                    body: value.to_string(),
                })
            })
        }
        fn get_authenticated<'a>(
            &'a self,
            url: &'a str,
            headers: HeaderMap,
        ) -> futures::future::BoxFuture<'a, std::result::Result<HttpResponse, Unreachable>>
        {
            assert_eq!(url, USER);
            assert_eq!(headers["x-github-api-version"], "2025-04-01");
            assert!(headers["authorization"].is_sensitive());
            let (status, value) = self.account.lock().unwrap().clone();
            Box::pin(async move {
                Ok(HttpResponse {
                    status,
                    body: value.to_string(),
                })
            })
        }
    }

    #[test]
    fn only_exact_copilot_https_api_roots_can_receive_credentials() {
        for host in [
            "api.githubcopilot.com",
            "api.individual.githubcopilot.com",
            "api.business.githubcopilot.com",
            "api.enterprise.githubcopilot.com",
        ] {
            assert_eq!(
                validate_target(&format!("https://{host}/")).unwrap(),
                format!("https://{host}")
            );
        }
        for value in [
            "http://api.githubcopilot.com",
            "https://api.githubcopilot.com.evil.example",
            "https://evilgithubcopilot.com",
            "https://user:password@api.githubcopilot.com",
            "https://api.githubcopilot.com:444",
            "https://api.githubcopilot.com/v1",
            "https://api.githubcopilot.com?secret=value",
            "https://api.githubcopilot.com#fragment",
            " https://api.githubcopilot.com",
            "https://api.githubcopilot.com\n",
        ] {
            assert!(validate_target(value).is_err(), "{value}");
        }
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn device_polling_respects_pending_slow_down_and_the_first_interval() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path().join("credentials.json"));
        let http = Device::new(vec![
            json!({"error":"authorization_pending"}),
            json!({"error":"slow_down","interval":12}),
            json!({"access_token":"fixture-github-secret","token_type":"bearer"}),
        ]);
        let start = tokio::time::Instant::now();
        let login = SignIn::prepare(&store, "copilot", &http).await.unwrap();
        let credential = login.complete(&http).await.unwrap();
        let polls = http.polls.lock().unwrap().clone();
        assert_eq!(polls[0] - start, Duration::from_secs(5));
        assert_eq!(polls[1] - polls[0], Duration::from_secs(5));
        assert_eq!(polls[2] - polls[1], Duration::from_secs(12));
        assert!(store.get(Audience::Provider("copilot")).unwrap().is_none());
        login.commit(&store, "copilot", &credential).await.unwrap();
        assert_eq!(
            store.get(Audience::Provider("copilot")).unwrap(),
            Some(credential)
        );
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn denial_expiration_and_unsupported_renewal_leave_prior_authority_untouched() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path().join("credentials.json"));
        let previous = Credential::new(Secret::new("previous-secret"));
        store.put(Audience::Provider("copilot"), &previous).unwrap();
        for reply in [
            json!({"error":"access_denied","error_description":"fixture-leaked-secret"}),
            json!({"error":"expired_token"}),
            json!({"access_token":"fixture-leaked-secret","expires_in":3600,"refresh_token":"fixture-refresh-secret"}),
            json!({"invalid":"fixture-leaked-secret"}),
        ] {
            let http = Device::new(vec![reply]);
            let login = SignIn::prepare(&store, "copilot", &http).await.unwrap();
            let error = login.complete(&http).await.unwrap_err();
            assert!(!format!("{error:?} {error}").contains("fixture-leaked-secret"));
            assert!(!format!("{error:?} {error}").contains("fixture-refresh-secret"));
            assert_eq!(
                store.get(Audience::Provider("copilot")).unwrap(),
                Some(previous.clone())
            );
        }
        let http = Device::new(vec![]);
        let mut login = SignIn::prepare(&store, "copilot", &http).await.unwrap();
        login.deadline = tokio::time::Instant::now() + Duration::from_secs(7);
        assert!(login.complete(&http).await.is_err());
        assert_eq!(http.polls.lock().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn logout_replacement_and_cancelled_turns_prevent_late_device_commits() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path().join("credentials.json"));
        let http = Device::new(vec![]);
        let credential = Credential::new(Secret::new("late-secret"));
        let login = SignIn::prepare(&store, "copilot", &http).await.unwrap();
        assert!(!store.remove(Audience::Provider("copilot")).unwrap());
        assert!(login.commit(&store, "copilot", &credential).await.is_err());
        let login = SignIn::prepare(&store, "copilot", &http).await.unwrap();
        let newer = Credential::new(Secret::new("newer-secret"));
        store.put(Audience::Provider("copilot"), &newer).unwrap();
        assert!(login.commit(&store, "copilot", &credential).await.is_err());
        let login = SignIn::prepare(&store, "copilot", &http).await.unwrap();
        assert!(
            login
                .commit_guarded(&store, "copilot", &credential, || None::<()>)
                .await
                .is_err()
        );
        assert_eq!(
            store.get(Audience::Provider("copilot")).unwrap(),
            Some(newer)
        );
        assert!(store.load().unwrap().host_id.is_none());
    }

    #[tokio::test]
    async fn account_denial_and_unsafe_targets_fail_while_missing_metadata_uses_a_safe_default() {
        let http = Device::new(vec![]);
        let secret = Secret::new("fixture-github-secret");
        for account in [
            (403, json!({"secret":"never-render"})),
            (200, json!({"chat_enabled":false})),
        ] {
            *http.account.lock().unwrap() = account;
            assert!(unauthorized(&target(&secret, &http).await.unwrap_err()));
        }
        *http.account.lock().unwrap() = (
            200,
            json!({"endpoints":{"api":"https://evil.example/private-secret"}}),
        );
        let error = target(&secret, &http).await.unwrap_err();
        assert!(!error.to_string().contains("private-secret"));
        *http.account.lock().unwrap() = (503, json!({}));
        assert_eq!(target(&secret, &http).await.unwrap(), DEFAULT_TARGET);
        *http.account.lock().unwrap() = (200, json!({}));
        assert_eq!(target(&secret, &http).await.unwrap(), DEFAULT_TARGET);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_turn_rejects_logout_even_if_the_same_token_is_reinstalled() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path().join("credentials.json"));
        let profile =
            crate::test_support::configured_providers("[providers.copilot]\ntype='github'")
                .get("copilot")
                .unwrap()
                .clone();
        let credential = Credential::new(Secret::new("fixture-github-secret"));
        store
            .put(Audience::Provider("copilot"), &credential)
            .unwrap();
        let session = Session::prepare(
            &profile,
            credential.clone(),
            Some(store.clone()),
            "opaque-interaction".into(),
            &Device::new(vec![]),
        )
        .await
        .unwrap();
        store.remove(Audience::Provider("copilot")).unwrap();
        store
            .put(Audience::Provider("copilot"), &credential)
            .unwrap();
        assert!(session.check(&profile).await.is_err());
    }

    #[tokio::test]
    async fn environment_authentication_needs_no_private_store_or_platform_specific_backend() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path().join("unused-credentials.json"));
        let profile = crate::test_support::configured_providers(
            "[providers.github]\ntype='github'\ncredential={env='TOKEN'}",
        )
        .get("github")
        .unwrap()
        .clone();
        let env =
            crate::config::Environment::default().with_variable("TOKEN", "environment-secret");
        let credential = super::super::resolve(&profile, &env, &store, super::super::unix_now())
            .unwrap()
            .unwrap();
        assert_eq!(credential.secret().expose(), "environment-secret");
        let session = Session::prepare(
            &profile,
            credential,
            None,
            "environment-interaction".into(),
            &Device::new(vec![]),
        )
        .await
        .unwrap();
        assert_eq!(session.target, "https://api.business.githubcopilot.com");
        session.check(&profile).await.unwrap();
        assert!(matches!(
            super::super::resolve(
                &profile,
                &crate::config::Environment::default(),
                &store,
                super::super::unix_now()
            ),
            Err(Error::AuthMissingEnv { .. })
        ));
        assert!(!store.path().exists());
    }
}

pub(crate) fn failure(reason: &'static str) -> Error {
    Error::Copilot { reason }
}

/// Provider-generated headers cannot be replaced with configuration values.
pub(crate) fn reserved_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "x-api-key"
            | "user-agent"
            | "x-github-api-version"
            | "openai-intent"
            | "x-interaction-type"
            | "x-interaction-id"
            | "x-initiator"
            | "copilot-vision-request"
    )
}

/// Validate before a provider-controlled target can receive a token or prompt.
pub(crate) fn validate_target(value: &str) -> Result<String> {
    if value.chars().any(char::is_whitespace) || value.chars().any(char::is_control) {
        return Err(failure(
            "invalid Copilot API target; check connection configuration",
        ));
    }
    let url = reqwest::Url::parse(value)
        .map_err(|_| failure("invalid Copilot API target; check connection configuration"))?;
    if url.scheme() != "https"
        || !matches!(
            url.host_str(),
            Some(
                "api.githubcopilot.com"
                    | "api.individual.githubcopilot.com"
                    | "api.business.githubcopilot.com"
                    | "api.enterprise.githubcopilot.com"
            )
        )
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(failure(
            "untrusted Copilot API target; use a supported GitHub.com connection",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').into())
}

/// Shared bearer presentation for discovery and all three inference protocols.
pub(crate) fn headers(secret: &Secret, version: &'static str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
        .map_err(|_| failure("invalid token header; replace the selected GitHub token"))?;
    bearer.set_sensitive(true);
    headers.insert("authorization", bearer);
    headers.insert(
        "user-agent",
        HeaderValue::from_static(concat!("bee/", env!("CARGO_PKG_VERSION"))),
    );
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers.insert("x-github-api-version", HeaderValue::from_static(version));
    Ok(headers)
}

/// Required target preparation remains available when model discovery is disabled.
pub(crate) async fn target(secret: &Secret, http: &dyn LoginHttp) -> Result<String> {
    let response = match http
        .get_authenticated(USER, headers(secret, "2025-04-01")?)
        .await
    {
        Ok(response) => response,
        Err(_) => return Ok(DEFAULT_TARGET.into()),
    };
    if matches!(response.status, 401 | 403) {
        return Err(failure(REJECTED));
    }
    if !(200..300).contains(&response.status) {
        return Ok(DEFAULT_TARGET.into());
    }
    let Ok(value) = serde_json::from_str::<Value>(&response.body) else {
        return Ok(DEFAULT_TARGET.into());
    };
    if value["chat_enabled"] == false {
        return Err(failure(NO_CHAT));
    }
    match value
        .get("endpoints")
        .and_then(|endpoints| endpoints.get("api"))
    {
        Some(Value::String(api)) => validate_target(api),
        Some(_) => Err(failure("invalid account API target; retry later")),
        None => Ok(DEFAULT_TARGET.into()),
    }
}

fn json_response(response: crate::http::HttpResponse) -> Result<Value> {
    if !(200..300).contains(&response.status) {
        return Err(failure(
            "GitHub device service rejected the request; retry login",
        ));
    }
    serde_json::from_str(&response.body)
        .map_err(|_| failure("invalid device service response; retry login"))
}

/// Pending device authorization. Debug deliberately omits all flow material.
pub struct SignIn {
    /// Validated page where the user enters the displayed code.
    pub url: String,
    /// Intentionally displayed only as part of login instructions.
    pub user_code: String,
    device_code: Secret,
    interval: Duration,
    deadline: tokio::time::Instant,
    previous: Option<Credential>,
    revision: u64,
}

impl SignIn {
    /// Capture store identity before requesting the device authorization.
    pub async fn prepare(store: &FileStore, profile: &str, http: &dyn LoginHttp) -> Result<Self> {
        let (previous, revision) = {
            let _guard = store.lock_async().await?;
            let document = store.load()?;
            (
                document.providers.get(profile).cloned(),
                document.revision(profile),
            )
        };
        let value = json_response(
            http.post_json(
                DEVICE,
                json!({"client_id":CLIENT_ID,"scope":"read:user"}).to_string(),
            )
            .await
            .map_err(|_| failure("GitHub device service unavailable; retry login"))?,
        )?;
        let text = |key: &str| {
            value[key]
                .as_str()
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                .map(str::to_owned)
                .ok_or_else(|| failure("invalid device service response; retry login"))
        };
        let url = text("verification_uri")?;
        if url != "https://github.com/login/device" {
            return Err(failure("untrusted device verification page; retry login"));
        }
        let expires = value["expires_in"]
            .as_u64()
            .filter(|n| (1..=900).contains(n))
            .ok_or_else(|| failure("invalid device expiration; retry login"))?;
        let interval = match value.get("interval") {
            None => 5,
            Some(value) => value
                .as_u64()
                .filter(|n| (1..=900).contains(n))
                .ok_or_else(|| failure("invalid device polling interval; retry login"))?,
        };
        let user_code = text("user_code")?;
        if user_code.len() != 9
            || !user_code.bytes().enumerate().all(|(index, byte)| {
                if index == 4 {
                    byte == b'-'
                } else {
                    byte.is_ascii_uppercase() || byte.is_ascii_digit()
                }
            })
        {
            return Err(failure("invalid user verification code; retry login"));
        }
        Ok(Self {
            url,
            user_code,
            device_code: Secret::new(text("device_code")?),
            interval: Duration::from_secs(interval),
            deadline: tokio::time::Instant::now() + Duration::from_secs(expires),
            previous,
            revision,
        })
    }

    /// Dropping this future abandons both polling and in-flight network requests.
    pub async fn complete(&self, http: &dyn LoginHttp) -> Result<Credential> {
        tokio::time::timeout_at(self.deadline, async {
            let mut interval = self.interval;
            loop {
                tokio::time::sleep(interval).await;
                let value = json_response(http.post_json(TOKEN, json!({"client_id":CLIENT_ID,"device_code":self.device_code.expose(),"grant_type":"urn:ietf:params:oauth:grant-type:device_code"}).to_string())
                    .await.map_err(|_| failure("GitHub device service unavailable; retry login"))?)?;
                if let Some(token) = value["access_token"].as_str().filter(|s| !s.trim().is_empty()) {
                    // The chosen public flow is long-lived; silently losing refresh authority would strand users.
                    if value.get("expires_in").is_some() || value.get("refresh_token").is_some() {
                        return Err(failure("this OAuth response requires unsupported token renewal; configure an environment token instead"));
                    }
                    if value["token_type"].as_str().is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer")) {
                        return Err(failure("unsupported device token type; retry login"));
                    }
                    let credential = Credential::new(Secret::new(token)).oauth_token(None).for_provider("github");
                    target(credential.secret(), http).await?;
                    return Ok(credential);
                }
                match value["error"].as_str() {
                    Some("authorization_pending") => {}
                    Some("slow_down") => {
                        interval = interval.saturating_add(Duration::from_secs(5));
                        if let Some(seconds) = value["interval"].as_u64().filter(|seconds| *seconds <= 900) {
                            interval = interval.max(Duration::from_secs(seconds));
                        }
                    }
                    Some("access_denied") => return Err(failure("device authorization declined; repeat login when ready")),
                    Some("expired_token") => return Err(failure("device authorization expired; repeat login")),
                    _ => return Err(failure("device authorization failed; repeat login")),
                }
            }
        }).await.map_err(|_| failure("device authorization expired; repeat login"))?
    }

    /// A later login or even an empty logout invalidates an older attempt.
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
        let _guard = store.lock_async().await?;
        // Keep cancellation and the final private-file commit on one side of the turn gate.
        let _current = current().ok_or_else(|| failure("device authorization cancelled"))?;
        let mut document = store.load()?;
        if document.revision(profile) != self.revision
            || document.providers.get(profile) != self.previous.as_ref()
        {
            return Err(failure(
                "credential changed during device authorization; repeat login",
            ));
        }
        document
            .providers
            .insert(profile.into(), credential.clone());
        document.changed(Audience::Provider(profile));
        store.save(&document)
    }
}

/// A validated, account-pinned request target, also usable by embedded callers.
#[derive(Clone)]
pub struct Session {
    pub(crate) target: String,
    pub(crate) credential: Credential,
    store: Option<FileStore>,
    revision: Option<u64>,
    profile: String,
    pub(crate) interaction: String,
}

impl Session {
    /// Resolve a trusted target without relying on optional model discovery.
    pub async fn prepare(
        profile: &Profile,
        credential: Credential,
        store: Option<FileStore>,
        interaction: String,
        http: &dyn LoginHttp,
    ) -> Result<Self> {
        if !profile.is_copilot()
            || credential.oauth.is_some()
            || credential.is_expired(super::unix_now())
        {
            return Err(failure(
                "Copilot requires a valid GitHub user token and Copilot connection",
            ));
        }
        let stored = matches!(profile.credential(), Some(CredentialSource::Store { .. }));
        if stored != store.is_some() {
            return Err(failure(
                "Copilot credential source does not match the request session",
            ));
        }
        // Validate header syntax even when the optional account service is unavailable.
        headers(credential.secret(), "2026-08-01")?;
        let revision = if let Some(store) = &store {
            let document = store.load()?;
            if document.providers.get(profile.name()) != Some(&credential) {
                return Err(failure(
                    "Copilot credentials changed during this turn; start a new turn",
                ));
            }
            Some(document.revision(profile.name()))
        } else {
            None
        };
        let target = target(credential.secret(), http).await?;
        let session = Self {
            target,
            credential,
            store,
            revision,
            profile: profile.name().into(),
            interaction,
        };
        session.check(profile).await?;
        Ok(session)
    }

    pub(crate) async fn check(&self, profile: &Profile) -> Result<()> {
        if !profile.is_copilot() || profile.name() != self.profile {
            return Err(failure(
                "Copilot request session belongs to a different connection",
            ));
        }
        if let Some(store) = &self.store {
            let store = store.clone();
            let profile = self.profile.clone();
            let (current, revision) = tokio::task::spawn_blocking(move || {
                let document = store.load()?;
                Ok::<_, Error>((
                    document.providers.get(&profile).cloned(),
                    document.revision(&profile),
                ))
            })
            .await
            .map_err(|_| failure("cannot read Copilot credentials; retry login"))??;
            if current.as_ref() != Some(&self.credential) || Some(revision) != self.revision {
                return Err(failure(
                    "Copilot credentials changed during this turn; start a new turn",
                ));
            }
        }
        Ok(())
    }
}
