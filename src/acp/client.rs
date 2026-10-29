//! Scoped outbound ACPv2 clients for explicitly selected local agents.
//!
//! The connection future owns the child. Prompt acceptance and foreground idle
//! are independent protocol events; [`Prompt::finish`] waits for both.
#![doc = include_str!("../../docs/acp-client.md")]

pub mod config;
mod interactions;

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo, V2ConnectionTo};
use futures::future::BoxFuture;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// The pinned SDK's structured wire types, confined to the ACP boundary.
pub use agent_client_protocol::schema::{MaybeUndefined, v2};
pub use interactions::{Handler, HostHandlers};

/// Value-redacted outbound diagnostic. Peer errors and stderr never become sources.
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    /// Invalid merged delegate configuration.
    #[error("agent configuration in `{path}` {reason}")]
    #[diagnostic(
        code(bee::acp_client::setting),
        help("correct the named [agents.NAME] definition")
    )]
    Setting {
        /// Originating configuration file.
        path: String,
        /// Static explanation, never configuration contents.
        reason: &'static str,
    },
    /// Explicit selection did not match a definition.
    #[error("the requested delegate is not configured")]
    #[diagnostic(
        code(bee::acp_client::unknown_agent),
        help("select a name returned by Agents::names")
    )]
    UnknownAgent,
    /// Launch, transport, or peer protocol failure.
    #[error("outbound ACP failed during {stage}")]
    #[diagnostic(
        code(bee::acp_client::connection),
        help(
            "check the executable and its ACPv2 compatibility; inspect the agent's own diagnostics"
        )
    )]
    Connection {
        /// Static lifecycle stage.
        stage: &'static str,
    },
    /// Required protocol capability was absent.
    #[error("the delegate does not support {capability}")]
    #[diagnostic(
        code(bee::acp_client::unsupported),
        help("use a compatible ACPv2 agent or install the required host handler")
    )]
    Unsupported {
        /// Known capability description.
        capability: &'static str,
    },
    /// A bounded stage did not complete.
    #[error("outbound ACP timed out during {stage}")]
    #[diagnostic(
        code(bee::acp_client::timeout),
        help("check whether the agent is responsive or adjust ClientOptions deadlines")
    )]
    Timeout {
        /// Static lifecycle stage.
        stage: &'static str,
    },
    /// Application-owned output or interaction bounds were exceeded.
    #[error("outbound ACP exceeded its bounded {resource}")]
    #[diagnostic(
        code(bee::acp_client::limit),
        help("consume progress promptly or increase the relevant ClientOptions bound")
    )]
    Limit {
        /// Known resource name.
        resource: &'static str,
    },
    /// Host interactions were unavailable or invalid.
    #[error("outbound ACP interaction {reason}")]
    #[diagnostic(
        code(bee::acp_client::interaction),
        help("provide a compatible host handler returning an offered, schema-valid response")
    )]
    Interaction {
        /// Static classification.
        reason: &'static str,
    },
    /// Caller requested cancellation, independently of the remote stop reason.
    #[error("outbound ACP was cancelled locally")]
    #[diagnostic(
        code(bee::acp_client::cancelled),
        help("start a new operation after cleanup completes")
    )]
    Cancelled,
    /// An abandoned or unfinished operation prevents reuse.
    #[error("the outbound session is busy or its state is uncertain")]
    #[diagnostic(
        code(bee::acp_client::busy),
        help("finish the current prompt or close the session before continuing")
    )]
    Busy,
}

/// Outbound client result, with safe typed diagnostics.
pub type Result<T> = std::result::Result<T, Error>;

/// Runtime-owned deadlines and application retention limits.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Initialization and session setup/acceptance deadline (default 30 seconds).
    pub request_timeout: Duration,
    /// Foreground completion deadline (default 10 minutes).
    pub prompt_timeout: Duration,
    /// Host interaction deadline (default 5 minutes).
    pub interaction_timeout: Duration,
    /// Cancellation and session-close grace (default 2 seconds each).
    pub cleanup_timeout: Duration,
    /// Maximum queued progress updates (default 256).
    pub max_updates: usize,
    /// Maximum queued update bytes and separately retained message bytes (default 4 MiB).
    pub max_bytes: usize,
    /// Maximum simultaneous interactions and tracked URL operations (default 16).
    pub max_interactions: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            prompt_timeout: Duration::from_secs(600),
            interaction_timeout: Duration::from_secs(300),
            cleanup_timeout: Duration::from_secs(2),
            max_updates: 256,
            max_bytes: 4 * 1024 * 1024,
            max_interactions: 16,
        }
    }
}

impl ClientOptions {
    fn validate(&self) -> Result<()> {
        if self.request_timeout.is_zero()
            || self.prompt_timeout.is_zero()
            || self.interaction_timeout.is_zero()
            || self.cleanup_timeout.is_zero()
            || self.max_updates == 0
            || self.max_bytes == 0
            || self.max_interactions == 0
            || self.max_interactions > tokio::sync::Semaphore::MAX_PERMITS
            || u32::try_from(self.max_interactions).is_err()
            || [
                self.request_timeout,
                self.prompt_timeout,
                self.interaction_timeout,
                self.cleanup_timeout,
            ]
            .iter()
            .any(|duration| tokio::time::Instant::now().checked_add(*duration).is_none())
        {
            return Err(Error::Setting {
                path: "<runtime>".into(),
                reason: "limits and deadlines must be positive and representable",
            });
        }
        Ok(())
    }
}

#[derive(Default)]
struct Inbox {
    session: Option<v2::SessionId>,
    collecting: bool,
    queue: VecDeque<(v2::UpdateSessionNotification, usize)>,
    bytes: usize,
    active: bool,
    idle: Option<v2::IdleStateUpdate>,
    error: Option<Error>,
}

struct Shared {
    interactions: interactions::State,
    inbox: Mutex<Inbox>,
    wake: Notify,
    closed: CancellationToken,
    shutdown: CancellationToken,
    session_cancel: Mutex<CancellationToken>,
    options: ClientOptions,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    fn fail(&self, error: Error) {
        lock(&self.inbox).error.get_or_insert(error);
        self.wake.notify_one();
    }

    fn update(&self, update: v2::UpdateSessionNotification) {
        let mut inbox = lock(&self.inbox);
        if inbox.session.as_ref() != Some(&update.session_id) {
            return;
        }
        match &update.update {
            v2::SessionUpdate::StateUpdate(
                v2::StateUpdate::Running(_) | v2::StateUpdate::RequiresAction(_),
            ) => {
                inbox.active = true;
                inbox.idle = None;
            }
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) if inbox.active => {
                inbox.active = false;
                inbox.idle = Some(idle.clone());
            }
            _ => {}
        }
        if inbox.collecting {
            let bytes = serde_json::to_vec(&update).map_or(usize::MAX, |v| v.len());
            if inbox.queue.len() >= self.options.max_updates
                || bytes > self.options.max_bytes.saturating_sub(inbox.bytes)
            {
                inbox.error.get_or_insert(Error::Limit {
                    resource: "progress queue",
                });
            } else {
                inbox.bytes += bytes;
                inbox.queue.push_back((update, bytes));
            }
        }
        // Control state stays independent of queue capacity so cancellation can still progress.
        self.wake.notify_one();
    }
}

/// Connection-scoped facade. A callback cannot return borrowed prompt handles.
pub struct Connection {
    sdk: V2ConnectionTo<Agent>,
    initialized: v2::InitializeResponse,
    session: Option<v2::SessionId>,
    shared: Arc<Shared>,
    cancellation: CancellationToken,
    busy: bool,
}

/// Launch a configured local agent and control it within an owned async scope.
///
/// # Errors
/// Returns safe configuration, negotiation, transport, deadline or callback errors.
/// On normal return sessions are closed; dropping the future invokes SDK process cleanup.
pub async fn connect_with<T: Send, F>(
    definition: &config::Definition,
    handlers: HostHandlers,
    options: ClientOptions,
    cancellation: CancellationToken,
    run: F,
) -> Result<T>
where
    F: for<'a> FnOnce(&'a mut Connection) -> BoxFuture<'a, Result<T>> + Send,
{
    let process = AcpAgentConfig::new(&definition.command)
        .args(definition.args.clone())
        .envs(definition.env.clone());
    connect_transport(AcpAgent::new(process), handlers, options, cancellation, run).await
}

// Keep the transport seam private: production launch and deterministic in-memory tests
// exercise the same policy without creating a second transport abstraction.
async fn connect_transport<T: Send, F>(
    transport: impl ConnectTo<Client>,
    handlers: HostHandlers,
    options: ClientOptions,
    cancellation: CancellationToken,
    run: F,
) -> Result<T>
where
    F: for<'a> FnOnce(&'a mut Connection) -> BoxFuture<'a, Result<T>> + Send,
{
    options.validate()?;
    let shared = Arc::new(Shared {
        interactions: interactions::State::new(options.max_interactions),
        inbox: Mutex::new(Inbox::default()),
        wake: Notify::new(),
        closed: CancellationToken::new(),
        shutdown: CancellationToken::new(),
        session_cancel: Mutex::new(CancellationToken::new()),
        options,
    });
    let updates = shared.clone();
    let close = shared.clone();
    let capabilities = handlers.capabilities();
    let builder = Client
        .v2()
        .name("bee-outbound")
        .on_close(async move |_| {
            close.closed.cancel();
            Ok(())
        })
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                updates.update(update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let permission_shared = shared.clone();
    let permission_handlers = handlers.clone();
    let elicitation_shared = shared.clone();
    let elicitation_handlers = handlers.clone();
    let complete_shared = shared.clone();
    let result = builder
        .on_receive_request(
            async move |request: v2::RequestPermissionRequest, responder, connection| {
                interactions::permission(
                    request,
                    responder,
                    connection,
                    permission_handlers.clone(),
                    permission_shared.clone(),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::CreateElicitationRequest, responder, connection| {
                interactions::elicit(
                    request,
                    responder,
                    connection,
                    elicitation_handlers.clone(),
                    elicitation_shared.clone(),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: v2::CompleteElicitationNotification, connection| {
                interactions::complete(
                    notification,
                    connection,
                    handlers.clone(),
                    complete_shared.clone(),
                )
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |sdk| {
            let request = v2::InitializeRequest::new(
                agent_client_protocol::schema::ProtocolVersion::V2,
                v2::Implementation::new("bee", env!("CARGO_PKG_VERSION")),
            )
            .capabilities(capabilities);
            let initialized = wait_request(
                sdk.send_request(request).block_task(),
                &shared,
                &cancellation,
                "initialization",
            )
            .await;
            let initialized = match initialized {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            if initialized.capabilities.session.is_none() {
                return Ok(Err(Error::Unsupported {
                    capability: "ACPv2 sessions",
                }));
            }
            let mut client = Connection {
                sdk,
                initialized,
                session: None,
                shared,
                cancellation,
                busy: false,
            };
            let cancelled = client.cancellation.clone();
            let closed = client.shared.closed.clone();
            let result = tokio::select! {
                biased;
                () = cancelled.cancelled() => Err(Error::Cancelled),
                () = closed.cancelled() => Err(Error::Connection { stage: "connection scope" }),
                result = run(&mut client) => result,
            };
            let result = result.and_then(|value| {
                lock(&client.shared.inbox)
                    .error
                    .clone()
                    .map_or(Ok(value), Err)
            });
            let cleanup = client.close_session().await;
            client.shared.shutdown.cancel();
            Ok(result.and_then(|value| cleanup.map(|()| value)))
        })
        .await;
    // SDK errors may include arbitrary peer data and captured stderr: never format them.
    result.map_err(|_| Error::Connection {
        stage: "launch or transport",
    })?
}

async fn wait_request<T>(
    future: impl std::future::Future<Output = std::result::Result<T, agent_client_protocol::Error>>,
    shared: &Shared,
    cancellation: &CancellationToken,
    stage: &'static str,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(Error::Cancelled),
        () = shared.closed.cancelled() => Err(Error::Connection { stage }),
        result = tokio::time::timeout(shared.options.request_timeout, future) =>
            result.map_err(|_| Error::Timeout { stage })?.map_err(|_| Error::Connection { stage }),
    }
}

impl Connection {
    /// Negotiated peer metadata and capabilities, preserved as structured data.
    pub fn initialized(&self) -> &v2::InitializeResponse {
        &self.initialized
    }

    /// Open a session with an absolute protocol cwd (not the child process cwd).
    ///
    /// # Errors
    /// Returns busy, cancellation, timeout, or protocol errors.
    pub async fn new_session(&mut self, cwd: &Path) -> Result<v2::NewSessionResponse> {
        if self.session.is_some() || self.busy {
            return Err(Error::Busy);
        }
        if !cwd.is_absolute() {
            return Err(Error::Setting {
                path: "<runtime>".into(),
                reason: "session cwd must be absolute",
            });
        }
        // Dropping setup after publication leaves remote state uncertain; do not reopen blindly.
        self.busy = true;
        let response = wait_request(
            self.sdk
                .send_request(v2::NewSessionRequest::new(cwd))
                .block_task(),
            &self.shared,
            &self.cancellation,
            "session creation",
        )
        .await?;
        self.session = Some(response.session_id.clone());
        self.busy = false;
        lock(&self.shared.inbox).session = self.session.clone();
        *lock(&self.shared.session_cancel) = CancellationToken::new();
        Ok(response)
    }

    /// Submit a prompt without confusing its acceptance response with completion.
    ///
    /// # Errors
    /// Returns busy, unsupported content, or cancellation errors before submission.
    pub fn start_prompt(
        &mut self,
        content: Vec<v2::ContentBlock>,
        cancellation: CancellationToken,
    ) -> Result<Prompt<'_>> {
        let Some(session) = &self.session else {
            return Err(Error::Busy);
        };
        if self.busy || lock(&self.shared.inbox).active {
            return Err(Error::Busy);
        }
        if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let caps = &self
            .initialized
            .capabilities
            .session
            .as_ref()
            .expect("negotiated session capability")
            .prompt;
        for block in &content {
            let supported = match block {
                v2::ContentBlock::Text(_) | v2::ContentBlock::ResourceLink(_) => true,
                v2::ContentBlock::Image(_) => caps.as_ref().is_some_and(|c| c.image.is_some()),
                v2::ContentBlock::Audio(_) => caps.as_ref().is_some_and(|c| c.audio.is_some()),
                v2::ContentBlock::Resource(_) => {
                    caps.as_ref().is_some_and(|c| c.embedded_context.is_some())
                }
                _ => false,
            };
            if !supported {
                return Err(Error::Unsupported {
                    capability: "requested prompt content",
                });
            }
        }
        {
            let mut inbox = lock(&self.shared.inbox);
            inbox.collecting = true;
            inbox.queue.clear();
            inbox.bytes = 0;
            inbox.idle = None;
            if let Some(error) = &inbox.error {
                return Err(error.clone());
            }
        }
        self.busy = true;
        let pending = self
            .sdk
            .send_request(v2::PromptRequest::new(session.clone(), content));
        Ok(Prompt {
            acceptance: Some(Box::pin(async move { pending.block_task().await })),
            accepted: None,
            client: self,
            cancellation,
            started: tokio::time::Instant::now(),
            messages: HashMap::new(),
            order: Vec::new(),
            done: false,
        })
    }

    /// Cancel unfinished work, close the session, and prevent uncertain reuse.
    ///
    /// # Errors
    /// Returns a bounded shutdown error if cancellation/close does not complete.
    pub async fn close_session(&mut self) -> Result<()> {
        let Some(session) = self.session.clone() else {
            return Ok(());
        };
        let unfinished = self.busy || lock(&self.shared.inbox).active;
        // Keep ownership registered across await points so dropping close still gets scope cleanup.
        self.busy = true;
        lock(&self.shared.session_cancel).cancel();
        let _ = tokio::time::timeout(
            self.shared.options.cleanup_timeout,
            self.shared
                .interactions
                .drain(self.shared.options.max_interactions),
        )
        .await;
        if unfinished {
            let _ = self
                .sdk
                .send_notification(v2::CancelSessionNotification::new(session.clone()));
            // A missing terminal update cannot indefinitely retain the subprocess.
            let _ = tokio::time::timeout(self.shared.options.cleanup_timeout, async {
                loop {
                    let wake = self.shared.wake.notified();
                    if lock(&self.shared.inbox).idle.is_some() || self.shared.closed.is_cancelled()
                    {
                        break;
                    }
                    wake.await;
                }
            })
            .await;
        }
        let closed = tokio::time::timeout(
            self.shared.options.cleanup_timeout,
            self.sdk
                .send_request(v2::CloseSessionRequest::new(session))
                .block_task(),
        )
        .await;
        self.session = None;
        lock(&self.shared.inbox).collecting = false;
        match closed {
            Ok(Ok(_)) => {
                self.busy = false;
                *lock(&self.shared.inbox) = Inbox::default();
                Ok(())
            }
            Ok(Err(_)) => Err(Error::Connection {
                stage: "session close",
            }),
            Err(_) => Err(Error::Timeout {
                stage: "session close",
            }),
        }
    }
}

/// Structured acceptance, terminal state, and bounded reduced agent messages.
pub struct PromptResult {
    /// User message insertion acknowledgement.
    pub acceptance: v2::PromptResponse,
    /// Observed terminal foreground state (including its optional stop reason).
    pub idle: v2::IdleStateUpdate,
    /// Agent messages in first-seen order, retaining non-text content.
    pub messages: Vec<v2::AgentMessage>,
}

/// Borrowed foreground operation. Dropping unfinished work cancels and prevents reuse.
pub struct Prompt<'a> {
    client: &'a mut Connection,
    acceptance: Option<
        BoxFuture<'static, std::result::Result<v2::PromptResponse, agent_client_protocol::Error>>,
    >,
    accepted: Option<v2::PromptResponse>,
    cancellation: CancellationToken,
    started: tokio::time::Instant,
    messages: HashMap<v2::MessageId, v2::AgentMessage>,
    order: Vec<v2::MessageId>,
    done: bool,
}

impl Prompt<'_> {
    /// Request session cancellation; the operation remains locally cancelled even if idle races.
    pub fn cancel(&self) {
        self.cancellation.cancel();
        lock(&self.client.shared.session_cancel).cancel();
        if let Some(session) = &self.client.session {
            let _ = self
                .client
                .sdk
                .send_notification(v2::CancelSessionNotification::new(session.clone()));
        }
    }

    /// Receive ordered structured progress, or `None` once acceptance and terminal idle are known.
    ///
    /// # Errors
    /// Returns cancellation, limit, peer, interaction, or deadline failures.
    pub async fn next_update(&mut self) -> Result<Option<v2::UpdateSessionNotification>> {
        loop {
            let shared = self.client.shared.clone();
            let wake = shared.wake.notified();
            let stage = if self.accepted.is_some() {
                "prompt completion"
            } else {
                "prompt acceptance"
            };
            let timeout = if self.accepted.is_some() {
                shared.options.prompt_timeout
            } else {
                shared.options.request_timeout
            };
            // Check even while draining a nonempty queue: chatty peers cannot evade deadlines.
            if self.started.elapsed() >= timeout {
                return Err(Error::Timeout { stage });
            }
            if self.cancellation.is_cancelled() || self.client.cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if shared.closed.is_cancelled() {
                return Err(Error::Connection {
                    stage: "prompt completion",
                });
            }
            let next = {
                let mut inbox = lock(&shared.inbox);
                if let Some(error) = &inbox.error {
                    return Err(error.clone());
                }
                let next = inbox.queue.pop_front();
                if let Some((_, bytes)) = &next {
                    inbox.bytes -= bytes;
                }
                next.map(|(update, _)| update)
            };
            if let Some(update) = next {
                self.reduce(&update.update)?;
                return Ok(Some(update));
            }
            if self.accepted.is_some() && lock(&shared.inbox).idle.is_some() {
                return Ok(None);
            }
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => return Err(Error::Cancelled),
                () = self.client.cancellation.cancelled() => return Err(Error::Cancelled),
                () = shared.closed.cancelled() => return Err(Error::Connection { stage }),
                result = async { self.acceptance.as_mut().expect("guarded pending acceptance").await }, if self.acceptance.is_some() => {
                    self.acceptance = None;
                    match result {
                        Ok(response) => self.accepted = Some(response),
                        Err(_) => {
                            let mut inbox = lock(&shared.inbox);
                            if !inbox.active {
                                self.done = true;
                                self.client.busy = false;
                                inbox.collecting = false;
                            }
                            return Err(Error::Connection { stage: "prompt acceptance" });
                        }
                    }
                }
                () = tokio::time::sleep_until(self.started + timeout) => return Err(Error::Timeout { stage }),
                () = wake => {}
            }
        }
    }

    fn reduce(&mut self, update: &v2::SessionUpdate) -> Result<()> {
        let (id, snapshot, chunk) = match update {
            v2::SessionUpdate::AgentMessage(message) => {
                (message.message_id.clone(), Some(message), None)
            }
            v2::SessionUpdate::AgentMessageChunk(chunk) => {
                (chunk.message_id.clone(), None, Some(&chunk.content))
            }
            _ => return Ok(()),
        };
        if !self.messages.contains_key(&id) {
            self.order.push(id.clone());
        }
        let message = self
            .messages
            .entry(id.clone())
            .or_insert_with(|| v2::AgentMessage::new(id));
        if let Some(snapshot) = snapshot {
            // Content replaces as a whole; metadata instead patches by top-level key.
            if !matches!(snapshot.content, MaybeUndefined::Undefined) {
                message.content = snapshot.content.clone();
            }
            v2::apply_meta_update(&mut message.meta, snapshot.meta.clone());
        }
        if let Some(chunk) = chunk {
            match &mut message.content {
                MaybeUndefined::Value(content) => content.push(chunk.clone()),
                content => *content = MaybeUndefined::Value(vec![chunk.clone()]),
            }
        }
        let bytes = serde_json::to_vec(&self.messages).map_or(usize::MAX, |v| v.len());
        if bytes > self.client.shared.options.max_bytes {
            return Err(Error::Limit {
                resource: "result messages",
            });
        }
        Ok(())
    }

    /// Drain progress and return the bounded structured result after terminal idle.
    ///
    /// # Errors
    /// Returns the same failures as [`Self::next_update`]. Failed operations remain
    /// non-reusable until the connection owner completes session cleanup.
    pub async fn finish(mut self) -> Result<PromptResult> {
        while self.next_update().await?.is_some() {}
        let idle = lock(&self.client.shared.inbox)
            .idle
            .clone()
            .expect("completion requires idle");
        self.done = true;
        self.client.busy = false;
        lock(&self.client.shared.inbox).collecting = false;
        Ok(PromptResult {
            acceptance: self
                .accepted
                .take()
                .expect("completion requires acceptance"),
            idle,
            messages: self
                .order
                .iter()
                .filter_map(|id| self.messages.remove(id))
                .collect(),
        })
    }
}

impl Drop for Prompt<'_> {
    fn drop(&mut self) {
        if !self.done {
            lock(&self.client.shared.session_cancel).cancel();
            if let Some(session) = &self.client.session {
                let _ = self
                    .client
                    .sdk
                    .send_notification(v2::CancelSessionNotification::new(session.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests;
