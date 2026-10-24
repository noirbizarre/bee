//! Provider credentials, free of the ACPv2 SDK.
//!
//! A profile's `credential` (see [`crate::provider::CredentialSource`]) says
//! *where* its secret is; this module finds it, keeps it, and makes sure it
//! never shows up in output. The pieces:
//!
//! - [`Secret`]: a string that cannot be printed by accident.
//! - [`CredentialStore`] and [`FileStore`]: where `bee auth login` keeps what
//!   it obtains, in a private file.
//! - [`resolve`]: from a profile to the secret its requests carry.
//! - [`header_for`]: how a protocol presents that secret.
//!
//! Authentication is configured apart from the protocol and the models: a
//! profile may use any protocol with any source, and the protocol only decides
//! how the resolved secret is *presented*. LLM provider credentials and MCP
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

use crate::config::Environment;
use crate::error::{Error, Result};
use crate::provider::{CredentialSource, Profile, Protocol, Providers};

/// The version of the credential file's layout.
const STORE_VERSION: u64 = 1;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    secret: Secret,
    /// Seconds since the Unix epoch, for credentials that expire.
    expires_at: Option<u64>,
}

impl Credential {
    /// A credential that does not expire.
    #[must_use]
    pub fn new(secret: Secret) -> Self {
        Self {
            secret,
            expires_at: None,
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
    providers: BTreeMap<String, Credential>,
    mcp_servers: BTreeMap<String, Credential>,
}

impl Document {
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
        self.providers.is_empty() && self.mcp_servers.is_empty()
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
        // `Value`, not a derived type: serde's type errors quote the offending
        // value, and this file is full of secrets. Syntax errors only say what
        // was expected, and are reduced to a category and a position anyway.
        let root: Value = serde_json::from_slice(&bytes).map_err(|error| {
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
        if root
            .keys()
            .any(|key| !matches!(key.as_str(), "version" | "providers" | "mcp_servers"))
        {
            return Err(self.parse_error("it has a key Bee does not write"));
        }
        if root.get("version").and_then(Value::as_u64) != Some(STORE_VERSION) {
            return Err(self.parse_error("its `version` is missing or not one Bee understands"));
        }
        Ok(Document {
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
            if entry
                .keys()
                .any(|key| !matches!(key.as_str(), "secret" | "expires_at"))
            {
                return Err(self.parse_error(format!(
                    "an entry of `{table}` has a key Bee does not write"
                )));
            }
            let secret = match entry.get("secret") {
                Some(Value::String(secret)) if !secret.is_empty() => Secret::new(secret.as_str()),
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
            credentials.insert(name.clone(), Credential { secret, expires_at });
        }
        Ok(credentials)
    }

    fn save(&self, document: &Document) -> Result<()> {
        if document.is_empty() {
            // The last credential is gone: leave nothing behind, not a file of
            // empty tables that still looks like it holds secrets.
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
                        (name.clone(), Value::Object(entry))
                    })
                    .collect(),
            )
        };
        let mut root = Map::new();
        root.insert("version".to_owned(), Value::from(STORE_VERSION));
        root.insert("providers".to_owned(), table(&document.providers));
        root.insert("mcp_servers".to_owned(), table(&document.mcp_servers));
        let mut bytes = serde_json::to_vec_pretty(&Value::Object(root))
            .map_err(|_| self.parse_error("the credentials could not be serialized"))?;
        bytes.push(b'\n');
        platform::write_private(&self.path, &bytes)
    }
}

impl CredentialStore for FileStore {
    fn get(&self, audience: Audience<'_>) -> Result<Option<Credential>> {
        Ok(self.load()?.table(audience).get(&audience.name()).cloned())
    }

    fn put(&self, audience: Audience<'_>, credential: &Credential) -> Result<()> {
        let mut document = self.load()?;
        document
            .table_mut(audience)
            .insert(audience.name(), credential.clone());
        self.save(&document)
    }

    fn remove(&self, audience: Audience<'_>) -> Result<bool> {
        let mut document = self.load()?;
        let removed = document
            .table_mut(audience)
            .remove(&audience.name())
            .is_some();
        if removed {
            self.save(&document)?;
        }
        Ok(removed)
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
        file.read_to_end(&mut bytes)
            .map_err(|source| Error::AuthStoreRead {
                path: path.display().to_string(),
                source,
            })?;
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
            fs::rename(&temporary, path)
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
/// Nothing is refreshed: the only browser sign-in Bee has returns a key that
/// does not expire, and a credential that stopped working is reported with the
/// command that replaces it (ADR-008).
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
        None => Ok(None),
        Some(CredentialSource::Env(variable)) => env
            .variable(variable)
            .map(|value| Some(Credential::new(Secret::new(value))))
            .ok_or_else(|| Error::AuthMissingEnv {
                profile: profile.name().to_owned(),
                variable: variable.clone(),
            }),
        Some(CredentialSource::Store { .. }) => {
            let credential = store
                .get(Audience::Provider(profile.name()))?
                .ok_or_else(|| Error::AuthNotLoggedIn {
                    profile: profile.name().to_owned(),
                })?;
            if credential.is_expired(now) {
                return Err(Error::AuthExpired {
                    profile: profile.name().to_owned(),
                });
            }
            Ok(Some(credential))
        }
    }
}

/// The header that presents `secret` to an endpoint speaking `protocol`.
///
/// The protocol decides this, not the credential source: a key from the
/// environment and one from a browser sign-in are presented identically.
#[must_use]
pub fn header_for(protocol: Protocol, secret: &Secret) -> (&'static str, Secret) {
    match protocol {
        Protocol::OpenAiChat => (
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
            None => Status::Unauthenticated,
            Some(CredentialSource::Env(variable)) => Status::Env {
                variable: variable.clone(),
                set: env.variable(variable).is_some(),
            },
            Some(CredentialSource::Store { login }) => Status::Store {
                state: match store.get(Audience::Provider(name))? {
                    None => StoreState::NotLoggedIn,
                    Some(credential) if credential.is_expired(now) => StoreState::Expired,
                    Some(_) => StoreState::LoggedIn,
                },
                login: login.clone(),
            },
        };
        statuses.push(ProfileStatus {
            profile: name.to_owned(),
            default: providers.default_name() == Some(name),
            status,
        });
    }
    Ok(statuses)
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
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(Error::AuthEmptyToken);
    }
    store.put(
        Audience::Provider(profile.name()),
        &Credential::new(Secret::new(secret)),
    )
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
