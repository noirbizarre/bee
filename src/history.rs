//! Private durable sessions, independent of the ACP SDK.
//!
//! A versioned JSONL journal is the sole source of truth. Each attached session
//! has one ordered worker and an OS ownership lock; blocking file I/O never runs
//! on the protocol reactor. Future indexes/checkpoints can remain rebuildable.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[path = "history/journal.rs"]
mod journal;
#[cfg(windows)]
#[path = "history/windows.rs"]
mod windows;

use journal::{Event, Journal, Outcome};

/// A sanitized failure: parsing and filesystem errors must not quote history.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
#[error("session history at `{path}`: {reason}; {help}")]
#[diagnostic(code(bee::history::storage))]
pub struct Error {
    /// Location, never stored contents.
    pub path: PathBuf,
    /// Fixed failure classification.
    pub reason: &'static str,
    /// How to recover without silently discarding history.
    pub help: &'static str,
}

type Result<T> = std::result::Result<T, Error>;

fn error(path: &Path, reason: &'static str) -> Error {
    Error {
        path: path.to_owned(),
        reason,
        help: "check the history directory and available disk space, then reconnect",
    }
}

/// Replay policy, separate from unlimited on-disk transcript retention.
#[derive(Clone, Copy)]
pub struct Settings {
    replay_max_bytes: usize,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HistorySettings([redacted])")
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            replay_max_bytes: 64 * 1024 * 1024,
        }
    }
}

impl Settings {
    /// Parse history-owned settings with leaf provenance and redacted errors.
    pub fn from_config(config: &crate::config::Config) -> crate::Result<Self> {
        let mut settings = Self::default();
        let failure = |key: &str, reason| crate::Error::HistorySetting {
            path: config.origin(key).map_or_else(
                || "<configuration>".into(),
                |path| path.display().to_string(),
            ),
            reason,
        };
        if let Some(value) = config.get("history") {
            let table = value
                .as_table()
                .ok_or_else(|| failure("history", "must be a table"))?;
            for (key, value) in table {
                if key != "replay_max_bytes" {
                    return Err(failure("history", "contains an unknown setting"));
                }
                settings.replay_max_bytes = value
                    .as_integer()
                    .and_then(|value| usize::try_from(value).ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        failure(
                            "history.replay_max_bytes",
                            "requires a positive replay byte budget",
                        )
                    })?;
            }
        }
        Ok(settings)
    }
}

/// The SDK has no delivery acknowledgement. Refunding bytes merely because they
/// entered its unbounded sender would let repeated resumes multiply stalled
/// output. Only a fresh connection resets this cumulative reservation.
pub(crate) struct ReplayBudget(AtomicUsize);

impl ReplayBudget {
    pub(crate) fn new(settings: Settings) -> Self {
        Self(AtomicUsize::new(settings.replay_max_bytes))
    }

    fn exhausted() -> Error {
        Error {
            path: "<session replay>".into(),
            reason: "history replay exceeds this connection's remaining byte budget",
            help: "reconnect or increase [history] replay_max_bytes, or resume without replay",
        }
    }

    fn message_bytes(message: &Message) -> Result<usize> {
        // Framing covers the session ID, method and queue bookkeeping absent
        // from this SDK-free DTO, in addition to serialized message contents.
        serde_json::to_vec(message)
            .map_err(|_| Self::exhausted())?
            .len()
            .checked_add(512)
            .ok_or_else(Self::exhausted)
    }

    pub(crate) fn reserve(&self, bytes: usize) -> Result<()> {
        let mut remaining = self.0.load(Ordering::Acquire);
        loop {
            let next = remaining.checked_sub(bytes).ok_or_else(Self::exhausted)?;
            match self
                .0
                .compare_exchange_weak(remaining, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(current) => remaining = current,
            }
        }
    }

    pub(crate) fn reserve_messages(&self, messages: &[Message]) -> Result<()> {
        let mut bytes = 0_usize;
        for message in messages {
            bytes = bytes
                .checked_add(Self::message_bytes(message)?)
                .ok_or_else(Self::exhausted)?;
            if bytes > self.0.load(Ordering::Acquire) {
                return Err(Self::exhausted());
            }
        }
        self.reserve(bytes)
    }
}

/// Restorable core state, excluding connection-owned execution and authority.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    // Missing ownership in pre-profile journals always means default, never
    // whichever profile happens to be selected when the journal is opened.
    #[serde(default = "default_profile")]
    pub(crate) profile: String,
    pub(crate) cwd: PathBuf,
    pub(crate) turns: u64,
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) selection_state: crate::selection::State,
    pub(crate) protocol: Option<crate::provider::Protocol>,
    pub(crate) messages: Vec<crate::llm::Message>,
    pub(crate) instructions: crate::instructions::State,
}

fn default_profile() -> String {
    "default".into()
}

/// An SDK-free upsert. Original user blocks are validated at the ACP boundary.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Message {
    pub(crate) id: String,
    pub(crate) user: bool,
    pub(crate) content: serde_json::Value,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TranscriptMessage([redacted])")
    }
}

pub(crate) struct Listing {
    pub(crate) profile: String,
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    created: u64,
}

/// Durable storage root. Cloning does not attach or authorize any session.
#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) root: PathBuf,
}

impl Store {
    /// Open the private production store before serving protocol traffic.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        private_directory(&root)?;
        private_directory(&root.join("sessions"))?;
        Ok(Self { root })
    }

    /// Resolve the injected XDG state root; absence is an explicit launch error.
    pub fn from_environment(environment: &crate::config::Environment) -> Result<Self> {
        let root = environment.history_path().ok_or_else(|| Error {
            path: "<state directory>".into(),
            reason: "cannot locate durable history",
            help: "set HOME or an absolute XDG_STATE_HOME",
        })?;
        Self::open(root)
    }

    fn directory(&self, id: &str) -> Result<PathBuf> {
        // Request-controlled IDs never become arbitrary filesystem paths.
        if id.len() != 44
            || !id.starts_with("bee-session-")
            || !id[12..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(error(
                &self.root,
                "unknown session ID; select an existing session with session/list",
            ));
        }
        Ok(self.root.join("sessions").join(id))
    }

    pub(crate) async fn create(&self, snapshot: Snapshot) -> Result<(String, Handle)> {
        let store = self.clone();
        blocking(move || {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random)
                .map_err(|_| error(&store.root, "cannot allocate session identity"))?;
            let id = format!(
                "bee-session-{}",
                random
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            let directory = store.directory(&id)?;
            create_directory(&directory)
                .map_err(|_| error(&directory, "cannot create session directory"))?;
            let handle = match open_session(&directory, &id, Some(snapshot)) {
                Ok(handle) => handle,
                Err(failure) => {
                    // Never remove an identity once its complete journal became
                    // visible: another client could already have discovered it.
                    if !directory.join("session.jsonl").exists() {
                        let _ = std::fs::remove_dir_all(&directory);
                    }
                    return Err(failure);
                }
            };
            Ok((id, handle))
        })
        .await
    }

    pub(crate) async fn attach(&self, id: &str) -> Result<(Snapshot, Handle, bool)> {
        let directory = self.directory(id)?;
        let id = id.to_owned();
        blocking(move || open_session(&directory, &id, None))
            .await?
            .load()
            .await
    }

    pub(crate) async fn list(&self) -> Result<Vec<Listing>> {
        let store = self.clone();
        blocking(move || {
            let mut listed = Vec::new();
            for entry in std::fs::read_dir(store.root.join("sessions"))
                .map_err(|_| error(&store.root, "cannot list sessions"))?
            {
                let entry = entry.map_err(|_| error(&store.root, "cannot read session entry"))?;
                let id = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| error(&store.root, "invalid session directory"))?;
                // An interrupted creation was never acknowledged or published.
                if id.starts_with(".pending-bee-session-") {
                    continue;
                }
                let directory = store.directory(&id)?;
                check_private(&directory, true)?;
                let path = directory.join("session.jsonl");
                // Directory creation and its private ownership lock precede the
                // atomic publication of the complete journal. Ignore only a
                // missing file; unreadable/invalid published journals still fail.
                match std::fs::symlink_metadata(&path) {
                    Ok(_) => {}
                    Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => return Err(error(&path, "cannot inspect session journal")),
                }
                let mut reader = journal::Reader::open(&path, u64::MAX)?;
                // Listing reads immutable header metadata only. It neither
                // scans long transcripts nor repairs a live writer's tail.
                let header = reader
                    .next()?
                    .ok_or_else(|| error(&path, "session journal header is unavailable"))?;
                let projection = journal::Projection::header(header, &id, &path)?;
                listed.push(Listing {
                    profile: projection.snapshot.profile,
                    id,
                    cwd: projection.snapshot.cwd,
                    created: projection.created,
                });
            }
            listed.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
            Ok(listed)
        })
        .await
    }
}

#[cfg(not(windows))]
trait PrivateBuilder {
    fn mode_private(&mut self) -> &mut Self;
}
#[cfg(not(windows))]
impl PrivateBuilder for std::fs::DirBuilder {
    fn mode_private(&mut self) -> &mut Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            self.mode(0o700);
        }
        self
    }
}

fn private_directory(path: &Path) -> Result<()> {
    #[cfg(not(any(unix, windows)))]
    return Err(error(
        path,
        "private durable history is unsupported on this platform",
    ));
    #[cfg(any(unix, windows))]
    {
        // Persist newly created ancestors too; syncing the catalog alone cannot
        // durably publish a freshly created XDG root after power loss.
        let mut missing = Vec::new();
        let mut ancestor = path;
        loop {
            match std::fs::symlink_metadata(ancestor) {
                Ok(_) => break,
                Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(ancestor.to_owned());
                    ancestor = ancestor
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or(Path::new("."));
                }
                Err(_) => return Err(error(path, "cannot inspect history directory")),
            }
        }
        for directory in missing.into_iter().rev() {
            match create_directory(&directory) {
                Ok(()) => {}
                Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(error(&directory, "cannot create private history directory")),
            }
            sync_directory(
                directory
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?;
        }
        check_private(path, true)
    }
}

fn create_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    return windows::create_directory(path);
    #[cfg(not(windows))]
    std::fs::DirBuilder::new().mode_private().create(path)
}

pub(crate) fn check_private(path: &Path, directory: bool) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| error(path, "history is unavailable"))?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(error(path, "history path has an unexpected file type"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 {
            return Err(Error {
                path: path.to_owned(),
                reason: "history permissions allow other users",
                help: "restrict session directories to mode 0700 and files to mode 0600, then reconnect",
            });
        }
    }
    #[cfg(windows)]
    windows::check_private(path, directory).map_err(|_| {
        error(
            path,
            "history ACL must grant full control only to the current user",
        )
    })?;
    Ok(())
}

fn open_options() -> OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
        options
    }
    #[cfg(not(unix))]
    OpenOptions::new()
}

pub(crate) fn open_private_file(path: &Path, create: bool, write: bool) -> Result<File> {
    if !create {
        check_private(path, false)?;
    }
    let file = open_options()
        .read(true)
        .write(write)
        .append(write)
        .create_new(create)
        .open(path)
        .map_err(|_| error(path, "cannot open session journal"))?;
    check_private(path, false)?;
    Ok(file)
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    // Windows has no unprivileged directory-fsync equivalent; publication below
    // uses a write-through rename, and journal commits still use FlushFileBuffers.
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| error(path, "cannot synchronize session directory"))
}

fn publish_file(staging: &Path, published: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    return windows::publish_file(staging, published);
    #[cfg(not(windows))]
    std::fs::rename(staging, published)
}

pub(crate) fn replace_file(staging: &Path, published: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    return windows::replace_file(staging, published);
    #[cfg(not(windows))]
    std::fs::rename(staging, published)
}

enum Job {
    Run(Box<dyn FnOnce(&mut Journal) + Send>),
    Release(tokio::sync::oneshot::Sender<Result<()>>),
    Text {
        turn: u64,
        text: String,
        emit: Box<dyn FnOnce() + Send>,
        worker: std::sync::Weak<Worker>,
        bytes: usize,
    },
}

struct Worker {
    sender: mpsc::Sender<Job>,
    path: PathBuf,
    queued: AtomicUsize,
    failure: Mutex<Option<Error>>,
    changed: tokio::sync::Notify,
}

/// The last attachment closes the journal before releasing its ownership lock.
#[derive(Clone)]
pub(crate) struct Handle(Arc<Worker>);

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryHandle")
            .field("path", &self.0.path)
            .finish_non_exhaustive()
    }
}

fn open_session(directory: &Path, id: &str, snapshot: Option<Snapshot>) -> Result<Handle> {
    check_private(directory, true)?;
    let lock_path = directory.join("owner.lock");
    let owner = open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|_| error(&lock_path, "cannot open session ownership lock"))?;
    check_private(&lock_path, false)?;
    owner.try_lock_exclusive().map_err(|_| Error {
        path: directory.to_owned(),
        reason: "session is owned by another Bee connection",
        help: "close it in that client or wait for that Bee process to exit, then resume",
    })?;
    let path = directory.join("session.jsonl");
    if let Some(snapshot) = snapshot {
        let staging = directory.join(".session.jsonl.pending");
        let journal = Journal::create(&staging, id, snapshot)?;
        drop(journal);
        if path.exists() {
            return Err(error(&path, "session identity already exists"));
        }
        // Windows cannot rename the directory containing our open ownership
        // handle. Publishing a closed journal file within that stable directory
        // preserves both atomic visibility and uninterrupted exclusive ownership.
        publish_file(&staging, &path)
            .map_err(|_| error(&path, "cannot publish session identity"))?;
        sync_directory(directory)?;
        sync_directory(directory.parent().expect("session catalog parent"))?;
    }
    let mut journal = Journal::open(&path, id)?;
    let (sender, receiver) = mpsc::channel::<Job>();
    let worker = Arc::new(Worker {
        sender,
        path: path.clone(),
        queued: AtomicUsize::new(0),
        failure: Mutex::new(None),
        changed: tokio::sync::Notify::new(),
    });
    let worker_path = path.clone();
    std::thread::Builder::new()
        .name("bee-history".into())
        .spawn(move || {
            let mut pending = None;
            loop {
                let job = match pending.take().or_else(|| receiver.recv().ok()) {
                    Some(job) => job,
                    None => break,
                };
                match job {
                    Job::Run(run) => run(&mut journal),
                    Job::Release(reply) => {
                        // Never hand ownership to another connection while this
                        // writer can still perform any file operations.
                        drop(journal);
                        let _ =
                            reply.send(FileExt::unlock(&owner).map_err(|_| {
                                error(&worker_path, "cannot release session ownership")
                            }));
                        return;
                    }
                    Job::Text { .. } => {
                        let mut batch = vec![job];
                        // Batch only already queued text. Replay, state publication
                        // and finalization remain strict boundaries in this lane.
                        while batch.len() < 64 {
                            match receiver.try_recv() {
                                Ok(next @ Job::Text { .. }) => batch.push(next),
                                Ok(next) => {
                                    pending = Some(next);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        persist_text(&mut journal, batch);
                    }
                }
            }
            drop(journal);
            drop(owner);
        })
        .map_err(|_| error(&path, "cannot start history worker"))?;
    Ok(Handle(worker))
}

fn persist_text(journal: &mut Journal, batch: Vec<Job>) {
    let Some(Job::Text { worker, .. }) = batch.first() else {
        return;
    };
    let Some(worker) = worker.upgrade() else {
        return;
    };
    let handle = Handle(worker);
    let result = handle.healthy().and_then(|()| {
        let events = batch
            .iter()
            .filter_map(|job| match job {
                Job::Text { turn, text, .. } => Some(Event::AssistantDelta {
                    turn: *turn,
                    text: text.clone(),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        journal.deltas(&events)
    });
    if let Err(failure) = &result {
        handle.fail(failure.clone());
    }
    for job in batch {
        if let Job::Text { emit, bytes, .. } = job {
            if result.is_ok() {
                emit();
            }
            handle.0.queued.fetch_sub(bytes, Ordering::AcqRel);
        }
    }
}

async fn blocking<T: Send + 'static>(
    body: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(body)
        .await
        .map_err(|_| error(Path::new("<history worker>"), "history worker stopped"))?
}

impl Handle {
    pub(crate) fn presentation(
        &self,
        turn: u64,
        event: crate::presentation::Event,
        emit: impl FnOnce() + Send + 'static,
    ) -> tokio::sync::oneshot::Receiver<Result<()>> {
        let (send, receive) = tokio::sync::oneshot::channel();
        let bytes = serde_json::to_vec(&event)
            .map_or(usize::MAX / 4, |v| v.len().saturating_mul(2))
            .saturating_add(256);
        if let Err(error) = self.healthy() {
            let _ = send.send(Err(error));
            return receive;
        }
        if self
            .0
            .queued
            .fetch_add(bytes, Ordering::AcqRel)
            .saturating_add(bytes)
            > 4 * 1024 * 1024
        {
            self.0.queued.fetch_sub(bytes, Ordering::AcqRel);
            let failure = error(&self.0.path, "history output queue is full");
            self.fail(failure.clone());
            let _ = send.send(Err(failure));
            return receive;
        }
        let handle = self.clone();
        if self
            .0
            .sender
            .send(Job::Run(Box::new(move |journal| {
                let result = handle.healthy().and_then(|()| {
                    journal.append(Event::Presentation { turn, event })?;
                    emit();
                    Ok(())
                });
                if let Err(failure) = &result {
                    handle.fail(failure.clone());
                }
                handle.0.queued.fetch_sub(bytes, Ordering::AcqRel);
                let _ = send.send(result);
            })))
            .is_err()
        {
            self.0.queued.fetch_sub(bytes, Ordering::AcqRel);
            self.fail(error(&self.0.path, "history worker stopped"));
        }
        receive
    }
    /// Ephemeral state publication shares replay's lane, not its journal.
    pub(crate) fn enqueue(&self, emit: impl FnOnce() + Send + 'static) -> Result<()> {
        let emit = Arc::new(Mutex::new(Some(emit)));
        let queued = Arc::clone(&emit);
        let result = self.0.sender.send(Job::Run(Box::new(move |_| {
            if let Some(emit) = queued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                emit();
            }
        })));
        if result.is_err() {
            // With no worker left, replay cannot interleave with this fallback.
            if let Some(emit) = emit
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                emit();
            }
            return Err(error(&self.0.path, "history worker stopped"));
        }
        Ok(())
    }

    pub(crate) async fn publish(&self, emit: impl FnOnce() + Send + 'static) {
        let emit = Arc::new(Mutex::new(Some(emit)));
        let queued = Arc::clone(&emit);
        let (send, receive) = tokio::sync::oneshot::channel();
        let _ = self.enqueue(move || {
            if let Some(emit) = queued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                emit();
            }
            let _ = send.send(());
        });
        if receive.await.is_err()
            && let Some(emit) = emit
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        {
            emit();
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        body: impl FnOnce(&mut Journal, &Path) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (send, receive) = tokio::sync::oneshot::channel();
        let path = self.0.path.clone();
        self.0
            .sender
            .send(Job::Run(Box::new(move |journal| {
                let _ = send.send(body(journal, &path));
            })))
            .map_err(|_| error(&self.0.path, "history worker stopped"))?;
        receive
            .await
            .map_err(|_| error(&self.0.path, "history worker stopped"))?
    }

    async fn load(self) -> Result<(Snapshot, Self, bool)> {
        let (snapshot, interrupted) = self
            .run(|journal, _| {
                Ok((
                    journal.projection.snapshot.clone(),
                    journal.needs_recovery(),
                ))
            })
            .await?;
        Ok((snapshot, self, interrupted))
    }

    pub(crate) async fn recover(&self) -> Result<()> {
        // Validation of cwd precedes repairs, so an invalid resume cannot
        // truncate even an incomplete tail or consume its recovery warning.
        self.run(|journal, _| journal.recover()).await
    }

    pub(crate) async fn accept(&self, turn: u64, content: serde_json::Value) -> Result<()> {
        self.healthy()?;
        self.run(move |journal, _| journal.append(Event::TurnAccepted { turn, content }))
            .await
            .inspect_err(|failure| self.fail(failure.clone()))
    }

    pub(crate) fn text(&self, turn: u64, text: String, emit: impl FnOnce() + Send + 'static) {
        const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
        if self.healthy().is_err() {
            return;
        }
        // Command overhead prevents empty/tiny deltas from evading the bound.
        let bytes = text.len().saturating_add(256);
        if self
            .0
            .queued
            .fetch_add(bytes, Ordering::AcqRel)
            .saturating_add(bytes)
            > MAX_QUEUED_BYTES
        {
            self.0.queued.fetch_sub(bytes, Ordering::AcqRel);
            self.fail(error(&self.0.path, "history output queue is full"));
            return;
        }
        let worker = Arc::downgrade(&self.0);
        if self
            .0
            .sender
            .send(Job::Text {
                turn,
                text,
                emit: Box::new(emit),
                worker,
                bytes,
            })
            .is_err()
        {
            self.0.queued.fetch_sub(bytes, Ordering::AcqRel);
            self.fail(error(&self.0.path, "history worker stopped"));
        }
    }

    pub(crate) fn plan(
        &self,
        turn: u64,
        plan: crate::plan::Entries,
        emit: impl FnOnce() + Send + 'static,
    ) -> tokio::sync::oneshot::Receiver<Result<()>> {
        let (send, receive) = tokio::sync::oneshot::channel();
        // Plans are size-bounded before enqueue, but still share streaming's
        // aggregate bound so slow disks cannot accumulate unbounded output.
        let bytes = serde_json::to_vec(&plan).map_or(crate::plan::MAX_BYTES, |v| v.len()) + 256;
        if let Err(error) = self.healthy() {
            let _ = send.send(Err(error));
            return receive;
        }
        if self
            .0
            .queued
            .fetch_add(bytes, Ordering::AcqRel)
            .saturating_add(bytes)
            > 4 * 1024 * 1024
        {
            self.0.queued.fetch_sub(bytes, Ordering::AcqRel);
            let failure = error(&self.0.path, "history output queue is full");
            self.fail(failure.clone());
            let _ = send.send(Err(failure));
            return receive;
        }
        let handle = self.clone();
        if self
            .0
            .sender
            .send(Job::Run(Box::new(move |journal| {
                let result = handle.healthy().and_then(|()| {
                    if journal.projection.plans.get(&turn) != Some(&plan) {
                        journal.append(Event::PlanUpdated { turn, plan })?;
                    }
                    emit();
                    Ok(())
                });
                if let Err(failure) = &result {
                    handle.fail(failure.clone());
                }
                handle.0.queued.fetch_sub(bytes, Ordering::AcqRel);
                let _ = send.send(result);
            })))
            .is_err()
        {
            self.0.queued.fetch_sub(bytes, Ordering::AcqRel);
            self.fail(error(&self.0.path, "history worker stopped"));
        }
        receive
    }

    fn fail(&self, failure: Error) {
        let mut slot = self
            .0
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(failure);
        }
        self.0.changed.notify_one();
    }

    pub(crate) fn healthy(&self) -> Result<()> {
        self.0
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map_or(Ok(()), Err)
    }

    pub(crate) async fn failed(&self) -> Error {
        loop {
            let notified = self.0.changed.notified();
            if let Err(failure) = self.healthy() {
                return failure;
            }
            notified.await;
        }
    }

    pub(crate) async fn save(&self, snapshot: Snapshot) -> Result<()> {
        self.healthy()?;
        self.run(move |journal, path| {
            let event = journal.projection.settings(&snapshot, path)?;
            journal.append(event)
        })
        .await
        .inspect_err(|failure| self.fail(failure.clone()))
    }

    /// Produce a historical event only for reader/recovery compatibility fixtures.
    #[cfg(test)]
    pub(crate) async fn initialize_model(&self, turn: u64, snapshot: Snapshot) -> Result<()> {
        self.healthy()?;
        self.run(move |journal, path| {
            journal.projection.settings(&snapshot, path)?;
            journal.append(Event::InitialModelSelected {
                turn,
                provider: snapshot
                    .provider
                    .ok_or_else(|| error(path, "initial selection needs an account"))?,
                model: snapshot
                    .model
                    .ok_or_else(|| error(path, "initial selection needs a model"))?,
            })
        })
        .await
        .inspect_err(|failure| self.fail(failure.clone()))
    }

    pub(crate) async fn finish(
        &self,
        turn: u64,
        outcome: &'static str,
        snapshot: Option<Snapshot>,
    ) -> Result<()> {
        let handle = self.clone();
        self.run(move |journal, path| {
            handle.healthy()?;
            let outcome = match outcome {
                "completed" => Outcome::Completed,
                "cancelled" => Outcome::Cancelled,
                "error" => Outcome::Error,
                "refusal" => Outcome::Refusal,
                "interrupted" => Outcome::Interrupted,
                _ => return Err(error(path, "unknown turn outcome")),
            };
            let context = snapshot
                .as_ref()
                .map(|snapshot| journal.projection.context(snapshot, path))
                .transpose()?;
            // Terminal outcome and successful-context delta are one record, so
            // they cannot disagree after a crash between separate file writes.
            journal.append(Event::TurnFinished {
                turn,
                outcome,
                context,
            })
        })
        .await
        .inspect_err(|failure| self.fail(failure.clone()))
    }

    #[cfg(test)]
    pub(crate) async fn replay(
        &self,
        budget: Arc<ReplayBudget>,
        emit: impl FnMut(Message) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.replay_with_plans(budget, emit, |_, _| Ok(())).await
    }

    #[cfg(test)]
    pub(crate) async fn replay_with_plans(
        &self,
        budget: Arc<ReplayBudget>,
        emit: impl FnMut(Message) -> Result<()> + Send + 'static,
        emit_plan: impl FnMut(u64, crate::plan::Entries) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.replay_all(budget, emit, emit_plan, |_| Ok(())).await
    }

    pub(crate) async fn replay_all(
        &self,
        budget: Arc<ReplayBudget>,
        mut emit: impl FnMut(Message) -> Result<()> + Send + 'static,
        mut emit_plan: impl FnMut(u64, crate::plan::Entries) -> Result<()> + Send + 'static,
        mut emit_presentation: impl FnMut(serde_json::Value) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.run(move |journal, _| {
            if journal.projection.transcript_bytes > budget.0.load(Ordering::Acquire) {
                return Err(ReplayBudget::exhausted());
            }
            let mut bytes = 0_usize;
            // Reserve the whole replay before delivery. The ordered worker
            // guarantees both passes see the same durable journal prefix.
            journal.for_each_message(|message| {
                bytes = bytes
                    .checked_add(ReplayBudget::message_bytes(&message)?)
                    .ok_or_else(ReplayBudget::exhausted)?;
                if bytes > budget.0.load(Ordering::Acquire) {
                    return Err(ReplayBudget::exhausted());
                }
                Ok(())
            })?;
            for plan in journal.projection.plans.values() {
                bytes = bytes
                    .checked_add(plan_replay_bytes(plan)?)
                    .ok_or_else(ReplayBudget::exhausted)?;
            }
            let presentation = journal.projection.presentation.snapshots();
            for update in &presentation {
                bytes = bytes
                    .checked_add(update.to_string().len() + 256)
                    .ok_or_else(ReplayBudget::exhausted)?;
            }
            budget.reserve(bytes)?;
            journal.for_each_message(&mut emit)?;
            for (turn, plan) in &journal.projection.plans {
                emit_plan(*turn, plan.clone())?;
            }
            for update in presentation {
                emit_presentation(update)?;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn drain(&self) -> Result<()> {
        self.run(|_, _| Ok(())).await
    }

    pub(crate) async fn release(&self) -> Result<()> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.0
            .sender
            .send(Job::Release(send))
            .map_err(|_| error(&self.0.path, "history worker stopped"))?;
        receive
            .await
            .map_err(|_| error(&self.0.path, "history worker stopped"))?
    }
}

pub(crate) fn plan_replay_bytes(plan: &crate::plan::Entries) -> Result<usize> {
    // Include protocol framing and the runtime-owned identity in replay budgets.
    serde_json::to_vec(plan)
        .map(|bytes| bytes.len() + 256)
        .map_err(|_| ReplayBudget::exhausted())
}

#[cfg(test)]
#[path = "history/tests.rs"]
mod tests;
