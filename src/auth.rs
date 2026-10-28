//! Provider credentials, free of the ACPv2 SDK.
//!
//! A profile's `credential` (see [`crate::provider::CredentialSource`]) says
//! *where* its secret is; this module finds it, keeps it, and makes sure it
//! never shows up in output. The pieces:
//!
//! - [`Secret`]: a string that cannot be printed by accident.
//! - [`CredentialStore`] and [`FileStore`]: where `bee auth login` keeps what
//!   it obtains, in a private file.
//! - [`resolve`]: fixed-credential lookup; [`chatgpt`] owns renewable sessions.
//! - [`header_for`]: the ordinary protocol defaults for presenting that secret.
//!
//! Authentication is configured apart from the protocol and the models: a
//! profile may use any protocol with any source. Protocols supply default secret
//! presentation; native authentication can override it. In particular, Copilot
//! uses bearer authentication across all three protocols, including Messages.
//! LLM provider credentials and MCP
//! server authorization are different audiences and are stored apart
//! ([`Audience`]), so a token for one can never be returned for the other. See
//! ADR-008.
//!
//! Nothing here prints. Every diagnostic names a profile, a variable, a path
//! or a status, and never a value: the credential file is where keys live.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::account::{Account as Profile, Accounts as Providers};
use crate::config::Environment;
use crate::error::{Error, Result};
use crate::provider::{CredentialSource, Protocol};

pub mod chatgpt;
pub mod copilot;
pub mod google_cloud;

/// The version of the credential file's layout.
// Publication and bounded reads must agree, or one successful login can disable every account.
const MAX_STORE_BYTES: usize = 4 * 1024 * 1024;

/// A credential that cannot be printed by accident.
///
/// No `Display`, no `Serialize`, and a `Debug` that hides the value: a secret
/// reaches output only through [`Secret::expose`], which names the intent.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself. Call this only where the value is used: an HTTP
    /// header, or the credential file.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A resolved secret and when it stops working.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    material: Material,
    // Native authority is not interchangeable with an arbitrary compatible endpoint.
    provider_type: Option<String>,
    pub(crate) cloud_identity: Option<Secret>,
    secret: Secret,
    /// Seconds since the Unix epoch, for credentials that expire.
    expires_at: Option<u64>,
    pub(crate) oauth: Option<chatgpt::Registration>,
}

/// Generic credential material; native registration remains separately typed.
#[derive(Clone, PartialEq, Eq)]
pub enum Material {
    /// The primary key is `Credential::secret`; additional declared secret fields are bounded by the store.
    ApiKey {
        /// Additional provider-declared secret fields, stored as one generation.
        fields: BTreeMap<String, Secret>,
    },
    /// The primary secret is an access token. Some grants have no refresh token.
    OAuth {
        /// Optional renewable grant, never displayed or sent as ordinary metadata.
        refresh_token: Option<Secret>,
    },
}

impl fmt::Debug for Material {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { fields } => f
                .debug_struct("ApiKey")
                .field("field_count", &fields.len())
                .finish(),
            Self::OAuth { refresh_token } => f
                .debug_struct("OAuth")
                .field("has_refresh_token", &refresh_token.is_some())
                .finish(),
        }
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("material", &self.material)
            .field("expires_at", &self.expires_at)
            .field("has_registration", &self.oauth.is_some())
            .field("has_provider_type", &self.provider_type.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for Material {
    fn default() -> Self {
        Self::ApiKey {
            fields: BTreeMap::new(),
        }
    }
}

impl Credential {
    /// Construct a complete static-secret generation before any store mutation.
    pub fn api_key_fields(secret: Secret, fields: BTreeMap<String, Secret>) -> Self {
        let mut credential = Self::new(secret);
        credential.material = Material::ApiKey { fields };
        credential
    }

    /// Provider-declared field validation never formats secrets or metadata into errors.
    pub fn validate_for(&self, profile: &Profile) -> Result<()> {
        if self
            .provider_type
            .as_deref()
            .is_some_and(|kind| kind != profile.provider())
        {
            return Err(Error::ProviderInvalid {
                path: "<credential store>".into(),
                key: "provider type".into(),
                reason: "stored credential belongs to a different provider type; explicitly authenticate this instance again",
            });
        }
        let definition = crate::provider::registry::definition(profile.provider())
            .expect("validated provider type");
        if !definition.methods.contains(&self.method()) {
            return Err(Error::ProviderInvalid {
                path: "<credential store>".into(),
                key: "authentication method".into(),
                reason: "credential method is incompatible with this provider type; authenticate this instance again",
            });
        }
        if let Material::ApiKey { fields } = &self.material {
            let mut values = toml::Table::from_iter([(
                "key".into(),
                toml::Value::String(self.secret.expose().into()),
            )]);
            values.extend(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), toml::Value::String(value.expose().into()))),
            );
            definition
                .validate_fields(crate::provider::registry::FieldOwner::Credential, &values)
                .map_err(|reason| Error::ProviderInvalid {
                    path: "<credential source>".into(),
                    key: "credential fields".into(),
                    reason,
                })?;
        }
        Ok(())
    }
    /// Generic authentication method, never a provider-specific variant.
    pub fn method(&self) -> crate::provider::AuthMethod {
        match self.material {
            Material::ApiKey { .. } => crate::provider::AuthMethod::ApiKey,
            Material::OAuth { .. } => crate::provider::AuthMethod::OAuth,
        }
    }

    /// The typed secondary credential material. Secrets retain redacted Debug.
    pub fn material(&self) -> &Material {
        &self.material
    }

    /// Mark a provider-acquired OAuth token without assuming a refresh grant exists.
    pub fn oauth_token(mut self, refresh_token: Option<Secret>) -> Self {
        self.material = Material::OAuth { refresh_token };
        self
    }

    /// Bind newly acquired credentials to the integration that validated them.
    pub fn for_provider(mut self, provider_type: &str) -> Self {
        self.provider_type = Some(provider_type.into());
        self
    }
    pub(crate) fn selection_oauth_usable(&self) -> bool {
        // An expired renewable token is usable, but logout preserves registration identity without authority.
        self.oauth.is_some()
            && matches!(
                chatgpt::store_state(self, unix_now()),
                StoreState::LoggedIn | StoreState::Refreshable
            )
    }
    pub(crate) fn selection_identity(&self) -> serde_json::Value {
        if let Some(identity) = &self.cloud_identity {
            return serde_json::json!(["google-cloud", identity.expose()]);
        }
        // This value is immediately hashed by selection; never expose it through Debug.
        match &self.oauth {
            Some(registration) => registration.selection_identity(),
            None => match &self.material {
                Material::ApiKey { fields } if !fields.is_empty() => serde_json::json!([
                    "key-fields",
                    self.secret.expose(),
                    fields
                        .iter()
                        .map(|(key, value)| (key, value.expose()))
                        .collect::<BTreeMap<_, _>>()
                ]),
                _ => serde_json::json!(["key", self.secret.expose()]),
            },
        }
    }
    /// A credential that does not expire.
    #[must_use]
    pub fn new(secret: Secret) -> Self {
        Self {
            material: Material::default(),
            provider_type: None,
            cloud_identity: None,
            secret,
            expires_at: None,
            oauth: None,
        }
    }

    /// The same credential, expiring at `expires_at` (Unix seconds).
    #[must_use]
    pub fn expiring_at(mut self, expires_at: u64) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// The secret.
    #[must_use]
    pub fn secret(&self) -> &Secret {
        &self.secret
    }

    /// When it expires, if it does.
    #[must_use]
    pub fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }

    /// Whether a validated ChatGPT identity granted subscription-backed inference.
    #[must_use]
    pub fn chatgpt_plan_enabled(&self) -> Option<bool> {
        self.oauth.as_ref().map(chatgpt::Registration::plan_enabled)
    }
    pub(crate) fn same_discovery_account(&self, other: &Self) -> bool {
        if self.cloud_identity.is_some() || other.cloud_identity.is_some() {
            return self.cloud_identity == other.cloud_identity;
        }
        match (&self.oauth, &other.oauth) {
            (Some(a), Some(b)) => a.same_discovery_account(b),
            (None, None) => self.selection_identity() == other.selection_identity(),
            _ => false,
        }
    }

    /// Whether it has stopped working at `now` (Unix seconds).
    #[must_use]
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

/// Who a credential is for.
///
/// Two audiences exist and are never interchangeable: a token that lets Bee
/// call a model must not be offered to an MCP server, and the other way
/// round. The store keeps them in separate tables, so no lookup can cross
/// them even when the names collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience<'a> {
    /// An LLM provider profile, by profile name.
    Provider(&'a str),
    /// A remote MCP server, by canonical URL. Reserved for MCP authorization
    /// (ADR-008): the store keeps the audience apart now so that flow can
    /// arrive without migrating the file.
    McpServer(&'a str),
}

impl Audience<'_> {
    fn name(self) -> String {
        match self {
            Self::Provider(name) | Self::McpServer(name) => name.to_owned(),
        }
    }
}

/// Where credentials Bee obtained are kept.
///
/// A trait so an OS keyring can be added beside the file without touching the
/// callers (ADR-008).
pub trait CredentialStore {
    /// Snapshot a provider generation before interaction; unsupported stores return no generation.
    fn generation(&self, _audience: Audience<'_>) -> Result<Option<u64>> {
        Ok(None)
    }

    /// Publish the authenticated generation. Concurrent stores override this atomically.
    fn put_guarded(
        &self,
        audience: Audience<'_>,
        credential: &Credential,
        _generation: Option<u64>,
    ) -> Result<()> {
        self.put(audience, credential)
    }
    /// File-backed OAuth transactions require a stable OS lock across processes.
    fn file_store(&self) -> Option<&FileStore> {
        None
    }
    /// The credential stored for `audience`, if any.
    ///
    /// # Errors
    ///
    /// A `bee::auth::*` error if the store cannot be read or is not private.
    fn get(&self, audience: Audience<'_>) -> Result<Option<Credential>>;

    /// Store `credential` for `audience`, replacing any previous one.
    ///
    /// # Errors
    ///
    /// A `bee::auth::*` error if the store cannot be read or written.
    fn put(&self, audience: Audience<'_>, credential: &Credential) -> Result<()>;

    /// Remove the credential for `audience`. `false` if there was none.
    ///
    /// # Errors
    ///
    /// A `bee::auth::*` error if the store cannot be read or written.
    fn remove(&self, audience: Audience<'_>) -> Result<bool>;
}

/// What the credential file holds, by audience.
#[derive(Default)]
struct Document {
    profile_revisions: BTreeMap<String, u64>,
    host_id: Option<String>,
    providers: BTreeMap<String, Credential>,
    mcp_servers: BTreeMap<String, Credential>,
}

impl Document {
    fn revision(&self, profile: &str) -> u64 {
        self.profile_revisions.get(profile).copied().unwrap_or(0)
    }

    fn changed(&mut self, audience: Audience<'_>) {
        if let Audience::Provider(profile) = audience {
            // Tombstones distinguish even an empty logout from an untouched registration.
            self.profile_revisions
                .insert(profile.into(), self.revision(profile).saturating_add(1));
        }
    }
    fn table(&self, audience: Audience<'_>) -> &BTreeMap<String, Credential> {
        match audience {
            Audience::Provider(_) => &self.providers,
            Audience::McpServer(_) => &self.mcp_servers,
        }
    }

    fn table_mut(&mut self, audience: Audience<'_>) -> &mut BTreeMap<String, Credential> {
        match audience {
            Audience::Provider(_) => &mut self.providers,
            Audience::McpServer(_) => &mut self.mcp_servers,
        }
    }

    fn is_empty(&self) -> bool {
        self.host_id.is_none()
            && self.profile_revisions.is_empty()
            && self.providers.is_empty()
            && self.mcp_servers.is_empty()
    }
}

/// Credentials in one JSON file, private to the user.
///
/// The file is created with mode `0600` in a `0700` directory, replaced
/// atomically so a crash never leaves half a file, and refused on read when
/// group or others have any access: a credential somebody else could read is
/// already leaked, and saying so beats quietly using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    /// A store in the file at `path`. Nothing is touched until it is used.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The store in its default place for `env`.
    ///
    /// # Errors
    ///
    /// [`Error::AuthNoStorePath`] if neither `XDG_DATA_HOME` nor `HOME` is
    /// usable.
    pub fn from_environment(env: &Environment) -> Result<Self> {
        env.credentials_path()
            .map(Self::new)
            .ok_or(Error::AuthNoStorePath)
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn display_path(&self) -> String {
        self.path.display().to_string()
    }

    fn parse_error(&self, reason: impl Into<String>) -> Error {
        Error::AuthStoreParse {
            path: self.display_path(),
            reason: reason.into(),
        }
    }

    fn load(&self) -> Result<Document> {
        let Some(bytes) = platform::read_private(&self.path)? else {
            return Ok(Document::default());
        };
        self.parse(&bytes)
    }

    fn parse(&self, bytes: &[u8]) -> Result<Document> {
        // `Value`, not a derived type: serde's type errors quote the offending
        // value, and this file is full of secrets. Syntax errors only say what
        // was expected, and are reduced to a category and a position anyway.
        let root: Value = serde_json::from_slice(bytes).map_err(|error| {
            let what = match error.classify() {
                serde_json::error::Category::Eof => "the file ends unexpectedly",
                _ => "the file is not valid JSON",
            };
            self.parse_error(format!(
                "{what} (line {}, column {})",
                error.line(),
                error.column()
            ))
        })?;
        let Value::Object(root) = root else {
            return Err(self.parse_error("the top level must be an object"));
        };
        if root.keys().any(|key| {
            !matches!(
                key.as_str(),
                "version" | "providers" | "mcp_servers" | "host_id" | "profile_revisions"
            )
        }) {
            return Err(self.parse_error("it has a key Bee does not write"));
        }
        if !matches!(root.get("version").and_then(Value::as_u64), Some(1..=3)) {
            return Err(self.parse_error("its `version` is missing or not one Bee understands"));
        }
        Ok(Document {
            profile_revisions: match root.get("profile_revisions") {
                None => BTreeMap::new(),
                Some(Value::Object(revisions)) => revisions
                    .iter()
                    .map(|(name, value)| {
                        value
                            .as_u64()
                            .map(|v| (name.clone(), v))
                            .ok_or_else(|| self.parse_error("profile revisions must be integers"))
                    })
                    .collect::<Result<_>>()?,
                Some(_) => return Err(self.parse_error("profile revisions must be an object")),
            },
            host_id: root
                .get("host_id")
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| self.parse_error("host identity must be text"))
                })
                .transpose()?,
            providers: self.entries(&root, "providers")?,
            mcp_servers: self.entries(&root, "mcp_servers")?,
        })
    }

    fn entries(
        &self,
        root: &Map<String, Value>,
        table: &str,
    ) -> Result<BTreeMap<String, Credential>> {
        let Some(value) = root.get(table) else {
            return Ok(BTreeMap::new());
        };
        let Value::Object(entries) = value else {
            return Err(self.parse_error(format!("`{table}` must be an object")));
        };
        let mut credentials = BTreeMap::new();
        for (name, entry) in entries {
            let Value::Object(entry) = entry else {
                return Err(self.parse_error(format!("every entry of `{table}` must be an object")));
            };
            if entry.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "secret" | "expires_at" | "oauth" | "material" | "provider_type"
                )
            }) {
                return Err(self.parse_error(format!(
                    "an entry of `{table}` has a key Bee does not write"
                )));
            }
            let oauth = entry
                .get("oauth")
                .map(chatgpt::Registration::parse)
                .transpose()?;
            let secret = match entry.get("secret") {
                Some(Value::String(secret)) if !secret.is_empty() || oauth.is_some() => {
                    Secret::new(secret.as_str())
                }
                _ => {
                    return Err(self.parse_error(format!(
                        "every entry of `{table}` needs a `secret` that is non-empty text"
                    )));
                }
            };
            let expires_at = match entry.get("expires_at") {
                None => None,
                Some(value) => Some(value.as_u64().ok_or_else(|| {
                    self.parse_error(format!(
                        "`expires_at` in `{table}` must be a whole number of seconds"
                    ))
                })?),
            };
            let provider_type = entry
                .get("provider_type")
                .map(|v| {
                    v.as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| self.parse_error("provider type must be nonempty text"))
                })
                .transpose()?;
            // Older files carry no method discriminator; only native registration proves OAuth.
            let material = match entry.get("material") {
                None if oauth.is_some() => Material::OAuth {
                    refresh_token: None,
                },
                None => Material::default(),
                Some(value) => {
                    let object = value
                        .as_object()
                        .ok_or_else(|| self.parse_error("credential material must be an object"))?;
                    match object.get("method").and_then(Value::as_str) {
                        Some("api-key")
                            if object
                                .keys()
                                .all(|k| matches!(k.as_str(), "method" | "fields"))
                                && oauth.is_none() =>
                        {
                            let fields =
                                object.get("fields").and_then(Value::as_object).ok_or_else(
                                    || self.parse_error("API key fields must be an object"),
                                )?;
                            let fields = fields
                                .iter()
                                .map(|(key, value)| {
                                    value
                                        .as_str()
                                        .filter(|v| !v.trim().is_empty())
                                        .map(|v| (key.clone(), Secret::new(v)))
                                        .ok_or_else(|| {
                                            self.parse_error(
                                                "credential fields must be nonempty text",
                                            )
                                        })
                                })
                                .collect::<Result<_>>()?;
                            Material::ApiKey { fields }
                        }
                        Some("oauth")
                            if object
                                .keys()
                                .all(|k| matches!(k.as_str(), "method" | "refresh_token")) =>
                        {
                            let refresh_token = object
                                .get("refresh_token")
                                .map(|v| {
                                    v.as_str()
                                        .filter(|v| !v.is_empty())
                                        .map(Secret::new)
                                        .ok_or_else(|| {
                                            self.parse_error("refresh token must be nonempty text")
                                        })
                                })
                                .transpose()?;
                            Material::OAuth { refresh_token }
                        }
                        _ => {
                            return Err(
                                self.parse_error("unsupported or inconsistent credential material")
                            );
                        }
                    }
                }
            };
            credentials.insert(
                name.clone(),
                Credential {
                    material,
                    provider_type,
                    cloud_identity: None,
                    secret,
                    expires_at,
                    oauth,
                },
            );
        }
        Ok(credentials)
    }

    fn save(&self, document: &Document) -> Result<()> {
        if document.is_empty() {
            // Empty MCP-only stores can disappear; provider revision tombstones
            // must survive to invalidate a device login that completes after logout.
            return platform::remove(&self.path);
        }
        let table = |credentials: &BTreeMap<String, Credential>| {
            Value::Object(
                credentials
                    .iter()
                    .map(|(name, credential)| {
                        let mut entry = Map::new();
                        entry.insert(
                            "secret".to_owned(),
                            Value::String(credential.secret.expose().to_owned()),
                        );
                        if let Some(at) = credential.expires_at {
                            entry.insert("expires_at".to_owned(), Value::from(at));
                        }
                        if let Some(oauth) = &credential.oauth {
                            entry.insert("oauth".into(), oauth.json());
                        }
                        if let Some(provider_type) = &credential.provider_type {
                            entry.insert("provider_type".into(), Value::String(provider_type.clone()));
                        }
                        // Only the private store serializer may unwrap secret-bearing fields.
                        let material = match &credential.material {
                            Material::ApiKey { fields } => serde_json::json!({"method": "api-key", "fields": fields.iter().map(|(k,v)| (k, v.expose())).collect::<BTreeMap<_,_>>() }),
                            Material::OAuth { refresh_token } => {
                                let mut material = serde_json::json!({"method": "oauth"});
                                if let Some(token) = refresh_token { material["refresh_token"] = token.expose().into(); }
                                material
                            }
                        };
                        entry.insert("material".into(), material);
                        (name.clone(), Value::Object(entry))
                    })
                    .collect(),
            )
        };
        let mut root = Map::new();
        root.insert("version".to_owned(), Value::from(3));
        if let Some(host) = &document.host_id {
            root.insert("host_id".into(), host.clone().into());
        }
        if !document.profile_revisions.is_empty() {
            root.insert(
                "profile_revisions".into(),
                serde_json::to_value(&document.profile_revisions).expect("integer map serializes"),
            );
        }
        root.insert("providers".to_owned(), table(&document.providers));
        root.insert("mcp_servers".to_owned(), table(&document.mcp_servers));
        let mut bytes = serde_json::to_vec_pretty(&Value::Object(root))
            .map_err(|_| self.parse_error("the credentials could not be serialized"))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_STORE_BYTES {
            return Err(self.parse_error("credential file is too large"));
        }
        // Use the actual reader schema before replacement, including OAuth's empty-token states.
        // This also rejects invalid public CredentialStore::put inputs without corrupting other accounts.
        self.parse(&bytes)?;
        platform::write_private(&self.path, &bytes)
    }
}

impl CredentialStore for FileStore {
    fn generation(&self, audience: Audience<'_>) -> Result<Option<u64>> {
        #[cfg(unix)]
        {
            match audience {
                Audience::Provider(name) => Ok(Some(self.load()?.revision(name))),
                Audience::McpServer(_) => Ok(None),
            }
        }
        #[cfg(not(unix))]
        {
            // A generation is only available with private storage. Defer unsupported-platform
            // errors until publication so input/flow validation keeps its actionable diagnostic.
            let _ = audience;
            Ok(None)
        }
    }
    fn put_guarded(
        &self,
        audience: Audience<'_>,
        credential: &Credential,
        generation: Option<u64>,
    ) -> Result<()> {
        let _guard = self.lock()?;
        let mut document = self.load()?;
        if let Audience::Provider(name) = audience
            && generation != Some(document.revision(name))
        {
            return Err(Error::AuthCredentialsChanged);
        }
        document
            .table_mut(audience)
            .insert(audience.name(), credential.clone());
        document.changed(audience);
        self.save(&document)
    }
    fn file_store(&self) -> Option<&FileStore> {
        Some(self)
    }
    fn get(&self, audience: Audience<'_>) -> Result<Option<Credential>> {
        Ok(self.load()?.table(audience).get(&audience.name()).cloned())
    }

    fn put(&self, audience: Audience<'_>, credential: &Credential) -> Result<()> {
        let _guard = self.lock()?;
        self.put_locked(audience, credential)
    }

    fn remove(&self, audience: Audience<'_>) -> Result<bool> {
        let _guard = self.lock()?;
        let mut document = self.load()?;
        let removed = document
            .table_mut(audience)
            .remove(&audience.name())
            .is_some();
        document.changed(audience);
        if removed || !document.profile_revisions.is_empty() {
            self.save(&document)?;
        }
        Ok(removed)
    }
}

impl FileStore {
    /// Caller holds the sibling OS lock; avoids reacquisition under ACP's session gate.
    pub(crate) fn put_locked(&self, audience: Audience<'_>, credential: &Credential) -> Result<()> {
        let mut document = self.load()?;
        document
            .table_mut(audience)
            .insert(audience.name(), credential.clone());
        document.changed(audience);
        self.save(&document)
    }
    /// Yield during process-lock contention so ACP cancellation remains responsive.
    pub(crate) async fn lock_async(&self) -> Result<std::fs::File> {
        let start = std::time::Instant::now();
        loop {
            if let Some(guard) = self.try_lock()? {
                return Ok(guard);
            }
            if start.elapsed() >= std::time::Duration::from_secs(30) {
                return Err(Error::AuthStoreBusy);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    /// Stable sibling locks survive atomic credential-file replacement.
    pub(crate) fn try_lock(&self) -> Result<Option<std::fs::File>> {
        let path = self.path.with_extension("lock");
        platform::open_lock(&path)
    }

    fn lock(&self) -> Result<std::fs::File> {
        let start = std::time::Instant::now();
        loop {
            if let Some(guard) = self.try_lock()? {
                return Ok(guard);
            }
            if start.elapsed() > std::time::Duration::from_secs(30) {
                return Err(Error::AuthStoreBusy);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// The operating-system specifics of keeping a private file.
#[cfg(unix)]
mod platform {
    use std::fs::{self, DirBuilder, File, OpenOptions};
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::path::Path;

    use crate::error::{Error, Result};

    pub fn open_lock(path: &Path) -> Result<Option<File>> {
        let error = |source| Error::AuthStoreWrite {
            path: path.display().to_string(),
            source,
        };
        if let Some(parent) = path.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(error)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(error)?;
        if file.metadata().map_err(error)?.mode() & 0o077 != 0 {
            return Err(Error::AuthInsecurePermissions {
                path: path.display().to_string(),
                mode: "unsafe".into(),
            });
        }
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(file)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(error(e)),
        }
    }

    /// Read `path`, which must be private to its owner. `None` if it does not
    /// exist.
    pub fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
        let read_error = |source| Error::AuthStoreRead {
            path: path.display().to_string(),
            source,
        };
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(read_error(error)),
        };
        // Checked on the open handle, not the path, so the file cannot be
        // swapped for another between the check and the read.
        let mode = file.metadata().map_err(read_error)?.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(Error::AuthInsecurePermissions {
                path: path.display().to_string(),
                mode: format!("{mode:03o}"),
            });
        }
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(super::MAX_STORE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| Error::AuthStoreRead {
                path: path.display().to_string(),
                source,
            })?;
        if bytes.len() > super::MAX_STORE_BYTES {
            return Err(Error::AuthStoreParse {
                path: path.display().to_string(),
                reason: "credential file is too large".into(),
            });
        }
        Ok(Some(bytes))
    }

    /// Replace `path` with `bytes`, atomically, private to its owner.
    pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
        let write_error = |source| Error::AuthStoreWrite {
            path: path.display().to_string(),
            source,
        };
        if let Some(dir) = path.parent() {
            // `0700`: the directory's listing is not secret, but a private
            // directory keeps the temporary file private too.
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(write_error)?;
        }
        // Written beside the target, so the final rename never crosses a
        // filesystem and stays atomic.
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{}.tmp", std::process::id()));
        let temporary = path.with_file_name(name);
        // A leftover from a crashed run would make `create_new` fail forever.
        let _ = fs::remove_file(&temporary);

        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                // `create_new` refuses to follow a link someone planted at the
                // temporary name, and `mode` means the file is never
                // world-readable, not even briefly.
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(bytes)?;
            // Flushed before the rename: a power cut must not leave the new
            // name pointing at an empty file.
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            // The host ID and rotating refresh token must survive a directory-entry loss.
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        written.map_err(|source| {
            let _ = fs::remove_file(&temporary);
            write_error(source)
        })
    }

    /// Delete `path`. Already gone is fine.
    pub fn remove(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::AuthStoreWrite {
                path: path.display().to_string(),
                source,
            }),
        }
    }
}

/// Without Unix permissions Bee cannot make a file private, so it keeps none.
#[cfg(not(unix))]
mod platform {
    use std::path::Path;

    use crate::error::{Error, Result};

    pub fn open_lock(_path: &Path) -> Result<Option<std::fs::File>> {
        Err(Error::AuthStoreUnsupported)
    }

    pub fn read_private(_path: &Path) -> Result<Option<Vec<u8>>> {
        Err(Error::AuthStoreUnsupported)
    }

    pub fn write_private(_path: &Path, _bytes: &[u8]) -> Result<()> {
        Err(Error::AuthStoreUnsupported)
    }

    pub fn remove(_path: &Path) -> Result<()> {
        Err(Error::AuthStoreUnsupported)
    }
}

/// The current time, in Unix seconds.
#[must_use]
pub fn unix_now() -> u64 {
    // A clock before 1970 is a broken clock: zero makes nothing look expired,
    // which is the safe direction (the provider rejects a dead key anyway).
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Find the credential `profile`'s requests carry.
///
/// `Ok(None)` means the profile has no `credential` and is meant to be
/// unauthenticated, as a local endpoint is. A credential that is named but
/// missing is an error, not `None`: silently sending the request without it
/// would turn a setup mistake into a confusing 401, or a request that leaks
/// that no key was used.
///
/// This synchronous lookup does not refresh. Production ChatGPT requests use
/// [`chatgpt::resolve`] before calling this fixed-credential seam (ADR-020).
///
/// # Errors
///
/// [`Error::AuthMissingEnv`], [`Error::AuthNotLoggedIn`], [`Error::AuthExpired`],
/// or a store error.
pub fn resolve(
    profile: &Profile,
    env: &Environment,
    store: &dyn CredentialStore,
    now: u64,
) -> Result<Option<Credential>> {
    match profile.credential() {
        Some(CredentialSource::ApplicationDefault) => google_cloud::identity(profile, env),
        None => Ok(None),
        Some(CredentialSource::Env(variable)) => {
            let missing = |variable: &str| Error::AuthMissingEnv {
                profile: profile.name().to_owned(),
                variable: variable.into(),
            };
            let secret = env.variable(variable).ok_or_else(|| missing(variable))?;
            let fields = profile
                .credential_env_fields()
                .iter()
                .map(|(field, variable)| {
                    env.variable(variable)
                        .map(|value| (field.clone(), Secret::new(value)))
                        .ok_or_else(|| missing(variable))
                })
                .collect::<Result<_>>()?;
            let credential = Credential::api_key_fields(Secret::new(secret), fields);
            credential.validate_for(profile)?;
            Ok(Some(credential))
        }
        Some(CredentialSource::Store { .. }) => {
            let credential = store
                .get(Audience::Provider(profile.name()))?
                .ok_or_else(|| Error::AuthNotLoggedIn {
                    profile: profile.name().to_owned(),
                })?;
            credential.validate_for(profile)?;
            if let Some(registration) = &credential.oauth {
                if !profile
                    .login_spec()
                    .is_some_and(crate::provider::signin::LoginSpec::is_chatgpt)
                {
                    return Err(chatgpt::failure(
                        "OAuth credentials require a ChatGPT profile",
                    ));
                }
                registration.check(&credential)?;
            } else if profile
                .login_spec()
                .is_some_and(crate::provider::signin::LoginSpec::is_chatgpt)
            {
                return Err(chatgpt::failure(
                    "stored API keys cannot authenticate a ChatGPT profile",
                ));
            }
            if credential.is_expired(now) {
                return Err(Error::AuthExpired {
                    profile: profile.name().to_owned(),
                });
            }
            Ok(Some(credential))
        }
    }
}

/// The default header that presents `secret` to an endpoint speaking `protocol`.
///
/// The credential source does not change this default. Native authentication
/// modes can override it; Copilot requests use bearer authentication even for Messages.
#[must_use]
pub fn header_for(protocol: Protocol, secret: &Secret) -> (&'static str, Secret) {
    match protocol {
        Protocol::OpenAiChat | Protocol::OpenAiResponses => (
            "Authorization",
            Secret::new(format!("Bearer {}", secret.expose())),
        ),
        Protocol::AnthropicMessages => ("x-api-key", secret.clone()),
    }
}

/// Where a profile's credential stands, without revealing it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Status {
    /// ADC is owned by Google tooling/deployment; status does not acquire tokens.
    ApplicationDefault,
    /// No `credential`: an unauthenticated endpoint.
    Unauthenticated,
    /// Read from a variable.
    Env {
        /// The variable's name.
        variable: String,
        /// Whether it is set to something.
        set: bool,
    },
    /// Kept in the credential store.
    Store {
        /// What the store holds for the profile.
        state: StoreState,
        /// The browser sign-in `bee auth login` can run for it, if any.
        login: Option<String>,
    },
}

/// What the store holds for a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreState {
    /// Identity exists but ChatGPT-plan permission was declined.
    PlanDisabled,
    /// The access token can be renewed without repeating browser authorization.
    Refreshable,
    /// A credential that works.
    LoggedIn,
    /// Nothing.
    NotLoggedIn,
    /// A credential past its expiry.
    Expired,
}

/// One profile's [`Status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileStatus {
    /// The profile's name.
    pub profile: String,
    /// Whether it is the default profile.
    pub default: bool,
    /// Where its credential stands.
    pub status: Status,
}

/// The [`Status`] of every profile.
///
/// # Errors
///
/// A store error, if the credential file is unreadable or not private.
pub fn status(
    providers: &Providers,
    env: &Environment,
    store: &dyn CredentialStore,
    now: u64,
) -> Result<Vec<ProfileStatus>> {
    let mut statuses = Vec::new();
    for name in providers.names() {
        let Some(profile) = providers.get(name) else {
            continue;
        };
        let status = match profile.credential() {
            Some(CredentialSource::ApplicationDefault) => Status::ApplicationDefault,
            None => Status::Unauthenticated,
            Some(CredentialSource::Env(variable)) => Status::Env {
                variable: variable.clone(),
                set: env.variable(variable).is_some(),
            },
            Some(CredentialSource::Store { login }) => Status::Store {
                state: match store.get(Audience::Provider(name))? {
                    None => StoreState::NotLoggedIn,
                    Some(credential) if credential.oauth.is_some() => {
                        chatgpt::store_state(&credential, now)
                    }
                    Some(credential) if credential.is_expired(now) => StoreState::Expired,
                    Some(_) => StoreState::LoggedIn,
                },
                login: login.clone(),
            },
        };
        statuses.push(ProfileStatus {
            profile: name.to_owned(),
            default: providers
                .configured_model()
                .is_some_and(|model| model.account == name),
            status,
        });
    }
    Ok(statuses)
}

/// Authentication can address configured accounts outside active profile membership.
///
/// # Errors
///
/// Returns configuration/profile validation errors before exposing an auth target.
pub fn configured_accounts(config: &crate::config::Config) -> Result<Providers> {
    let mut accounts = Providers::from_config(config)?;
    // Validate the ordinary scoped view first; auth must not bypass invalid profile configuration.
    accounts.accounts = Providers::connections(config)?;
    Ok(accounts)
}

/// The profile `bee auth` was asked about.
///
/// # Errors
///
/// [`Error::AuthUnknownProfile`] if there is none of that name.
pub fn profile<'a>(providers: &'a Providers, name: &str) -> Result<&'a Profile> {
    providers
        .get(name)
        .ok_or_else(|| Error::AuthUnknownProfile {
            profile: name.to_owned(),
            available: if providers.is_empty() {
                "none".to_owned()
            } else {
                providers.names().collect::<Vec<_>>().join(", ")
            },
        })
}

/// Check that `profile` keeps its credential in the store.
///
/// # Errors
///
/// [`Error::AuthNotStored`] if it does not.
pub fn require_store(profile: &Profile) -> Result<()> {
    if profile.google_cloud().is_some() {
        return Err(google_cloud::failure(
            "ADC is managed externally; use gcloud auth application-default login/revoke or configure an attached service account",
        ));
    }
    if matches!(profile.credential(), Some(CredentialSource::Store { .. })) {
        Ok(())
    } else {
        Err(Error::AuthNotStored {
            profile: profile.name().to_owned(),
        })
    }
}

/// Store `secret` as `profile`'s credential, replacing any previous one.
///
/// # Errors
///
/// [`Error::AuthNotStored`] if the profile does not use the store,
/// [`Error::AuthEmptyToken`] if `secret` is blank, or a store error.
pub fn store_secret(profile: &Profile, secret: &str, store: &dyn CredentialStore) -> Result<()> {
    require_store(profile)?;
    let generation = store.generation(Audience::Provider(profile.name()))?;
    store_secret_guarded(profile, secret, store, generation)
}

/// Publish an imported key only if login/logout has not superseded its acquisition.
///
/// # Errors
/// Returns the validation/store errors of [`store_secret`], or a changed-generation error.
pub fn store_secret_guarded(
    profile: &Profile,
    secret: &str,
    store: &dyn CredentialStore,
    generation: Option<u64>,
) -> Result<()> {
    require_store(profile)?;
    if profile
        .login_spec()
        .is_some_and(crate::provider::signin::LoginSpec::is_chatgpt)
    {
        return Err(chatgpt::failure("use browser sign-in for this profile"));
    }
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(Error::AuthEmptyToken);
    }
    let credential = Credential::new(Secret::new(secret)).for_provider(profile.provider());
    credential.validate_for(profile)?;
    store.put_guarded(Audience::Provider(profile.name()), &credential, generation)
}

/// Forget `profile`'s stored credential. `false` if there was none.
///
/// Only Bee's own copy: an environment variable is the user's, and Bee never
/// touches it. The key itself stays valid with the provider; revoking it is
/// done there.
///
/// # Errors
///
/// [`Error::AuthNotStored`] if the profile does not use the store, or a store
/// error.
pub fn forget(profile: &Profile, store: &dyn CredentialStore) -> Result<bool> {
    require_store(profile)?;
    store.remove(Audience::Provider(profile.name()))
}
