//! Bounded Google ADC flows, separate from Bee-owned login and credential storage.

#[cfg(test)]
mod tests;

use std::{path::PathBuf, sync::Arc};

use futures::future::BoxFuture;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::{Credential, Secret, unix_now};
use crate::{
    account::Account as Profile,
    config::Environment,
    error::{Error, Result},
    provider::GoogleCloud,
};

pub(crate) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(crate) const METADATA: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
pub(crate) const MAX_DOCUMENT: usize = 1024 * 1024;

pub(crate) fn failure(reason: &'static str) -> Error {
    Error::GoogleCloud { reason }
}

/// Fixed operations prevent credential files or projects from choosing token destinations.
pub enum Request {
    /// An OAuth form sent only to Google's token service.
    Token(Vec<(String, Secret)>),
    /// The attached service account's email, not an arbitrary metadata path.
    MetadataIdentity,
    /// The attached service account's OAuth token.
    MetadataToken,
}

/// Injectable authentication HTTP; implementations must bound responses and disable redirects.
pub trait Http: Send + Sync {
    /// Acquire one response without exposing any request or upstream error text.
    fn request(&self, request: Request) -> BoxFuture<'_, Result<Value>>;
}

#[derive(Clone)]
enum Source {
    File { path: PathBuf, document: Value },
    Metadata,
}

fn source(env: &Environment) -> Result<Source> {
    let explicit = env
        .variable("GOOGLE_APPLICATION_CREDENTIALS")
        .map(PathBuf::from);
    let local = env
        .variable("CLOUDSDK_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(windows)]
            {
                env.variable("APPDATA")
                    .map(|p| PathBuf::from(p).join("gcloud"))
            }
            #[cfg(not(windows))]
            {
                env.home_dir().map(|p| p.join(".config/gcloud"))
            }
        })
        .map(|p| p.join("application_default_credentials.json"));
    let Some(path) = explicit.clone().or(local) else {
        return Ok(Source::Metadata);
    };
    match std::fs::metadata(&path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(failure("ADC path must name a regular credential file"));
        }
        Err(error) if explicit.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Source::Metadata);
        }
        Err(_) => {
            return Err(failure(
                "cannot inspect ADC file; check its path and permissions",
            ));
        }
        Ok(_) => {}
    }
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if explicit.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Source::Metadata);
        }
        Err(_) => {
            return Err(failure(
                "cannot read ADC file; check GOOGLE_APPLICATION_CREDENTIALS and file permissions",
            ));
        }
    };
    // Read a bounded prefix even for special files or a file that grows after stat.
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure("cannot read ADC file; check file permissions"))?;
    if bytes.len() > MAX_DOCUMENT {
        return Err(failure("ADC file exceeds the size limit"));
    }
    let document: Value = serde_json::from_slice(&bytes)
        .map_err(|_| failure("invalid ADC JSON; recreate application default credentials"))?;
    validate(&document)?;
    Ok(Source::File { path, document })
}

fn field<'a>(document: &'a Value, key: &str) -> Result<&'a str> {
    document[key].as_str().filter(|s| !s.is_empty())
        .ok_or_else(|| failure("ADC is missing required credential fields; recreate application default credentials"))
}

fn validate(document: &Value) -> Result<()> {
    match field(document, "type")? {
        "authorized_user" => {
            for key in ["client_id", "client_secret", "refresh_token"] {
                field(document, key)?;
            }
        }
        "service_account" => {
            for key in ["client_email", "private_key"] {
                field(document, key)?;
            }
        }
        _ => {
            return Err(failure(
                "unsupported ADC type; use authorized-user ADC, a service-account key, or an attached service account",
            ));
        }
    }
    // Impersonation and alternate universes need their own audited routing contract.
    if document.get("service_account_impersonation_url").is_some()
        || document
            .get("universe_domain")
            .is_some_and(|v| v != "googleapis.com")
        || document
            .get("token_uri")
            .is_some_and(|v| v != TOKEN_URL && v != "https://accounts.google.com/o/oauth2/token")
    {
        return Err(failure(
            "ADC requires an unsupported token destination, universe or impersonation flow",
        ));
    }
    quota(document)?;
    Ok(())
}

fn quota(document: &Value) -> Result<Option<String>> {
    document
        .get("quota_project_id")
        .map(|v| {
            let value = v
                .as_str()
                .filter(|v| crate::provider::cloud_identifier(v))
                .ok_or_else(|| {
                    failure("invalid ADC quota project; recreate ADC or correct its quota project")
                })?;
            Ok(value.to_owned())
        })
        .transpose()
}

fn target(profile: &Profile) -> Result<GoogleCloud> {
    let target = profile
        .google_cloud()
        .cloned()
        .ok_or_else(|| failure("prepare a Google Cloud connection before authentication"))?;
    if target.project.is_none() {
        return Err(failure(
            "missing resource project; configure google_cloud.project",
        ));
    }
    Ok(target)
}

fn fingerprint(source: &Source, principal: Option<&str>, target: &GoogleCloud) -> Result<Secret> {
    let authority = match source {
        // Canonical object serialization ignores whitespace/key order but detects replacement credentials.
        Source::File { path, document } => {
            json!(["file", path.as_os_str().as_encoded_bytes(), document])
        }
        Source::Metadata => json!([
            "metadata",
            principal
                .ok_or_else(|| failure("attached service-account identity is not resolved"))?
        ]),
    };
    let bytes = serde_json::to_vec(&(
        authority,
        &target.project,
        &target.location,
        &target.quota_project,
    ))
    .map_err(|_| failure("cannot bind Google Cloud credentials"))?;
    let digest = Sha256::digest(bytes);
    Ok(Secret::new(
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    ))
}

fn bound(secret: Secret, identity: Secret, expires_at: Option<u64>) -> Credential {
    Credential {
        material: super::Material::OAuth {
            refresh_token: None,
        },
        provider_type: Some("google-cloud".into()),
        secret,
        cloud_identity: Some(identity),
        expires_at,
        oauth: None,
    }
}

/// Inspect file-backed authority without acquiring or refreshing tokens.
/// Metadata identity must instead be checked at asynchronous prompt preparation.
pub(crate) fn identity(profile: &Profile, env: &Environment) -> Result<Option<Credential>> {
    let target = target(profile)?;
    let source = source(env)?;
    if matches!(source, Source::Metadata) {
        return Ok(None);
    }
    Ok(Some(bound(
        Secret::new(""),
        fingerprint(&source, None, &target)?,
        None,
    )))
}

struct State {
    credential: Option<Credential>,
}

/// A turn pins account and Cloud target while access tokens rotate in memory.
#[derive(Clone)]
pub struct Session {
    env: Environment,
    source: Source,
    target: GoogleCloud,
    identity: Secret,
    quota_project: Option<String>,
    http: Arc<dyn Http>,
    state: Arc<Mutex<State>>,
}

impl Session {
    /// Prepare official ADC using the production bounded transport.
    pub async fn prepare(profile: &Profile, env: &Environment) -> Result<Self> {
        Self::with_http(profile, env, Arc::new(crate::http::GoogleAuthHttp)).await
    }

    /// Prepare with an injected authentication transport for embedded runtimes and tests.
    pub async fn with_http(
        profile: &Profile,
        env: &Environment,
        http: Arc<dyn Http>,
    ) -> Result<Self> {
        let target = target(profile)?;
        let source = source(env)?;
        let principal = if matches!(source, Source::Metadata) {
            Some(metadata_identity(http.as_ref()).await?)
        } else {
            None
        };
        let quota_project = target.quota_project.clone().or(match &source {
            Source::File { document, .. } => quota(document)?,
            Source::Metadata => None,
        });
        let identity = fingerprint(&source, principal.as_deref(), &target)?;
        let session = Self {
            env: env.clone(),
            source,
            target,
            identity,
            quota_project,
            http,
            state: Arc::new(Mutex::new(State { credential: None })),
        };
        session.credential().await?;
        Ok(session)
    }

    /// Validate source authority and renew only an expiring token; concurrent steps share renewal.
    pub async fn credential(&self) -> Result<Credential> {
        let mut state = self.state.lock().await;
        let current = source(&self.env)?;
        let principal = if matches!(current, Source::Metadata) {
            Some(metadata_identity(self.http.as_ref()).await?)
        } else {
            None
        };
        if fingerprint(&current, principal.as_deref(), &self.target)? != self.identity {
            return Err(failure(
                "ADC account or source changed; select a model explicitly and start a new turn",
            ));
        }
        let now = unix_now();
        if let Some(credential) = &state.credential
            && !credential.is_expired(now.saturating_add(60))
        {
            return Ok(credential.clone());
        }
        let request = match &self.source {
            Source::Metadata => Request::MetadataToken,
            Source::File { document, .. } if document["type"] == "authorized_user" => {
                Request::Token(vec![
                    ("grant_type".into(), Secret::new("refresh_token")),
                    (
                        "client_id".into(),
                        Secret::new(field(document, "client_id")?),
                    ),
                    (
                        "client_secret".into(),
                        Secret::new(field(document, "client_secret")?),
                    ),
                    (
                        "refresh_token".into(),
                        Secret::new(field(document, "refresh_token")?),
                    ),
                ])
            }
            Source::File { document, .. } => {
                let key = jsonwebtoken::EncodingKey::from_rsa_pem(
                    field(document, "private_key")?.as_bytes(),
                )
                .map_err(|_| {
                    failure("invalid service-account private key; replace the credential file")
                })?;
                let claims = json!({"iss":field(document, "client_email")?, "scope":SCOPE, "aud":TOKEN_URL, "iat":now, "exp":now.saturating_add(3600)});
                let assertion = jsonwebtoken::encode(
                    &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
                    &claims,
                    &key,
                )
                .map_err(|_| {
                    failure("cannot sign service-account assertion; replace the credential file")
                })?;
                Request::Token(vec![
                    (
                        "grant_type".into(),
                        Secret::new("urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ),
                    ("assertion".into(), Secret::new(assertion)),
                ])
            }
        };
        let response = self.http.request(request).await?;
        if !response["token_type"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case("bearer"))
        {
            return Err(failure("invalid OAuth token type; retry authentication"));
        }
        let token = field(&response, "access_token")?;
        let expires = response["expires_in"]
            .as_u64()
            .filter(|v| *v > 60 && *v <= 86400)
            .ok_or_else(|| failure("invalid OAuth token lifetime; retry authentication"))?;
        // Validate before accepting credentials so malformed header bytes never reach inference.
        reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| failure("invalid OAuth token; retry authentication"))?;
        let credential = bound(
            Secret::new(token),
            self.identity.clone(),
            Some(now.saturating_add(expires)),
        );
        state.credential = Some(credential.clone());
        Ok(credential)
    }

    pub(crate) fn check(&self, profile: &Profile) -> Result<()> {
        if profile.google_cloud() != Some(&self.target) {
            return Err(failure(
                "prepared Cloud target does not match the selected connection",
            ));
        }
        Ok(())
    }

    pub(crate) fn quota_project(&self) -> Option<&str> {
        self.quota_project.as_deref()
    }
}

async fn metadata_identity(http: &dyn Http) -> Result<String> {
    let response = http.request(Request::MetadataIdentity).await?;
    response
        .as_str()
        .filter(|s| !s.is_empty() && s.len() < 1024 && !s.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or_else(|| {
            failure("cannot identify attached service account; check metadata availability")
        })
}
