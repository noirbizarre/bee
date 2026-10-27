//! Profile-scoped recent model choices, separate from configuration and sessions.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::CredentialStore;
use crate::provider::{Profile, Providers};

/// Private correlation data must never become diagnostic or protocol metadata.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reference {
    pub(crate) connection: String,
    pub(crate) model: String,
    binding: String,
    #[serde(default)]
    fallback: bool,
}

impl std::fmt::Debug for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ModelReference(<redacted>)")
    }
}

/// Session-owned safety state survives restart and explicit-setting rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) enum State {
    #[default]
    Explicit,
    Automatic(Reference),
    Blocked(Reason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Reason {
    ConnectionRemoved,
    CredentialsUnavailable,
    AccountChanged,
    ModelUnavailable,
}

impl Reason {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::ConnectionRemoved => {
                "The recent model's connection is outside this profile. Choose a provider and model explicitly."
            }
            Self::CredentialsUnavailable => {
                "The recent model's credentials are unavailable or authorization was rejected. Sign in and choose a provider/model explicitly."
            }
            Self::AccountChanged => {
                "The recent model's account or connection target changed. Choose a provider and model explicitly to confirm the account."
            }
            Self::ModelUnavailable => {
                "The recent model is no longer listed for this account. Choose an available model explicitly."
            }
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    profiles: BTreeMap<String, Reference>,
}

/// Injectable recent-choice storage. Clones share memory; independent defaults do not.
#[derive(Clone, Default)]
pub struct Store {
    memory: Arc<Mutex<BTreeMap<String, Reference>>>,
    transaction: Arc<Mutex<()>>,
    root: Option<PathBuf>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SelectionStore(<redacted>)")
    }
}

impl Store {
    /// Open private durable preferences beside the production session directory.
    ///
    /// # Errors
    /// Returns a path-only history diagnostic for unreadable or invalid state.
    pub fn open(history: &crate::history::Store) -> Result<Self, crate::history::Error> {
        let root = history.root.clone();
        let store = Self {
            root: Some(root),
            ..Self::default()
        };
        let document = store.read()?;
        *store.memory.lock().expect("selection lock") = document.profiles;
        Ok(store)
    }

    pub(crate) fn get(&self, profile: &str) -> Option<Reference> {
        self.memory
            .lock()
            .expect("selection lock")
            .get(profile)
            .cloned()
    }

    pub(crate) fn remember(
        &self,
        profile: &str,
        reference: Reference,
    ) -> Result<(), crate::history::Error> {
        let path = self.root.as_ref().map_or_else(
            || PathBuf::from("<model preferences>"),
            |root| root.join("model-preferences.json"),
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        // Bound both local and cross-process contention: recency cannot hold a completed answer busy indefinitely.
        let transaction = wait_lock(&path, deadline, || match self.transaction.try_lock() {
            Ok(guard) => Ok(Some(guard)),
            Err(std::sync::TryLockError::WouldBlock) => Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                Err(failure(&path, "recent model transaction failed"))
            }
        });
        // Keep recency usable in this runtime even if the best-effort disk transaction fails.
        self.memory
            .lock()
            .expect("selection lock")
            .insert(profile.into(), reference.clone());
        let _transaction = transaction?;
        let Some(root) = &self.root else {
            return Ok(());
        };
        let lock_path = root.join("model-preferences.lock");
        let file = match crate::history::open_private_file(&lock_path, true, true) {
            Ok(file) => file,
            Err(_) => crate::history::open_private_file(&lock_path, false, true)?,
        };
        wait_lock(&path, deadline, || match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(())),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || (error.raw_os_error().is_some()
                        && error.raw_os_error() == fs2::lock_contended_error().raw_os_error()) =>
            {
                Ok(None)
            }
            Err(_) => Err(failure(&path, "cannot lock recent model state")),
        })?;
        // Merge under a stable inode lock: renaming the data file cannot lose another profile's update.
        let mut document = self.read()?;
        document.version = 1;
        document.profiles.insert(profile.into(), reference);
        let bytes = serde_json::to_vec(&document)
            .map_err(|_| failure(&path, "cannot encode recent model state"))?;
        if bytes.len() > 1024 * 1024 {
            return Err(failure(&path, "recent model state exceeds 1 MiB"));
        }
        let mut staged = tempfile::NamedTempFile::new_in(root)
            .map_err(|_| failure(&path, "cannot stage recent model state"))?;
        // The root already carries the private ACL; retain the existing file's private permissions.
        if path.exists() {
            crate::history::check_private(&path, false)?;
            staged
                .as_file()
                .set_permissions(
                    std::fs::metadata(&path)
                        .map_err(|_| failure(&path, "cannot inspect recent model state"))?
                        .permissions(),
                )
                .map_err(|_| failure(&path, "cannot preserve recent model permissions"))?;
        }
        staged
            .write_all(&bytes)
            .and_then(|()| staged.as_file().sync_all())
            .map_err(|_| failure(&path, "cannot sync recent model state"))?;
        crate::history::check_private(staged.path(), false)?;
        crate::history::replace_file(staged.path(), &path)
            .map_err(|_| failure(&path, "cannot publish recent model state"))?;
        crate::history::sync_directory(root)
    }

    fn read(&self) -> Result<Document, crate::history::Error> {
        let path = self
            .root
            .as_ref()
            .expect("durable store")
            .join("model-preferences.json");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Document {
                    version: 1,
                    ..Document::default()
                });
            }
            Err(_) => return Err(failure(&path, "cannot inspect recent model state")),
            Ok(_) => {}
        }
        let file = crate::history::open_private_file(&path, false, false)?;
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| failure(&path, "cannot read recent model state"))?;
        if bytes.len() > 1024 * 1024 {
            return Err(failure(&path, "recent model state exceeds 1 MiB"));
        }
        let document: Document = serde_json::from_slice(&bytes)
            .map_err(|_| failure(&path, "invalid recent model state"))?;
        if document.version != 1 {
            return Err(failure(&path, "unsupported recent model state version"));
        }
        if document
            .profiles
            .iter()
            .any(|(name, r)| name.trim().is_empty() || !r.valid())
        {
            return Err(failure(&path, "invalid recent model reference"));
        }
        Ok(document)
    }
}

fn failure(path: &Path, reason: &'static str) -> crate::history::Error {
    crate::history::Error {
        path: path.into(),
        reason,
        help: "check private state-directory permissions and the model-preferences.json version; repair or remove invalid preference state, then restart Bee",
    }
}

fn wait_lock<T>(
    path: &Path,
    deadline: Instant,
    mut attempt: impl FnMut() -> Result<Option<T>, crate::history::Error>,
) -> Result<T, crate::history::Error> {
    loop {
        if let Some(guard) = attempt()? {
            return Ok(guard);
        }
        if Instant::now() >= deadline {
            return Err(failure(path, "recent model state is busy; retry later"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Reference {
    fn valid(&self) -> bool {
        !self.connection.trim().is_empty()
            && crate::provider::check_model_id(&self.model).is_ok()
            && self.binding.len() == 64
            && self.binding.bytes().all(|byte| byte.is_ascii_hexdigit())
    }
    pub(crate) fn fallback_notice(&self) -> Option<&'static str> {
        self.fallback.then_some("The recent model is no longer listed for this account. Using the configured default on the same connection/account; select a model explicitly to change it.")
    }
    pub(crate) fn new(
        profile: &Profile,
        model: String,
        credential: Option<&crate::auth::Credential>,
    ) -> Self {
        // Length-framed JSON avoids ambiguous concatenations; defaults/metadata do not change account identity.
        let targets: Vec<_> = profile
            .endpoints()
            .iter()
            .map(|endpoint| (endpoint.protocol(), endpoint.base_url()))
            .collect();
        let identity = credential.map(crate::auth::Credential::selection_identity);
        let bytes = serde_json::to_vec(&(targets, profile.headers(), identity))
            .expect("serializable identity");
        let mut digest = Sha256::new();
        digest.update(b"bee-model-account-v1\0");
        if profile.is_copilot() {
            // The same token/URL configured as a generic endpoint must not inherit Copilot authority.
            digest.update(b"github-copilot\0");
        }
        if let Some(target) = profile.google_cloud() {
            // Resource routing and quota attribution are authority even before token acquisition.
            digest.update(b"google-cloud\0");
            digest.update(
                serde_json::to_vec(&(&target.project, &target.location, &target.quota_project))
                    .expect("serializable Cloud target"),
            );
        }
        digest.update(bytes);
        let binding = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            connection: profile.name().into(),
            model,
            binding,
            fallback: false,
        }
    }

    pub(crate) fn matches(
        &self,
        profile: &Profile,
        credential: Option<&crate::auth::Credential>,
    ) -> bool {
        self.binding == Self::new(profile, self.model.clone(), credential).binding
    }
}

impl State {
    pub(crate) fn valid(&self, provider: Option<&str>, model: Option<&str>) -> bool {
        match self {
            Self::Explicit => true,
            Self::Blocked(_) => model.is_none(),
            Self::Automatic(reference) => {
                reference.valid()
                    && provider == Some(&reference.connection)
                    && model == Some(&reference.model)
            }
        }
    }
}

pub(crate) fn credential(
    profile: &Profile,
    env: &crate::config::Environment,
) -> Result<Option<crate::auth::Credential>, Reason> {
    let store = match profile.credential() {
        Some(crate::provider::CredentialSource::Store { .. }) => {
            crate::auth::FileStore::from_environment(env)
                .map_err(|_| Reason::CredentialsUnavailable)?
        }
        _ => crate::auth::FileStore::new(""),
    };
    // Read stable OAuth registration without refreshing here; prompt authentication owns network renewal.
    if profile
        .login_spec()
        .is_some_and(crate::preset::LoginSpec::is_chatgpt)
    {
        let credential = store
            .get(crate::auth::Audience::Provider(profile.name()))
            .map_err(|_| Reason::CredentialsUnavailable)?
            .ok_or(Reason::CredentialsUnavailable)?;
        if !credential.selection_oauth_usable() {
            return Err(Reason::CredentialsUnavailable);
        }
        return Ok(Some(credential));
    }
    crate::auth::resolve(profile, env, &store, crate::auth::unix_now())
        .map_err(|_| Reason::CredentialsUnavailable)
}

pub(crate) fn unavailable(
    snapshot: &crate::models::Snapshot,
    profile: &Profile,
    model: &str,
) -> Option<Reason> {
    let listing = snapshot.endpoints.get(profile.name())?;
    if listing.error == Some(crate::models::Error::Unauthorized) {
        return Some(Reason::CredentialsUnavailable);
    }
    // Only current complete account listings constrain automatic reuse, never explicit/custom selections.
    if listing.error.is_none()
        && !listing.stale
        && listing
            .fetched_at
            .is_some_and(|at| crate::auth::unix_now().saturating_sub(at) <= 300)
        && !listing.models.iter().any(|id| id == model)
    {
        return Some(Reason::ModelUnavailable);
    }
    None
}

pub(crate) fn resolve(
    reference: &Reference,
    providers: &Providers,
    env: &crate::config::Environment,
    discovery: Option<&crate::models::Service>,
) -> Result<Reference, Reason> {
    let profile = providers
        .get(&reference.connection)
        .ok_or(Reason::ConnectionRemoved)?;
    let credential = credential(profile, env)?;
    if profile.google_cloud().is_some() && credential.is_none() {
        // Metadata identity requires cancellable I/O. Prompt preparation must validate
        // this tentative reference against the resolved principal before any inference.
        return Ok(reference.clone());
    }
    let snapshot = discovery
        .map(|service| service.account_snapshot(profile, credential.as_ref()))
        .unwrap_or_default();
    resolve_visible(
        reference,
        providers,
        profile,
        credential.as_ref(),
        &snapshot,
    )
}

fn resolve_visible(
    reference: &Reference,
    providers: &Providers,
    profile: &Profile,
    credential: Option<&crate::auth::Credential>,
    snapshot: &crate::models::Snapshot,
) -> Result<Reference, Reason> {
    if !reference.matches(profile, credential) {
        return Err(Reason::AccountChanged);
    }
    if let Some(reason) = unavailable(snapshot, profile, &reference.model) {
        if reason == Reason::ModelUnavailable
            && providers.default_name() == Some(profile.name())
            && let Some(model) = profile.default_model()
            && unavailable(snapshot, profile, model).is_none()
        {
            let mut fallback = Reference::new(profile, model.into(), credential);
            fallback.fallback = true;
            return Ok(fallback);
        }
        return Err(reason);
    }
    Ok(reference.clone())
}

pub(crate) fn authentication_failure(
    reference: &Reference,
    profile: &Profile,
    store: &dyn CredentialStore,
) -> Option<Reason> {
    match store.get(crate::auth::Audience::Provider(profile.name())) {
        Ok(Some(current)) if !reference.matches(profile, Some(&current)) => {
            Some(Reason::AccountChanged)
        }
        Ok(Some(current)) if !current.selection_oauth_usable() => {
            Some(Reason::CredentialsUnavailable)
        }
        Ok(None) => Some(Reason::CredentialsUnavailable),
        // An unchanged renewable registration can retry a temporary renewal or store-lock failure.
        Ok(Some(_)) | Err(_) => None,
    }
}
