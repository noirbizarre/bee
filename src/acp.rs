//! The ACPv2 boundary: the only module that imports the ACP Rust SDK.
//!
//! Keeping the SDK behind this boundary and its private submodules confines
//! draft-protocol upgrades instead of spreading them through the core.
//!
//! Two layers, so the agent never learns which transport carries it (ADR-003):
//! [`agent`] is the transport-free core, [`serve`] runs it over any transport
//! the SDK can connect, and [`serve_stdio`] picks stdin/stdout. A remote
//! transport later supplies another [`ConnectTo`] and changes nothing else.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Agent, Client, ConnectTo, Responder, Stdio, V2ConnectionTo};
use futures::channel::oneshot;
use futures::future::{AbortHandle, AbortRegistration, Abortable, BoxFuture, FutureExt, Shared};

use crate::account::Accounts as Providers;
use crate::session::{SessionError, Sessions, Settings};
use crate::{Error, Result};

pub mod client;
mod interactions;
mod mcp;
mod plans;
mod presentation;
mod scenarios;

#[cfg(test)]
mod cloudflare_tests;
#[cfg(test)]
mod google_cloud_tests;
#[cfg(all(test, unix))]
mod oauth_tests;
#[cfg(test)]
mod ovh_tests;
#[cfg(test)]
mod scaleway_tests;

#[cfg(all(test, unix))]
mod copilot_tests;

/// The turn-owned plan operation added to every ACP runtime's model catalog.
///
/// Exposed for hosts estimating context budgets; execution requires an active
/// ACP turn and is supplied by the boundary rather than the native registry.
#[must_use]
pub fn plan_tool() -> crate::llm::Tool {
    plans::definition()
}

// Retain the busy barrier until aborted futures have actually been dropped and
// their terminal update sent. Session cancellation alone only requests abort.
type Tasks = Arc<Mutex<BTreeMap<String, Shared<BoxFuture<'static, ()>>>>>;
// Request-level mutations serialize per session, without blocking other IDs or
// cancellation notifications while disk work or historical replay is pending.
type Gates = Arc<Mutex<BTreeMap<String, std::sync::Weak<RequestGate>>>>;

#[derive(Default)]
struct RequestGate {
    mutation: Arc<tokio::sync::Mutex<()>>,
    closing: std::sync::atomic::AtomicUsize,
    selection_abort: Mutex<Option<AbortHandle>>,
}

struct Mutation {
    _gate: Arc<RequestGate>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for Mutation {
    fn drop(&mut self) {
        // An idle cancel must not retain an obsolete selection operation.
        lock(&self._gate.selection_abort).take();
    }
}

struct Closing {
    gate: Arc<RequestGate>,
    reservation: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl RequestGate {
    fn try_enter(self: Arc<Self>) -> std::result::Result<Mutation, ()> {
        let guard = Arc::clone(&self.mutation)
            .try_lock_owned()
            .map_err(|_| ())?;
        if self.closing.load(std::sync::atomic::Ordering::Acquire) != 0 {
            return Err(());
        }
        Ok(Mutation {
            _gate: self,
            _guard: guard,
        })
    }

    fn close(self: Arc<Self>) -> Closing {
        if let Some(abort) = lock(&self.selection_abort).take() {
            abort.abort();
        }
        // Reserve in dispatch, even if the close future has not been polled.
        // A count keeps multiple idempotent closes from releasing each other's reservation.
        self.closing
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let reservation = Arc::clone(&self.mutation).try_lock_owned().ok();
        Closing {
            gate: self,
            reservation,
        }
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        self.gate
            .closing
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

fn session_gate(gates: &Gates, id: &str) -> Arc<RequestGate> {
    let mut gates = lock(gates);
    // Only outstanding requests retain gates. Invalid IDs cannot accumulate
    // permanent keys, and live requests still share one mutation boundary.
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(id).and_then(std::sync::Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(RequestGate::default());
    gates.insert(id.to_owned(), Arc::downgrade(&gate));
    gate
}

/// Immutable execution policy and complete native catalog for one connection.
pub struct Runtime {
    #[cfg(test)]
    inference_http: Option<crate::http::InferenceHttp>,
    instructions: Option<crate::instructions::Sources>,
    limits: crate::agent::Limits,
    tools: crate::tool::Registry,
    permissions: crate::permissions::Settings,
    auth_environment: Option<crate::config::Environment>,
    login_http: Arc<dyn crate::http::LoginHttp>,
    google_cloud_http: Arc<dyn crate::auth::google_cloud::Http>,
    discovery: Option<Arc<crate::models::Service>>,
    history: Option<crate::history::Store>,
    history_settings: crate::history::Settings,
    preferences: crate::selection::Store,
}

impl Runtime {
    /// Configure native-tool routing without storing client approval decisions.
    #[must_use]
    pub fn with_permissions(mut self, settings: crate::permissions::Settings) -> Self {
        self.permissions = settings;
        self
    }
    /// Supply isolated or durable profile-scoped recent model preferences.
    #[must_use]
    pub fn with_preferences(mut self, store: crate::selection::Store) -> Self {
        self.preferences = store;
        self
    }
    /// Supply a complete catalog, which may be empty or extended by the caller.
    pub fn new(limits: crate::agent::Limits, tools: crate::tool::Registry) -> Self {
        Self {
            #[cfg(test)]
            inference_http: None,
            limits,
            tools,
            permissions: Default::default(),
            instructions: None,
            auth_environment: None,
            login_http: Arc::new(crate::http::AsyncLoginHttp),
            google_cloud_http: Arc::new(crate::http::GoogleAuthHttp),
            discovery: None,
            history: None,
            history_settings: Default::default(),
            preferences: Default::default(),
        }
    }

    /// Enable optional discovery with explicitly injected transport/cache dependencies.
    #[must_use]
    pub fn with_discovery(mut self, service: Arc<crate::models::Service>) -> Self {
        self.discovery = Some(service);
        self
    }

    /// Enable durable sessions. Embedded runtimes otherwise remain memory-only.
    #[must_use]
    pub fn with_history(mut self, store: crate::history::Store) -> Self {
        self.history = Some(store);
        self
    }

    /// Set the cumulative replay output budget for this connection.
    #[must_use]
    pub fn with_history_settings(mut self, settings: crate::history::Settings) -> Self {
        self.history_settings = settings;
        self
    }

    /// Supply credential sources and transport explicitly for an embedded runtime.
    #[must_use]
    pub fn with_auth(
        mut self,
        environment: crate::config::Environment,
        http: Arc<dyn crate::http::LoginHttp>,
    ) -> Self {
        self.auth_environment = Some(environment);
        self.login_http = http;
        self
    }

    /// Supply Google ADC transport independently of Bee-owned browser login.
    #[must_use]
    pub fn with_google_cloud_http(
        mut self,
        http: Arc<dyn crate::auth::google_cloud::Http>,
    ) -> Self {
        self.google_cloud_http = http;
        self
    }

    /// Enable instruction discovery with explicitly injected home and configuration.
    pub fn with_instructions(mut self, sources: crate::instructions::Sources) -> Self {
        self.instructions = Some(sources);
        self
    }

    /// Compile the shipped catalog before accepting prompts.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Tool`] if shipped metadata or schemas fail registration.
    pub fn builtins(limits: crate::agent::Limits) -> Result<Self> {
        Ok(Self::new(limits, crate::tool::builtins()?))
    }
}

/// Why a turn failed.
///
/// Reported to the client as the `error` stop reason, with this text as the
/// error's data. Not part of the stable API: it exists so tests can inject a
/// failing turn (see [`agent_with`]).
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TurnFailure(String);

impl TurnFailure {
    /// A failure that says `reason`.
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// How a running turn talks to its client.
///
/// Not part of the stable API: it is what [`agent_with`] hands the turn body.
/// It can only write agent output, and only while the turn is still the
/// session's current one, so a cancelled turn cannot speak after the client was
/// told it stopped.
#[doc(hidden)]
#[derive(Clone)]
pub struct Reply {
    sessions: Arc<Mutex<Sessions>>,
    connection: V2ConnectionTo<Client>,
    id: String,
    session_id: v2::SessionId,
    message_id: String,
    token: u64,
    input: Option<crate::session::InferenceInput>,
    protocol: Arc<Mutex<Option<crate::provider::Protocol>>>,
    completed: Arc<Mutex<Option<crate::agent::Outcome>>>,
    used: Arc<Mutex<Option<crate::selection::Reference>>>,
    selection_failure: Arc<Mutex<Option<crate::selection::Reason>>>,
    preferences: crate::selection::Store,
    plan: Arc<Mutex<Option<crate::plan::Entries>>>,
    presentation: Arc<Mutex<presentation::Ledger>>,
    presentation_history: Option<crate::history::Handle>,
    mcp: Arc<Mutex<mcp::Bindings>>,
    servers: Vec<mcp::Server>,
    interactions: Arc<Mutex<interactions::State>>,
}

impl Reply {
    fn selection_failed(&self, reason: crate::selection::Reason) {
        if lock(&self.sessions).automatic(&self.id).is_some() {
            *lock(&self.selection_failure) = Some(reason);
        }
    }
    fn diagnostic(&self, diagnostic: &crate::instructions::Diagnostic) {
        let sessions = lock(&self.sessions);
        if sessions.is_current(&self.id, self.token) {
            let description = match diagnostic.position {
                Some((line, column)) => format!("{diagnostic} (line {line}, column {column})"),
                None => diagnostic.to_string(),
            };
            let _ = send_update(
                &self.connection,
                &self.session_id,
                v2::SessionUpdate::Notice(
                    v2::Notice::new(v2::NoticeSeverity::Warning, "Agent skill discovery warning")
                        .description(description),
                ),
            );
        }
    }
    /// Send `text` as a chunk of the agent's answer, unless the turn was
    /// cancelled or has ended.
    pub fn text(&self, text: &str) {
        self.output(None, text);
    }

    /// Publish only displayable provider text, with Bee-owned turn/step identity.
    ///
    /// Returns a limit error for item or part ordinals outside the native display bounds.
    pub fn thought(
        &self,
        step: u32,
        item: u64,
        part: u64,
        text: &str,
    ) -> std::result::Result<(), crate::llm::Error> {
        let id = crate::history::ThoughtId { step, item, part };
        if item >= 128 || part >= 128 {
            return Err(crate::llm::Error::Limit);
        }
        self.output(Some(id), text);
        Ok(())
    }

    fn output(&self, thought: Option<crate::history::ThoughtId>, text: &str) {
        if text.is_empty() {
            return;
        }
        let message_id =
            thought.map_or_else(|| self.message_id.clone(), |id| id.message_id(self.token));
        let update = |text: String| {
            let chunk = v2::ContentChunk::new(text.into(), message_id.clone());
            if thought.is_some() {
                v2::SessionUpdate::AgentThoughtChunk(chunk)
            } else {
                v2::SessionUpdate::AgentMessageChunk(chunk)
            }
        };
        // Check/enqueue share cancellation's lock. Durable finalization drains
        // that ordered queue before idle; memory-only sessions send under the
        // guard. Neither path can emit a chunk after terminal idle.
        let mut sessions = lock(&self.sessions);
        if sessions.accepts_output(&self.id, self.token) {
            if let Some(history) = sessions.history(&self.id) {
                let connection = self.connection.clone();
                let session_id = self.session_id.clone();
                let update = update(text.to_owned());
                let text = text.to_owned();
                // The worker serializes durable chunks with replay. It does not
                // reacquire the registry, avoiding a worker/dispatch lock cycle.
                history.output(self.token, thought, text, move || {
                    let _ = send_update(&connection, &session_id, update);
                });
                return;
            }
            let kind = if thought.is_some() {
                crate::history::MessageKind::Thought
            } else {
                crate::history::MessageKind::Answer
            };
            sessions.record_output(&self.id, kind, message_id.as_ref(), text);
            // A failed send means the peer is gone, and the connection is
            // already shutting down: nothing is left to tell.
            let _ = send_update(&self.connection, &self.session_id, update(text.to_owned()));
        }
    }
}

/// Credentials are resolved at request time, not while loading configuration.
async fn inference(reply: Reply, runtime: Arc<Runtime>) -> std::result::Result<(), TurnFailure> {
    let profile = reply
        .input
        .as_ref()
        .map(|input| input.profile.clone())
        .ok_or_else(|| {
            TurnFailure::new(
                "choose an available account-qualified model over ACP before prompting",
            )
        })?;
    let env = runtime
        .auth_environment
        .clone()
        .unwrap_or_else(crate::config::Environment::from_process);
    let store = if matches!(
        profile.credential(),
        Some(crate::provider::CredentialSource::Store { .. })
    ) {
        crate::auth::FileStore::from_environment(&env).map_err(|_| {
            TurnFailure::new("cannot locate the credential store; configure HOME or XDG_DATA_HOME")
        })?
    } else {
        // resolve never touches the store for an environment/no-auth profile.
        crate::auth::FileStore::new("")
    };
    let chatgpt = profile
        .login_spec()
        .is_some_and(crate::provider::signin::LoginSpec::is_chatgpt);
    let cloud_session = if profile.google_cloud().is_some() {
        Some(
            crate::auth::google_cloud::Session::with_http(
                &profile,
                &env,
                runtime.google_cloud_http.clone(),
            )
            .await
            .map_err(|error| {
                reply.selection_failed(crate::selection::Reason::CredentialsUnavailable);
                TurnFailure::new(error.to_string())
            })?,
        )
    } else {
        None
    };
    let resolved = if let Some(session) = &cloud_session {
        session.credential().await.map(Some)
    } else if chatgpt {
        crate::auth::chatgpt::resolve(&store, profile.name(), runtime.login_http.as_ref())
            .await
            .map(Some)
    } else {
        crate::auth::resolve(&profile, &env, &store, crate::auth::unix_now())
    };
    let credential = match resolved {
        Ok(credential) => credential,
        Err(crate::Error::AuthNotLoggedIn { .. } | crate::Error::AuthExpired { .. })
            if profile.login_spec().is_some() && lock(&reply.interactions).url =>
        {
            reply
                .login(&profile, &store, &env, runtime.login_http.as_ref())
                .await?;
            if let Some(discovery) = &runtime.discovery {
                discovery.invalidate_endpoint(profile.name());
            }
            crate::auth::resolve(&profile, &env, &store, crate::auth::unix_now()).map_err(
                |error| {
                    if chatgpt {
                        return TurnFailure::new(error.to_string());
                    }
                    TurnFailure::new("cannot resolve the stored credential; run `bee auth login`")
                },
            )?
        }
        Err(error) if chatgpt || cloud_session.is_some() => {
            if let Some(reference) = lock(&reply.sessions).automatic(&reply.id) {
                *lock(&reply.selection_failure) =
                    crate::selection::authentication_failure(&reference, &profile, &store);
            }
            return Err(TurnFailure::new(error.to_string()));
        }
        Err(_) => {
            reply.selection_failed(crate::selection::Reason::CredentialsUnavailable);
            return Err(TurnFailure::new(
                "cannot resolve the model credential; check its environment variable or run `bee auth login`",
            ));
        }
    };
    if let Some(discovery) = &runtime.discovery {
        discovery.observe_account(&profile, credential.as_ref());
        if profile.discovery()
            && !discovery
                .account_snapshot(&profile, credential.as_ref())
                .endpoints
                .get(profile.name())
                .is_some_and(|listing| listing.fetched_at.is_some())
        {
            // A configured/explicit model is a preference, never evidence of account availability.
            let _ = discovery.list_endpoint(&profile, &env).await;
        }
    }
    let crate::session::InferenceInput {
        profile,
        model,
        mut messages,
        cwd,
        instructions,
        protocol,
    } = reply.input.clone().ok_or_else(|| {
        TurnFailure::new("choose an available account-qualified model over ACP before prompting")
    })?;
    let snapshot = runtime
        .discovery
        .as_ref()
        .map(|service| service.account_snapshot(&profile, credential.as_ref()))
        .unwrap_or_default();
    if let Some(reference) = lock(&reply.sessions).automatic(&reply.id) {
        // Login/renewal can replace an account after the initial preference check.
        if !reference.matches(&profile, credential.as_ref()) {
            *lock(&reply.selection_failure) = Some(crate::selection::Reason::AccountChanged);
            return Err(TurnFailure::new(
                crate::selection::Reason::AccountChanged.message(),
            ));
        }
        if let Some(reason) = crate::selection::unavailable(&snapshot, &profile, &model) {
            *lock(&reply.selection_failure) = Some(reason);
            return Err(TurnFailure::new(reason.message()));
        }
    }
    snapshot
        .check_available(&profile, &model)
        .map_err(TurnFailure::new)?;
    // Freeze routing and budgets only after authority and membership have both been checked.
    let selection = snapshot.resolve(&profile, &model);
    if profile.is_copilot() {
        snapshot
            .check_route(&profile, &model, selection.protocol)
            .map_err(|error| TurnFailure::new(error.to_string()))?;
    }
    if protocol.is_some_and(|previous| previous != selection.protocol)
        || messages.iter().any(|message| matches!(message, crate::llm::Message::Assistant { native: Some(native), .. } if native.protocol != selection.protocol))
    {
        // Only the authenticated, availability-checked route may decide whether continuation is portable.
        crate::session::portable(&mut messages);
    }
    *lock(&reply.used) = Some(crate::selection::Reference::new(
        &profile,
        model.clone(),
        credential.as_ref(),
    ));
    *lock(&reply.protocol) = Some(selection.protocol);
    let session = if chatgpt {
        Some(
            crate::auth::chatgpt::Session::new(
                store.clone(),
                profile.name().into(),
                credential
                    .as_ref()
                    .ok_or_else(|| TurnFailure::new("missing ChatGPT credential"))?,
                runtime.login_http.clone(),
            )
            .map_err(|e| TurnFailure::new(e.to_string()))?,
        )
    } else {
        None
    };
    let observed_credential = credential.clone();
    let observed_profile = profile.clone();
    let observed_model = model.clone();
    let copilot_session = if profile.is_copilot() {
        Some(
            crate::auth::copilot::Session::prepare(
                &profile,
                credential
                    .clone()
                    .ok_or_else(|| TurnFailure::new("missing Copilot credential"))?,
                matches!(
                    profile.credential(),
                    Some(crate::provider::CredentialSource::Store { .. })
                )
                .then(|| store.clone()),
                reply.id.clone(),
                runtime.login_http.as_ref(),
            )
            .await
            .map_err(|error| {
                if crate::auth::copilot::unauthorized(&error) {
                    reply.selection_failed(crate::selection::Reason::CredentialsUnavailable);
                    if let Some(discovery) = &runtime.discovery {
                        discovery.invalidate_account(&profile, credential.as_ref());
                    }
                }
                TurnFailure::new(error.to_string())
            })?,
        )
    } else {
        None
    };
    let credential = credential.map(|value| value.secret().clone());
    let mut model = crate::llm::Model::new(profile, model, credential)
        .map_err(|error| TurnFailure::new(error.to_string()))?
        .with_selection(selection)
        .with_conversation_id(reply.id.clone())
        .with_cache_scope(lock(&reply.sessions).profile_name().to_owned());
    if let Some(session) = session {
        model = model.with_chatgpt_session(session);
    }
    if let Some(session) = cloud_session {
        model = model.with_google_cloud_session(session);
    }
    if let Some(session) = copilot_session {
        model = model.with_copilot_session(session);
    }
    #[cfg(test)]
    if let Some(http) = &runtime.inference_http {
        model = model.with_test_http(http.clone());
    }
    let guidance = if let Some(sources) = &runtime.instructions {
        Some(
            sources
                .start(&cwd, instructions)
                .await
                .map_err(|error| TurnFailure::new(error.to_string()))?,
        )
    } else {
        None
    };
    if let Some(guidance) = &guidance {
        for diagnostic in guidance.diagnostics.iter().take(16) {
            reply.diagnostic(diagnostic);
        }
        if guidance.diagnostics.len() > 16 {
            reply.diagnostic(&crate::instructions::Diagnostic::new(
                "bee::skills::warnings",
                std::path::Path::new("<discovery>"),
                "additional discovery warnings omitted after 16 notices",
                "fix the reported sources and retry to see remaining warnings",
            ));
        }
    }
    let cancellation = tokio_util::sync::CancellationToken::new();
    // ACP owns the terminal update; dropping its future also signals tool observers.
    let _guard = cancellation.clone().drop_guard();
    let artifacts = reply.clone();
    let context = crate::tool::Context::new(cwd, cancellation, crate::tool::MAX_OUTPUT_BYTES)
        .map_err(|error| TurnFailure::new(error.to_string()))?
        .with_artifacts(move |id, artifact| artifacts.artifact(id, artifact));
    if runtime
        .tools
        .definitions()
        .any(|tool| matches!(tool.name.as_str(), "ask_user" | "update_plan"))
    {
        return Err(TurnFailure::new(
            "ask_user and update_plan are reserved ACP tools; rename the custom tool",
        ));
    }
    let native = interactions::Native {
        inner: crate::agent::NativeExecutor::new(&runtime.tools, context),
        reply: &reply,
        permissions: &runtime.permissions,
    };
    let catalog = mcp::Catalog::discover(
        &reply.connection,
        &reply.mcp,
        &reply.servers,
        native.definitions(),
    )
    .await
    .map_err(TurnFailure::new)?;
    let executor = mcp::Composite {
        native,
        catalog,
        connection: &reply.connection,
        bindings: &reply.mcp,
    };
    let outcome = crate::agent::run_observed(
        &model,
        messages,
        executor.definitions(),
        Some(&executor),
        &mut |step, event| match event {
            crate::llm::Stream::Text(text) => reply.text(text),
            crate::llm::Stream::Thought { item, part, text } => {
                // Native adapters validate bounded coordinates before entering this display-only lane.
                reply.output(Some(crate::history::ThoughtId { step, item, part }), text);
            }
        },
        runtime.limits,
        guidance,
        Some(&reply),
    )
    .await
    .map_err(|error| {
        if error == crate::llm::Error::Authentication {
            reply.selection_failed(crate::selection::Reason::CredentialsUnavailable);
            if let Some(discovery) = &runtime.discovery {
                discovery.invalidate_account(&observed_profile, observed_credential.as_ref());
            }
        }
        TurnFailure::new(error.to_string())
    })?;
    if let Some(discovery) = &runtime.discovery {
        discovery.record_inference_success(
            &observed_profile,
            observed_credential.as_ref(),
            &observed_model,
        );
    }
    *lock(&reply.completed) = Some(outcome);
    Ok(())
}

/// Run the agent over `transport` until the peer disconnects.
///
/// `providers` are the configured provider profiles the sessions choose among
/// (ADR-007).
///
/// # Errors
///
/// Returns [`Error::Connection`] if the connection ends with a protocol or
/// transport error, or [`Error::Tool`] if the shipped catalog cannot register.
/// A peer that simply closes its end is not an error.
pub async fn serve(transport: impl ConnectTo<Agent>, providers: Providers) -> Result<()> {
    serve_with_runtime(
        transport,
        providers,
        Runtime::builtins(crate::agent::Limits::default())?,
    )
    .await
}

/// Serve with caller-owned execution limits and native catalog.
///
/// # Errors
///
/// Returns [`Error::Connection`] if the transport ends with a protocol error.
pub async fn serve_with_runtime(
    transport: impl ConnectTo<Agent>,
    providers: Providers,
    runtime: Runtime,
) -> Result<()> {
    // `agent()` is `ConnectTo<Client>`, and the transport is the client's end,
    // so it is the transport that the agent connects to.
    agent_with_runtime(providers, runtime)
        .connect_to(transport)
        .await
        .map_err(|error| Error::Connection {
            // Stringified so the SDK's error type stays inside this module.
            reason: error.to_string(),
        })
}

/// Run the agent over the process's stdin and stdout.
///
/// # Errors
///
/// As [`serve`].
pub async fn serve_stdio(providers: Providers) -> Result<()> {
    // No debug callback on purpose: stdout carries protocol traffic only, and a
    // trace line written there would corrupt the client's JSON-RPC stream.
    serve(Stdio::new(), providers).await
}

/// Serve stdio with validated policy and a precompiled catalog.
///
/// # Errors
///
/// As [`serve_with_runtime`].
pub async fn serve_stdio_with_runtime(providers: Providers, runtime: Runtime) -> Result<()> {
    serve_with_runtime(Stdio::new(), providers, runtime).await
}

/// Serve a deterministic scenario over a real ACP transport with memory-only state.
///
/// # Errors
///
/// Returns [`Error::Connection`] if the protocol connection fails.
pub async fn serve_scenario(
    transport: impl ConnectTo<Agent>,
    scenario: crate::scenario::Scenario,
) -> Result<()> {
    agent_with_scenario(scenario)
        .connect_to(transport)
        .await
        .map_err(|error| Error::Connection {
            reason: error.to_string(),
        })
}

/// Serve a deterministic scenario over stdin/stdout.
///
/// # Errors
///
/// As [`serve_scenario`].
pub async fn serve_stdio_scenario(scenario: crate::scenario::Scenario) -> Result<()> {
    serve_scenario(Stdio::new(), scenario).await
}

/// Build a service-free scenario agent using the ordinary session handlers.
pub fn agent_with_scenario(scenario: crate::scenario::Scenario) -> impl ConnectTo<Client> {
    agent_with(move |reply| scenarios::run(scenario, reply))
}

/// Build the ACPv2 agent side of a connection.
///
/// It serves the baseline session lifecycle (`session/new`, `list`, `resume`,
/// `close`, `prompt`, `cancel`) and streams configured model responses.
///
/// A running prompt can be stopped with `session/cancel` or `session/close`,
/// and a second prompt is refused until it ends (ADR-005).
///
/// The result connects to any [`Client`] transport, which is how the tests
/// drive it in-process without a subprocess. Each call builds its own session
/// registry, so two connections never share a session.
pub fn agent() -> impl ConnectTo<Client> {
    agent_with_accounts(Providers::default())
}

/// [`agent`], with sessions that choose among `providers`.
///
/// Each session is offered a `provider` and a `model` configuration option,
/// and `session/set_config_option` changes them for that session only. The
/// choice is a session setting; the profile's endpoint, headers and credential
/// are configuration, and no request can change them (ADR-007).
pub fn agent_with_accounts(providers: Providers) -> impl ConnectTo<Client> {
    // Fixed shipped schemas failing registration is an internal programming error.
    agent_with_runtime(
        providers,
        Runtime::builtins(crate::agent::Limits::default()).expect("valid shipped tool schemas"),
    )
}

/// Build sessions using the supplied complete native catalog and turn budgets.
pub fn agent_with_runtime(providers: Providers, runtime: Runtime) -> impl ConnectTo<Client> {
    let runtime = Arc::new(runtime);
    let discovery = runtime.discovery.clone();
    let history = runtime.history.clone();
    let history_settings = runtime.history_settings;
    let environment = runtime
        .auth_environment
        .clone()
        .unwrap_or_else(crate::config::Environment::from_process);
    build(
        providers,
        discovery,
        environment,
        history,
        history_settings,
        runtime.preferences.clone(),
        move |reply| inference(reply, Arc::clone(&runtime)),
    )
}

/// [`agent`], with `body` as the turn that answers every prompt.
///
/// Not part of the stable API: lifecycle tests inject deterministic turn bodies.
#[doc(hidden)]
pub fn agent_with<F, Fut>(body: F) -> impl ConnectTo<Client>
where
    F: Fn(Reply) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<(), TurnFailure>> + Send + 'static,
{
    build(
        Providers::default(),
        None,
        crate::config::Environment::default(),
        None,
        Default::default(),
        Default::default(),
        body,
    )
}

/// [`agent_with`], with sessions that choose among `providers`.
///
/// Not part of the stable API, for the same reason as [`agent_with`]: a test of
/// changing settings mid-turn needs a turn that does not end.
#[doc(hidden)]
pub fn agent_configured<F, Fut>(providers: Providers, body: F) -> impl ConnectTo<Client>
where
    F: Fn(Reply) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<(), TurnFailure>> + Send + 'static,
{
    build(
        providers,
        None,
        crate::config::Environment::default(),
        None,
        Default::default(),
        Default::default(),
        body,
    )
}

fn build<F, Fut>(
    providers: Providers,
    discovery: Option<Arc<crate::models::Service>>,
    environment: crate::config::Environment,
    history: Option<crate::history::Store>,
    history_settings: crate::history::Settings,
    preferences: crate::selection::Store,
    body: F,
) -> impl ConnectTo<Client>
where
    F: Fn(Reply) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<(), TurnFailure>> + Send + 'static,
{
    let body = Arc::new(body);
    let endpoint_profiles: Vec<_> = providers
        .names()
        .filter_map(|name| providers.get(name))
        .filter(|profile| profile.discovery())
        .cloned()
        .collect();
    let sessions = Arc::new(Mutex::new(
        Sessions::with_accounts(providers)
            .with_discovery(discovery.clone())
            .with_preferences(preferences.clone(), environment.clone()),
    ));
    let mcp = Arc::new(Mutex::new(mcp::Bindings::default()));
    let tasks: Tasks = Arc::default();
    let gates: Gates = Arc::default();
    let replay_budget = Arc::new(crate::history::ReplayBudget::new(history_settings));
    let interactions = Arc::new(Mutex::new(interactions::State::default()));
    let discovery_started = std::sync::atomic::AtomicBool::new(false);

    // `Agent.v2()`, not `Agent.builder()`: the latter is the stable v1 surface.
    Agent
        .v2()
        .name("bee")
        .on_receive_request(
            {
                let interactions = Arc::clone(&interactions);
                let discovery = discovery.clone();
                let sessions = Arc::clone(&sessions);
                let gates = Arc::clone(&gates);
                let tasks = Arc::clone(&tasks);
                let environment = environment.clone();
                async move |request: v2::InitializeRequest,
                            responder: Responder<v2::InitializeResponse>,
                            connection: V2ConnectionTo<Client>| {
                    let discovery = discovery.clone();
                    let sessions = Arc::clone(&sessions);
                    let gates = Arc::clone(&gates);
                    let tasks = Arc::clone(&tasks);
                    let endpoint_profiles = endpoint_profiles.clone();
                    let environment = environment.clone();
                    {
                        let mut state = lock(&interactions);
                        state.form = request
                            .capabilities
                            .elicitation
                            .as_ref()
                            .is_some_and(v2::ElicitationCapabilities::supports_form);
                        state.url = request
                            .capabilities
                            .elicitation
                            .as_ref()
                            .is_some_and(v2::ElicitationCapabilities::supports_url);
                    }
                    // Advertise only transports we actually route, not HTTP or subprocess MCP.
                    responder.respond(
                        v2::InitializeResponse::new(
                            // Hand back the version the SDK gives us rather than
                            // choosing one: the v2 builder negotiates the version on
                            // the wire, so a hard-coded value here would fight it.
                            request.protocol_version,
                            v2::Implementation::new("bee", env!("CARGO_PKG_VERSION")),
                        )
                        .capabilities(
                            v2::AgentCapabilities::new().session(
                                v2::SessionCapabilities::new().mcp(
                                    v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new()),
                                ),
                            ),
                        ),
                    )?;
                    if let Some(discovery) = discovery.filter(|_| {
                        !discovery_started.swap(true, std::sync::atomic::Ordering::SeqCst)
                    }) {
                        let mut changes = discovery.subscribe();
                        let worker = Arc::clone(&discovery);
                        connection.spawn(async move {
                            worker.run().await;
                            Ok(())
                        })?;
                        for profile in endpoint_profiles {
                            let worker = Arc::clone(&discovery);
                            let environment = environment.clone();
                            connection.spawn(async move {
                                worker.run_endpoint(&profile, &environment).await;
                                Ok(())
                            })?;
                        }
                        let updates = connection.clone();
                        connection.spawn(async move {
                            let mut sent = BTreeMap::new();
                            while changes.changed().await.is_ok() {
                                let ids = lock(&sessions).open_ids();
                                for id in ids {
                                    let Ok(_gate) = session_gate(&gates, &id).try_enter() else { continue; };
                                    // Hold through publication so pending settings saves never leak a choice.
                                    if refresh_recent(&sessions, &tasks, &id).await.is_err() { continue; }
                                    let sessions = lock(&sessions);
                                    let Some(settings) = sessions.open_settings_for(&id) else { continue; };
                                    let options = config_options(Some(settings));
                                    let encoded =
                                        serde_json::to_value(&options).unwrap_or_default();
                                    if sent.get(&id) != Some(&encoded) {
                                        if let Some(reason) = sessions.selection_notice(&id) { send_selection_notice(&updates, &id, reason); }
                                        let _ = send_update(
                                            &updates,
                                            &id.clone().into(),
                                            v2::SessionUpdate::ConfigOptionUpdate(
                                                v2::ConfigOptionUpdate::new(options),
                                            ),
                                        );
                                        sent.insert(id, encoded);
                                    }
                                }
                            }
                            Ok(())
                        })?;
                    }
                    Ok(())
                }
            },
            // The SDK requires this macro to tie the handler to the request type.
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let mcp = Arc::clone(&mcp);
                let history = history.clone();
                let gates = Arc::clone(&gates);
                let tasks = Arc::clone(&tasks);
                async move |request: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    let servers = match mcp::declarations(request.mcp_servers) {
                        Ok(servers) => servers,
                        Err(reason) => return responder.respond_with_error(invalid_params(reason)),
                    };
                    if let Some(store) = history.clone() {
                        let sessions = Arc::clone(&sessions);
                        let mcp = Arc::clone(&mcp);
                        let gates = Arc::clone(&gates);
                        let tasks = Arc::clone(&tasks);
                        let updates = connection.clone();
                        return connection.spawn(async move {
                            let (provider, model, selection_state) = lock(&sessions).initial_selection();
                            let snapshot = crate::history::Snapshot {
                                profile: lock(&sessions).profile_name().to_owned(),
                                cwd: request.cwd.into_inner(), turns: 0, provider, model,
                                selection_state,
                                protocol: None, messages: Vec::new(), instructions: Default::default(),
                            };
                            let (id, handle) = match store.create(snapshot.clone()).await {
                                Ok(created) => created,
                                Err(error) => return responder.respond_with_error(history_error(error)),
                            };
                            // Reserve before exposing the new registry entry to discovery publications.
                            let _gate = session_gate(&gates, &id).try_enter().expect("new session gate");
                            {
                                let mut sessions = lock(&sessions);
                                sessions.restore(id.clone(), snapshot, handle);
                                lock(&mcp).sessions.insert(id.clone(), servers);
                            }
                            if let Err(error) = refresh_recent(&sessions, &tasks, &id).await {
                                return responder.respond_with_error(history_error(error));
                            }
                            let options = config_options(lock(&sessions).settings(&id).ok().flatten());
                            selection_notice(&sessions, &updates, &id);
                            drop(_gate);
                            responder.respond(v2::NewSessionResponse::new(id).config_options(options))
                        });
                    }
                    // One guard, so the options describe the session just made.
                    let mut sessions = lock(&sessions);
                    let id = sessions.create(request.cwd.into_inner());
                    lock(&mcp).sessions.insert(id.clone(), servers);
                    let options = config_options(sessions.settings(&id).ok().flatten());
                    if let Some(reason) = sessions.selection_notice(&id) { send_selection_notice(&connection, &id, reason); }
                    responder.respond(v2::NewSessionResponse::new(id).config_options(options))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let history = history.clone();
                async move |request: v2::ListSessionsRequest,
                            responder: Responder<v2::ListSessionsResponse>,
                            connection: V2ConnectionTo<Client>| {
                    // Bee never returns a `nextCursor`, so a cursor can only be
                    // made up; silently ignoring it would hide a client bug.
                    if request.cursor.is_some() {
                        return responder.respond_with_error(invalid_params(
                            "`cursor` is not supported: `session/list` returns a single page",
                        ));
                    }
                    if let Some(store) = history.clone() {
                        let profile = lock(&sessions).profile_name().to_owned();
                        return connection.spawn(async move {
                            let listed = match store.list().await {
                                Ok(listed) => listed,
                                Err(error) => return responder.respond_with_error(history_error(error)),
                            };
                            responder.respond(v2::ListSessionsResponse::new(listed.into_iter()
                                .filter(|session| session.profile == profile)
                                .filter(|session| request.cwd.as_ref().is_none_or(|cwd| AsRef::<Path>::as_ref(cwd) == session.cwd))
                                .map(|session| v2::SessionInfo::new(session.id, session.cwd)).collect()))
                        });
                    }
                    let listed = lock(&sessions).list(request.cwd.as_ref().map(AsRef::as_ref));
                    responder.respond(v2::ListSessionsResponse::new(
                        listed
                            .into_iter()
                            .map(|session| v2::SessionInfo::new(session.id, session.cwd))
                            .collect(),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let mcp = Arc::clone(&mcp);
                let tasks = Arc::clone(&tasks);
                let interactions = Arc::clone(&interactions);
                let history = history.clone();
                let gates = Arc::clone(&gates);
                let replay_budget = Arc::clone(&replay_budget);
                async move |request: v2::ResumeSessionRequest,
                            responder: Responder<v2::ResumeSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    // Start replays all retained ACP history, which may be empty:
                    // model context is separate from retained protocol updates.
                    // Unknown cursors must fail before changing session state.
                    if !matches!(request.replay_from, None | Some(v2::ReplayFrom::Start(_))) {
                        return responder.respond_with_error(invalid_params(
                            "unsupported `replayFrom` cursor: omit it or use {\"type\":\"start\"}",
                        ));
                    }
                    let id = request.session_id.to_string();
                    let gate = match session_gate(&gates, &id).try_enter() {
                        Ok(gate) => gate,
                        Err(_) => return responder.respond_with_error(session_error(SessionError::Busy(id))),
                    };
                    let servers = match mcp::declarations(request.mcp_servers) {
                        Ok(servers) => servers,
                        Err(reason) => return responder.respond_with_error(invalid_params(reason)),
                    };
                    let cwd: &Path = request.cwd.as_ref();
                    if let Some(store) = history.clone() {
                        let sessions = Arc::clone(&sessions);
                        let mcp = Arc::clone(&mcp);
                        let tasks = Arc::clone(&tasks);
                        let interactions = Arc::clone(&interactions);
                        let updates = connection.clone();
                        let replay_budget = Arc::clone(&replay_budget);
                        return connection.spawn(async move {
                            let _gate = gate;
                            if lock(&sessions).history(&id).is_none() {
                                let (snapshot, handle, interrupted) = match store.attach(&id).await {
                                    Ok(loaded) => loaded,
                                    Err(error) => return responder.respond_with_error(history_error(error)),
                                };
                                let validation = lock(&sessions).validate_snapshot(&snapshot);
                                if let Err(error) = validation {
                                    // Release before responding so the owning profile can
                                    // immediately retry; rejected resumes must not repair history.
                                    if let Err(error) = handle.release().await {
                                        return responder.respond_with_error(history_error(error));
                                    }
                                    return responder.respond_with_error(session_error(error));
                                }
                                if snapshot.cwd != AsRef::<Path>::as_ref(&request.cwd) {
                                    // Release before rejecting, so a corrected
                                    // request does not race this worker's Drop.
                                    if let Err(error) = handle.release().await {
                                        return responder.respond_with_error(history_error(error));
                                    }
                                    return responder.respond_with_error(session_error(SessionError::CwdMismatch {
                                        id, expected: snapshot.cwd.display().to_string(), actual: AsRef::<Path>::as_ref(&request.cwd).display().to_string(),
                                    }));
                                }
                                if let Err(error) = handle.recover().await {
                                    let _ = handle.release().await;
                                    return responder.respond_with_error(history_error(error));
                                }
                                lock(&sessions).restore(id.clone(), snapshot, handle);
                                if interrupted {
                                    let _ = send_update(&updates, &request.session_id, v2::SessionUpdate::Notice(
                                        v2::Notice::new(v2::NoticeSeverity::Warning, "Previous session activity was interrupted")
                                            .description("Retained complete journal records and partial conversation; no model or tool execution was restarted.")));
                                }
                            }
                            let handle = {
                                let mut sessions = lock(&sessions);
                                if let Err(error) = sessions.resume(&id, request.cwd.as_ref()) {
                                    return responder.respond_with_error(session_error(error));
                                }
                                sessions.history(&id).expect("durable attachment")
                            };
                            if let Err(error) = refresh_recent(&sessions, &tasks, &id).await {
                                return responder.respond_with_error(history_error(error));
                            }
                            if matches!(request.replay_from, Some(v2::ReplayFrom::Start(_))) {
                                let replay_connection = updates.clone();
                                let replay_id = request.session_id.clone();
                                let plan_connection = updates.clone();
                                let plan_id = request.session_id.clone();
                                let presentation_connection = updates.clone();
                                let presentation_id = request.session_id.clone();
                                if let Err(error) = handle.replay_all(replay_budget, move |message| replay_message(&replay_connection, &replay_id, message), move |turn, plan| {
                                    send_update(&plan_connection, &plan_id, plans::update(turn, plan))
                                        .map_err(|_| crate::history::Error { path: "<session plan>".into(), reason: "cannot restore execution plan", help: "reconnect and resume the session" })
                                }, move |value| {
                                    send_update(&presentation_connection, &presentation_id, presentation::update(value))
                                        .map_err(|_| crate::history::Error { path: "<session tools>".into(), reason: "cannot restore tool presentation", help: "reconnect and resume the session" })
                                }).await {
                                    return responder.respond_with_error(history_error(error));
                                }
                            }
                            handle.publish(move || {
                                let sessions = lock(&sessions);
                                lock(&mcp).sessions.insert(id.clone(), servers);
                                // Compute current state on the same publication
                                // lane as terminal idle, never from a stale snapshot.
                                let state = if lock(&tasks).contains_key(&id) {
                                    interactions::phase(lock(&interactions).waiting.contains_key(&id))
                                } else { idle(None) };
                                let options = config_options(sessions.settings(&id).ok().flatten());
                                if let Some(reason) = sessions.selection_notice(&id) { send_selection_notice(&updates, &id, reason); }
                                // Release on this publication lane, before the
                                // client can observe the snapshot/response. The
                                // awaiting continuation may not be scheduled yet.
                                drop(_gate);
                                let _ = send_update(&updates, &request.session_id, state);
                                let _ = responder.respond(v2::ResumeSessionResponse::new().config_options(options));
                            }).await;
                            Ok(())
                        });
                    }
                    let options = {
                        let mut sessions = lock(&sessions);
                        if let Err(error) = sessions.resume(&id, cwd) {
                            return responder.respond_with_error(session_error(error));
                        }
                        if !lock(&tasks).contains_key(&id) {
                            sessions.refresh_recent(&id);
                        }
                        // Always replace, including an empty list: old routes are not authorization.
                        lock(&mcp).sessions.insert(id.clone(), servers);
                        if matches!(request.replay_from, Some(v2::ReplayFrom::Start(_))) {
                            let presentation_state = sessions.presentation(&id);
                            let presentation = lock(&presentation_state);
                            let tool_updates = presentation.snapshots();
                            let plan_bytes = sessions.plans(&id).try_fold(0usize, |bytes, (_, plan)| {
                                bytes.checked_add(crate::history::plan_replay_bytes(plan)?).ok_or(crate::history::Error {
                                    path: "<session plan>".into(), reason: "plan replay exceeds the byte budget", help: "increase the history replay byte budget",
                                })
                            });
                            if let Err(error) = plan_bytes.and_then(|bytes| replay_budget.reserve(bytes.saturating_add(tool_updates.iter().map(|v| v.to_string().len() + 256).sum::<usize>()))) {
                                return responder.respond_with_error(history_error(error));
                            }
                            if let Err(error) = replay_budget.reserve_messages(sessions.transcript(&id)) {
                                return responder.respond_with_error(history_error(error));
                            }
                            for message in sessions.transcript(&id) {
                                if let Err(error) = replay_message(&connection, &request.session_id, message.clone()) {
                                    return responder.respond_with_error(history_error(error));
                                }
                            }
                            for (turn, plan) in sessions.plans(&id) {
                                send_update(&connection, &request.session_id, plans::update(turn, plan.clone()))?;
                            }
                            for value in tool_updates {
                                send_update(&connection, &request.session_id, presentation::update(value))?;
                            }
                        }
                        // The session keeps what the client chose before it
                        // was closed, so the options report that, not the defaults.
                        let state = if lock(&tasks).contains_key(&id) {
                            interactions::phase(lock(&interactions).waiting.contains_key(&id))
                        } else {
                            idle(None)
                        };
                        // Snapshot and send share the session lock so a finishing
                        // turn cannot send idle before a stale running snapshot.
                        send_update(&connection, &request.session_id, state)?;
                        if let Some(reason) = sessions.selection_notice(&id) { send_selection_notice(&connection, &id, reason); }
                        config_options(sessions.settings(&id).ok().flatten())
                    };
                    // The protocol requires a current-state snapshot before the
                    // response: the response alone does not imply idle.
                    drop(gate);
                    responder.respond(v2::ResumeSessionResponse::new().config_options(options))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let tasks = Arc::clone(&tasks);
                let gates = Arc::clone(&gates);
                let selection_discovery = discovery.clone();
                let selection_environment = environment.clone();
                async move |request: v2::SetSessionConfigOptionRequest,
                            responder: Responder<v2::SetSessionConfigOptionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    // Every option Bee offers is a selector. Anything else is a
                    // client that did not read the options it was given.
                    let Some(value) = request.value.as_id() else {
                        return responder.respond_with_error(invalid_params(
                            "this option takes a selected value (`type: \"id\"`), not a boolean",
                        ));
                    };
                    let value = value.to_string();
                    let id = request.session_id.to_string();
                    let gate = match session_gate(&gates, &id).try_enter() {
                        Ok(gate) => gate,
                        Err(_) => return responder.respond_with_error(session_error(SessionError::SettingsBusy(id))),
                    };

                    if lock(&tasks).contains_key(&id) {
                        return responder
                            .respond_with_error(session_error(SessionError::SettingsBusy(id)));
                    }
                    if request.config_id.to_string() != MODEL_OPTION {
                        return responder.respond_with_error(invalid_params("unknown configuration option; Bee offers `model`"));
                    }
                    let account = match lock(&sessions).selection_account(&id, &value) {
                        Ok(account) => account,
                        Err(error) => return responder.respond_with_error(session_error(error)),
                    };
                    let (selection_abort, selection_registration) = AbortHandle::new_pair();
                    *lock(&gate._gate.selection_abort) = Some(selection_abort.clone());
                    let discovery = selection_discovery.clone();
                    let environment = selection_environment.clone();
                    let sessions = Arc::clone(&sessions);
                    let tasks = Arc::clone(&tasks);
                    // Dispatch remains free while native discovery performs cancellable I/O.
                    connection.spawn(async move {
                        let _gate = gate;
                        if account.discovery()
                            && let Some(discovery) = discovery {
                                let credential = crate::selection::credential(&account, &environment).ok().flatten();
                                let snapshot = discovery.account_snapshot(&account, credential.as_ref());
                                if !snapshot.endpoints.get(account.name()).is_some_and(|listing| listing.fetched_at.is_some()) {
                                    let attempt = discovery.list_endpoint(&account, &environment);
                                    if futures::future::Abortable::new(attempt, selection_registration).await.is_err() {
                                        return responder.respond_with_error(invalid_params("model selection was cancelled; choose a model again"));
                                    }
                                }
                        }
                        if selection_abort.is_aborted() || _gate._gate.closing.load(std::sync::atomic::Ordering::Acquire) != 0 {
                            return responder.respond_with_error(invalid_params("session is closing"));
                        }
                        let pending = {
                            let mut registry = lock(&sessions);
                            if lock(&tasks).contains_key(&id) { return responder.respond_with_error(session_error(SessionError::SettingsBusy(id))); }
                            let previous = registry.snapshot(&id);
                            if let Err(error) = registry.set_model(&id, &value) { return responder.respond_with_error(session_error(error)); }
                            registry.history(&id).map(|handle| (handle, registry.snapshot(&id).expect("existing session"), previous.expect("existing session")))
                        };
                        if let Some((handle, snapshot, previous)) = pending
                            && let Err(error) = handle.save(snapshot).await {
                                lock(&sessions).rollback(&id, previous);
                                return responder.respond_with_error(history_error(error));
                        }
                        let options = config_options(lock(&sessions).settings(&id).ok().flatten());
                        responder.respond(v2::SetSessionConfigOptionResponse::new(options))
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let mcp = Arc::clone(&mcp);
                let tasks = Arc::clone(&tasks);
                let gates = Arc::clone(&gates);
                async move |request: v2::CloseSessionRequest,
                            responder: Responder<v2::CloseSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    let closing = session_gate(&gates, &request.session_id.to_string()).close();
                    // Reserve immediately when possible: a later prompt must not
                    // overtake a close merely because its continuation is pending.
                    connection.spawn(close_session(
                        Arc::clone(&sessions), Arc::clone(&mcp), Arc::clone(&tasks),
                        closing, request, responder,
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let mcp = Arc::clone(&mcp);
                let tasks = Arc::clone(&tasks);
                let interactions = Arc::clone(&interactions);
                let gates = Arc::clone(&gates);
                let preferences = preferences.clone();
                async move |request: v2::PromptRequest,
                            responder: Responder<v2::PromptResponse>,
                            connection: V2ConnectionTo<Client>| {
                    // Before anything is inserted: a prompt that carries content
                    // Bee recognizes but did not advertise support for is a bad
                    // request. Opaque future content is retained, not interpreted.
                    if let Some(reason) = unsupported_content(&request.prompt) {
                        return responder.respond_with_error(invalid_params(reason));
                    }

                    // Registered here, in the handler, and not in the spawned
                    // task: the SDK dispatches a `session/cancel` as soon as this
                    // handler returns, and a turn the registry does not know yet
                    // could not be cancelled.
                    let (abort, registration) = AbortHandle::new_pair();
                    let id = request.session_id.to_string();
                    let gate = match session_gate(&gates, &id).try_enter() {
                        Ok(gate) => gate,
                        Err(_) => return responder.respond_with_error(session_error(SessionError::Busy(id))),
                    };
                    let mut registry = lock(&sessions);
                    if lock(&tasks).contains_key(&id) {
                        return responder.respond_with_error(session_error(SessionError::Busy(id)));
                    }
                    let turn = match registry.begin_turn(&id, move || abort.abort()) {
                        Ok(turn) => turn,
                        // A prompt refused before insertion is an error
                        // response, not an idle update: no turn ever started.
                        Err(error) => return responder.respond_with_error(session_error(error)),
                    };
                    let mut input = registry.inference(&id);
                    // Session lock precedes MCP lock everywhere; snapshot before accepting changes.
                    let servers = lock(&mcp).sessions.get(&id).cloned().unwrap_or_default();
                    if let Some(input) = &mut input {
                        let content = prompt_text(&request.prompt);
                        input.messages.push(crate::llm::Message::User(content));
                    }
                    let durable = registry.history(&id);
                    if let Some(history) = &durable
                        && let Err(error) = history.healthy() {
                            registry.finish_turn(&id, turn.token);
                            return responder.respond_with_error(history_error(error));
                    }
                    drop(registry);
                    let session_id = request.session_id;

                    let (finished, completion) = oneshot::channel();
                    lock(&tasks).insert(id.clone(), async move { let _ = completion.await; }.boxed().shared());
                    let sessions = Arc::clone(&sessions);
                    let tasks = Arc::clone(&tasks);
                    let mcp = Arc::clone(&mcp);
                    let interactions = Arc::clone(&interactions);
                    let body = Arc::clone(&body);
                    let preferences = preferences.clone();
                    let updates = connection.clone();
                    connection.spawn(async move {
                    let connection = updates;
                    let content = match serde_json::to_value(&request.prompt) {
                        Ok(content) => content,
                        Err(_) => {
                            lock(&sessions).finish_turn(&id, turn.token);
                            lock(&tasks).remove(&id);
                            let _ = finished.send(());
                            return responder.respond_with_error(invalid_params("cannot encode prompt content"));
                        }
                    };
                    if let Some(history) = &durable
                        && let Err(error) = history.accept(turn.token, content.clone()).await {
                            lock(&sessions).finish_turn(&id, turn.token);
                            lock(&tasks).remove(&id);
                            let _ = finished.send(());
                            return responder.respond_with_error(history_error(error));
                    }
                    lock(&sessions).record_user(&id, turn.user_message_id.clone(), content);
                    // The accepted turn's busy barrier now protects execution;
                    // do not keep a request reservation after acknowledging it.
                    drop(gate);

                    // Acceptance only: the response says the user message was
                    // inserted, not that anything has been answered. Everything
                    // after it travels as `session/update`.
                    if let Err(error) =
                        responder.respond(v2::PromptResponse::new(turn.user_message_id.clone()))
                    {
                        // The turn was registered but will never run; leaving it
                        // would keep the session busy for good.
                        lock(&sessions).finish_turn(&id, turn.token);
                        lock(&tasks).remove(&id);
                        let _ = finished.send(());
                        return Err(error);
                    }
                    // Both updates precede invoking the cancellable body, so
                    // even cancellation during durable acceptance ends after them.
                    // Send failures mean the peer is gone and the response is
                    // already out: returning them would make the SDK answer the
                    // same request a second time.
                    let acceptance_connection = connection.clone();
                    let acceptance_id = session_id.clone();
                    let user_message_id = turn.user_message_id.clone();
                    let publish_acceptance = move || {
                    let _ = send_update(
                        &acceptance_connection,
                        &acceptance_id,
                        v2::SessionUpdate::UserMessage(
                            // The same id the response returned, so a client can
                            // match the echoed message to its prompt.
                            v2::UserMessage::new(user_message_id).content(request.prompt),
                        ),
                    );
                    let _ = send_update(
                        &acceptance_connection,
                        &acceptance_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                            v2::RunningStateUpdate::new(),
                        )),
                    );
                    };
                    if let Some(history) = durable {
                        let _ = history.enqueue(publish_acceptance);
                    } else { publish_acceptance(); }

                    let retained_presentation = lock(&sessions).presentation(&id);
                    let presentation_history = lock(&sessions).history(&id);
                    let reply = Reply {
                        sessions: Arc::clone(&sessions),
                        connection: connection.clone(),
                        id: id.clone(),
                        session_id,
                        message_id: turn.agent_message_id,
                        token: turn.token,
                        protocol: Arc::new(Mutex::new(input.as_ref().and_then(|input| input.protocol))),
                        input,
                        completed: Arc::new(Mutex::new(None)),
                        used: Arc::new(Mutex::new(None)),
                        selection_failure: Arc::new(Mutex::new(None)),
                        preferences,
                        plan: Arc::new(Mutex::new(None)),
                        presentation: Arc::new(Mutex::new(presentation::Ledger::new(retained_presentation))),
                        presentation_history,
                        mcp: Arc::clone(&mcp),
                        servers,
                        interactions: Arc::clone(&interactions),
                    };
                    // Continue off dispatch in the task that persisted prompt
                    // acceptance, so cancellation and close remain serviceable.
                    run_turn(
                        Arc::clone(&body),
                        reply,
                        registration,
                        Arc::clone(&tasks),
                        finished,
                    ).await
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = Arc::clone(&sessions);
                let gates = Arc::clone(&gates);
                async move |notification: v2::CancelSessionNotification,
                            _connection: V2ConnectionTo<Client>| {
                    // Cancelling an idle or unknown session is not an error: a
                    // notification has no reply to carry one, and the protocol
                    // asks for no update when there was no work to cancel.
                    // The turn task owns terminal idle, after abort has dropped
                    // its future and all pending outgoing requests.
                    let id = notification.session_id.to_string();
                    if let Some(abort) = lock(&session_gate(&gates, &id).selection_abort).take() { abort.abort(); }
                    lock(&sessions).cancel(&id);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// Caller owns either the settings gate or the current turn's busy barrier.
async fn refresh_recent(
    sessions: &Arc<Mutex<Sessions>>,
    tasks: &Tasks,
    id: &str,
) -> std::result::Result<(), crate::history::Error> {
    let pending = {
        let mut registry = lock(sessions);
        // Cancellation revokes model output before the terminal journal write.
        // The task remains busy until idle, even when Session::active is already empty.
        if lock(tasks).contains_key(id) {
            return Ok(());
        }
        let previous = registry.snapshot(id);
        let refreshed = registry.refresh_recent(id);
        if !refreshed {
            return Ok(());
        }
        registry.history(id).map(|handle| {
            (
                handle,
                registry.snapshot(id).expect("existing session"),
                previous.expect("existing session"),
            )
        })
    };
    if let Some((handle, snapshot, previous)) = pending {
        let result = handle.save(snapshot).await;
        if let Err(error) = result {
            lock(sessions).rollback(id, previous);
            return Err(error);
        }
    }
    Ok(())
}

fn send_selection_notice(connection: &V2ConnectionTo<Client>, id: &str, reason: &str) {
    let _ = send_update(
        connection,
        &id.to_owned().into(),
        v2::SessionUpdate::Notice(
            v2::Notice::new(
                v2::NoticeSeverity::Warning,
                "Recent model selection needs attention",
            )
            .description(reason),
        ),
    );
}

fn selection_notice(
    sessions: &Arc<Mutex<Sessions>>,
    connection: &V2ConnectionTo<Client>,
    id: &str,
) {
    if let Some(reason) = lock(sessions).selection_notice(id) {
        send_selection_notice(connection, id, reason);
    }
}

async fn close_session(
    sessions: Arc<Mutex<Sessions>>,
    mcp: Arc<Mutex<mcp::Bindings>>,
    tasks: Tasks,
    mut closing: Closing,
    request: v2::CloseSessionRequest,
    responder: Responder<v2::CloseSessionResponse>,
) -> agent_client_protocol::Result<()> {
    let _gate = match closing.reservation.take() {
        Some(reservation) => reservation,
        None => Arc::clone(&closing.gate.mutation).lock_owned().await,
    };
    let id = request.session_id.to_string();
    let (pending, history) = {
        let mut sessions = lock(&sessions);
        if let Err(error) = sessions.close(&id) {
            return responder.respond_with_error(session_error(error));
        }
        lock(&mcp).sessions.remove(&id);
        (lock(&tasks).get(&id).cloned(), sessions.history(&id))
    };
    // Await off dispatch: cancelling an outgoing request can itself require
    // the dispatcher, and neither idle nor ownership release may precede it.
    if let Some(pending) = pending {
        pending.await;
    }
    if let Some(history) = history {
        if let Err(error) = history.drain().await {
            return responder.respond_with_error(history_error(error));
        }
        lock(&sessions).detach(&id);
        if let Err(error) = history.release().await {
            return responder.respond_with_error(history_error(error));
        }
    }
    drop(_gate);
    drop(closing);
    responder.respond(v2::CloseSessionResponse::new())
}

/// The id of the option that selects the model.
const MODEL_OPTION: &str = "model";

/// A single qualified selector also works before any account/model is selected.
fn config_options(settings: Option<Settings>) -> Vec<v2::SessionConfigOption> {
    let Some(settings) = settings else {
        return Vec::new();
    };
    let selected = settings
        .account
        .as_ref()
        .zip(settings.model.as_ref())
        .map(|(account, model)| format!("{account}/{model}"));
    let current = selected
        .as_deref()
        .filter(|id| settings.models.iter().any(|model| model == id))
        .unwrap_or("");
    let mut choices = Vec::new();
    if current.is_empty() {
        // The SDK requires a string current value. Empty is presentation only and fails ModelRef parsing.
        choices.push(v2::SessionConfigSelectOption::new("", "Choose a model"));
    }
    choices.extend(settings.model_info.iter().map(|info| {
        let (account, raw) = info.id.split_once('/').expect("qualified projection");
        let account_label = settings
            .account_display_names
            .get(account)
            .map_or(account, String::as_str);
        let model_label = info
            .facts
            .get("display_name")
            .and_then(|fact| fact.value.as_str())
            .unwrap_or(raw);
        let label = format!("{account_label} / {model_label} ({})", info.id);
        v2::SessionConfigSelectOption::new(info.id.as_str(), label).meta(
            serde_json::json!({"bee.dev/models": info})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        )
    }));
    vec![
        v2::SessionConfigOption::select(MODEL_OPTION, "Model", current, choices)
            .description("Choose an available model and its account together.")
            .meta(
                serde_json::json!({"bee.dev/models": settings.catalog_status})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            )
            .category(v2::SessionConfigOptionCategory::Model),
    ]
}

/// Run one turn to its terminal state.
///
/// Always returns `Ok`: the SDK shuts the whole connection down when a spawned
/// task fails, and one failed turn must not take the other sessions with it.
async fn run_turn<F, Fut>(
    body: Arc<F>,
    reply: Reply,
    registration: AbortRegistration,
    tasks: Tasks,
    finished: oneshot::Sender<()>,
) -> agent_client_protocol::Result<()>
where
    F: Fn(Reply) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<(), TurnFailure>> + Send + 'static,
{
    let sessions = Arc::clone(&reply.sessions);
    let connection = reply.connection.clone();
    let id = reply.id.clone();
    let session_id = reply.session_id.clone();
    let token = reply.token;
    let completed = Arc::clone(&reply.completed);
    let plan = Arc::clone(&reply.plan);
    let presentation = Arc::clone(&reply.presentation);
    let selected_protocol = Arc::clone(&reply.protocol);
    let used = Arc::clone(&reply.used);
    let selection_failure = Arc::clone(&reply.selection_failure);
    let preferences = reply.preferences.clone();
    let grouping_profile = lock(&sessions).profile_name().to_owned();
    let interactions = Arc::clone(&reply.interactions);
    let history = lock(&sessions).history(&id);

    // `catch_unwind` so a panicking turn ends as a failed turn and not as a
    // dead task whose session stays busy forever. The `async` block also
    // covers a body that panics before it returns its future.
    let storage = history.clone();
    let attempt = AssertUnwindSafe(async move {
        let inference = body(reply);
        if let Some(history) = storage {
            tokio::select! {
                result = inference => result,
                failure = history.failed() => Err(TurnFailure::new(failure.to_string())),
            }
        } else {
            inference.await
        }
    })
    .catch_unwind();
    let mut stop_reason = match Abortable::new(attempt, registration).await {
        Err(_aborted) => v2::StopReason::Cancelled,
        Ok(Ok(Ok(()))) => match lock(&completed).as_ref().map(|outcome| outcome.stop) {
            Some(crate::llm::Stop::MaxTokens) => v2::StopReason::MaxTokens,
            Some(crate::llm::Stop::Refusal) => v2::StopReason::Refusal,
            _ => v2::StopReason::EndTurn,
        },
        Ok(Ok(Err(failure))) => failed(&failure.to_string()),
        Ok(Err(_panic)) => failed("the turn panicked; retry or report this failure"),
    };

    // The Abortable has been dropped before taking this lock: cleanup and SDK
    // request cancellation must happen before terminal idle. A concurrent cancel
    // can still win against natural completion, but cannot start a replacement.
    // Claim finalization before awaiting disk: a later cancel cannot invalidate
    // an already durable successful context snapshot.
    let mut outcome = lock(&completed).take();
    let protocol = *lock(&selected_protocol);
    if history.is_none() && !lock(&sessions).finalize(&id, token) {
        stop_reason = v2::StopReason::Cancelled;
    }
    if let Some(history) = &history {
        let snapshot = {
            let mut sessions = lock(&sessions);
            let current = sessions.finalize(&id, token);
            if !current {
                stop_reason = v2::StopReason::Cancelled;
            }
            if current
                && matches!(
                    stop_reason,
                    v2::StopReason::EndTurn | v2::StopReason::MaxTokens
                )
            {
                outcome.as_ref().and_then(|outcome| {
                    let mut snapshot = sessions.snapshot(&id)?;
                    snapshot.messages = outcome.messages.clone();
                    snapshot.instructions = outcome.instructions.clone();
                    snapshot.protocol = protocol;
                    Some(snapshot)
                })
            } else {
                None
            }
        };
        let terminal = match stop_reason {
            v2::StopReason::Cancelled => "cancelled",
            v2::StopReason::Error(_) => "error",
            v2::StopReason::Refusal => "refusal",
            _ => "completed",
        };
        if let Err(error) = history.finish(token, terminal, snapshot).await {
            stop_reason = failed(&error.to_string());
            outcome = None;
        }
    }
    let durable = history.is_some();
    let selection_failure = *lock(&selection_failure);
    if let Some(reason) = selection_failure
        && matches!(stop_reason, v2::StopReason::Error(_))
        && lock(&sessions).is_current(&id, token)
    {
        let pending = {
            let mut registry = lock(&sessions);
            let previous = registry.snapshot(&id).expect("current session");
            registry.block_automatic(&id, reason);
            (previous, registry.snapshot(&id).expect("current session"))
        };
        // Terminal history is already closed; a settings event can now retain the safety guard.
        if let Some(history) = &history
            && history.save(pending.1).await.is_err()
        {
            lock(&sessions).rollback(&id, pending.0);
        }
        let notice = lock(&sessions)
            .selection_notice(&id)
            .unwrap_or_else(|| reason.message());
        send_selection_notice(&connection, &id, notice);
    }
    if outcome
        .as_ref()
        .is_some_and(|outcome| outcome.stop != crate::llm::Stop::Refusal)
        && matches!(
            stop_reason,
            v2::StopReason::EndTurn | v2::StopReason::MaxTokens
        )
        && lock(&sessions).is_current(&id, token)
    {
        let reference = lock(&used).take();
        if let Some(reference) = reference {
            // Disk work is outside the registry lock and after history commit. Failure cannot undo an answer.
            let saved = tokio::task::spawn_blocking(move || {
                preferences.remember(&grouping_profile, reference)
            })
            .await;
            if !matches!(saved, Ok(Ok(()))) {
                send_selection_notice(
                    &connection,
                    &id,
                    "Answer completed, but the recent model preference could not be saved. Check private state-directory permissions before restarting Bee.",
                );
            }
        }
    }
    let publish = move || {
        let mut sessions = lock(&sessions);
        for update in presentation::finish(
            &presentation,
            token,
            matches!(stop_reason, v2::StopReason::Cancelled),
        ) {
            let _ = send_update(&connection, &session_id, update);
        }
        lock(&interactions).waiting.remove(&id);
        if let Some(plan) = lock(&plan).as_mut()
            && plan.cancel_unfinished()
        {
            // Finalization retains authority after cancellation revoked model output.
            sessions.record_plan(&id, token, plan.clone());
            let _ = send_update(&connection, &session_id, plans::update(token, plan.clone()));
        }
        if let Some(outcome) = outcome
            && outcome.stop != crate::llm::Stop::Refusal
            && matches!(
                stop_reason,
                v2::StopReason::EndTurn | v2::StopReason::MaxTokens
            )
            && sessions.is_current(&id, token)
        {
            sessions.commit(&id, token, outcome.messages, outcome.instructions, protocol);
            // Report only committed context, under the same lock as cancellation/idle.
            if let Some(window) = outcome.context.window {
                let meta = serde_json::json!({"bee.dev/context": {"estimated":true,"method":"utf8-json-bytes/3+256","reservedOutput":outcome.context.reserved_output,"window":outcome.context.window_fact}});
                let _ = send_update(
                    &connection,
                    &session_id,
                    v2::SessionUpdate::UsageUpdate(
                        v2::UsageUpdate::new(outcome.context.estimated_input, u64::from(window))
                            .meta(meta.as_object().cloned().unwrap_or_default()),
                    ),
                );
            }
            if outcome.context.removed_turns > 0 {
                let _ = send_update(
                &connection,
                &session_id,
                v2::SessionUpdate::Notice(
                    v2::Notice::new(
                        v2::NoticeSeverity::Info,
                        "Older conversation removed from model context",
                    )
                    .description(format!(
                        "Removed {} oldest complete turn(s) to fit the effective context window.",
                        outcome.context.removed_turns
                    )),
                ),
            );
            }
        }
        let stop_reason = if sessions.finish_turn(&id, token)
            || (durable && matches!(stop_reason, v2::StopReason::Error(_)))
        {
            stop_reason
        } else {
            v2::StopReason::Cancelled
        };
        // Release the busy barrier only after sending idle, under the same session
        // lock used by prompt acceptance. Close responses await this completion.
        let _ = send_update(&connection, &session_id, idle(Some(stop_reason)));
        lock(&tasks).remove(&id);
        let _ = finished.send(());
    };
    if let Some(history) = history {
        // All session state updates share replay's publication boundary. Never
        // acquire the request gate here: close holds it while awaiting this task.
        history.publish(publish).await;
    } else {
        publish();
    }
    Ok(())
}

/// The `error` stop reason, carrying what failed as a JSON-RPC error.
fn failed(reason: &str) -> v2::StopReason {
    v2::StopReason::Error(
        v2::ErrorStopReason::new().error(v2::Error::internal_error().data(reason.to_owned())),
    )
}

fn history_error(error: crate::history::Error) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(error.to_string())
}

fn replay_message(
    connection: &V2ConnectionTo<Client>,
    id: &v2::SessionId,
    message: crate::history::Message,
) -> std::result::Result<(), crate::history::Error> {
    let failure = || crate::history::Error {
        path: "<session transcript>".into(),
        reason: "cannot restore conversation message",
        help: "check the stored history version and reconnect with a compatible client",
    };
    let content: Vec<v2::ContentBlock> =
        serde_json::from_value(message.content).map_err(|_| failure())?;
    let update = match message.kind {
        crate::history::MessageKind::User => {
            v2::SessionUpdate::UserMessage(v2::UserMessage::new(message.id).content(content))
        }
        crate::history::MessageKind::Answer => {
            v2::SessionUpdate::AgentMessage(v2::AgentMessage::new(message.id).content(content))
        }
        crate::history::MessageKind::Thought => {
            v2::SessionUpdate::AgentThought(v2::AgentThought::new(message.id).content(content))
        }
    };
    send_update(connection, id, update).map_err(|_| failure())
}

/// Why `prompt` cannot be served, if it has a block Bee does not accept.
///
/// Known operational content requires an advertised capability. The open union's
/// opaque fallback instead requires preservation, without claiming understanding.
fn unsupported_content(prompt: &[v2::ContentBlock]) -> Option<String> {
    prompt.iter().find_map(|block| {
        let kind = match block {
            v2::ContentBlock::Text(_)
            | v2::ContentBlock::ResourceLink(_)
            | v2::ContentBlock::Other(_) => return None,
            v2::ContentBlock::Image(_) => "image",
            v2::ContentBlock::Audio(_) => "audio",
            v2::ContentBlock::Resource(_) => "resource",
            // A new typed SDK variant needs review; unknown wire types use Other.
            _ => "unrecognized",
        };
        Some(format!(
            "`{kind}` prompt content is not supported: Bee does not advertise that prompt \
             capability; send `text` or `resource_link` blocks instead"
        ))
    })
}

const UNKNOWN_PROMPT: &str =
    "[Prompt contains content Bee does not understand; its contents were not sent to the model.]";

/// Only called after capability validation; opaque payloads never become instructions.
fn prompt_text(prompt: &[v2::ContentBlock]) -> String {
    let understood = prompt
        .iter()
        .filter_map(|block| match block {
            v2::ContentBlock::Text(text) => Some(text.text.clone()),
            v2::ContentBlock::ResourceLink(link) => Some(format!(
                "[Resource link, contents not fetched: {}]",
                link.uri
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Block presence, not string emptiness, preserves explicit empty text input.
    if understood.is_empty()
        && prompt
            .iter()
            .any(|block| matches!(block, v2::ContentBlock::Other(_)))
    {
        UNKNOWN_PROMPT.to_owned()
    } else {
        understood.join("\n")
    }
}

#[cfg(test)]
mod content_tests {
    use super::{UNKNOWN_PROMPT, prompt_text};
    use serde_json::json;

    #[test]
    fn unknown_content_uses_a_fixed_marker_only_without_recognized_blocks() {
        let opaque = json!({"type":"_vendor","text":"do not forward","payload":[null, {"command":"unsafe"}]});
        let future = json!({"type":"future_instruction","_meta":{"secret":"hidden"}});
        for (content, expected) in [
            (json!([opaque]), UNKNOWN_PROMPT),
            (json!([future, opaque]), UNKNOWN_PROMPT),
            (json!([opaque, {"type":"text","text":"hello"}]), "hello"),
            (json!([opaque, {"type":"text","text":""}]), ""),
            (
                json!([{"type":"text","text":""}, opaque, {"type":"text","text":"hello"}]),
                "\nhello",
            ),
            (
                json!([opaque, {"type":"resource_link","uri":"file:///reference","name":"reference"}]),
                "[Resource link, contents not fetched: file:///reference]",
            ),
            (json!([]), ""),
        ] {
            let blocks: Vec<agent_client_protocol::schema::v2::ContentBlock> =
                serde_json::from_value(content).unwrap();
            assert_eq!(prompt_text(&blocks), expected);
        }
    }
}

/// Lock the registry.
fn lock<T>(sessions: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock only means another handler panicked mid-update. The
    // registry is a few counters and a map with no invariant spanning
    // operations, and failing every later request would be worse.
    sessions.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An `idle` state update, with the reason foreground work stopped if any.
fn idle(stop_reason: Option<v2::StopReason>) -> v2::SessionUpdate {
    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
        v2::IdleStateUpdate::new().stop_reason(stop_reason),
    ))
}

fn send_update(
    connection: &V2ConnectionTo<Client>,
    session_id: &v2::SessionId,
    update: v2::SessionUpdate,
) -> agent_client_protocol::Result<()> {
    connection.send_notification(v2::UpdateSessionNotification::new(
        session_id.clone(),
        update,
    ))
}

/// The protocol error a refused session operation becomes.
fn session_error(error: SessionError) -> agent_client_protocol::Error {
    match error {
        SessionError::History(error) => history_error(error),
        // A well-formed request, sent at a moment it cannot be honoured: not a
        // problem with its parameters, so not `-32602`.
        SessionError::Busy(_) | SessionError::SettingsBusy(_) | SessionError::SelectionRequired => {
            agent_client_protocol::Error::invalid_request().data(error.to_string())
        }
        // Everything else names a session or directory the client got wrong.
        _ => invalid_params(error),
    }
}

/// The client sent something this agent cannot act on; the text says what.
fn invalid_params(reason: impl ToString) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(reason.to_string())
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_turns_never_acquire_an_implicit_model_after_terminal_journal_write() {
        struct Catalogue;
        impl crate::models::Http for Catalogue {
            fn get<'a>(
                &'a self,
                _: &'a str,
                _: reqwest::header::HeaderMap,
                _: usize,
            ) -> BoxFuture<'a, std::result::Result<Vec<u8>, crate::models::Error>> {
                Box::pin(async { Ok(br#"{"data":[{"id":"native-model"}]}"#.to_vec()) })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[accounts.account]\nprovider='openrouter'\ncredential={env='KEY'}\ndiscovery=false",
        )
        .unwrap();
        let env = crate::config::Environment::new(None, None, Some(config_path))
            .with_variable("KEY", "fixture");
        let providers =
            Providers::from_config(&crate::config::Config::load(&env, root.path()).unwrap())
                .unwrap();
        let discovery = Arc::new(crate::models::Service::new(
            crate::models::Settings {
                enabled: false,
                ..Default::default()
            },
            Arc::new(Catalogue),
            Arc::new(crate::models::FileCache(None)),
            Arc::new(crate::models::SystemClock),
        ));
        discovery
            .list_endpoint(providers.get("account").unwrap(), &env)
            .await
            .unwrap();
        let sessions = Arc::new(Mutex::new(
            Sessions::with_accounts(providers).with_discovery(Some(discovery)),
        ));
        let snapshot = crate::history::Snapshot {
            selection_state: Default::default(),
            profile: "default".into(),
            cwd: root.path().into(),
            turns: 0,
            provider: None,
            model: None,
            protocol: None,
            messages: Vec::new(),
            instructions: Default::default(),
        };
        let store = crate::history::Store::open(root.path().join("state")).unwrap();
        let (id, history) = store.create(snapshot.clone()).await.unwrap();
        lock(&sessions).restore(id.clone(), snapshot, history.clone());
        let turn = lock(&sessions).begin_turn(&id, || {}).unwrap();
        history
            .accept(turn.token, serde_json::json!([]))
            .await
            .unwrap();
        let tasks = Tasks::default();
        lock(&tasks).insert(
            id.clone(),
            futures::future::pending::<()>().boxed().shared(),
        );
        assert!(lock(&sessions).cancel(&id));
        refresh_recent(&sessions, &tasks, &id).await.unwrap();
        assert!(
            lock(&sessions)
                .settings(&id)
                .unwrap()
                .unwrap()
                .model
                .is_none()
        );
        assert!(history.healthy().is_ok());
        history.finish(turn.token, "cancelled", None).await.unwrap();
        lock(&sessions).finish_turn(&id, turn.token);
        lock(&tasks).remove(&id);
        refresh_recent(&sessions, &tasks, &id).await.unwrap();
        assert_eq!(
            lock(&sessions)
                .settings(&id)
                .unwrap()
                .unwrap()
                .model
                .as_deref(),
            None
        );
        assert!(history.healthy().is_ok());
    }

    #[test]
    fn an_unpolled_close_blocks_later_mutations_even_when_the_gate_was_occupied() {
        let gates = Gates::default();
        let gate = session_gate(&gates, "session");
        let first = Arc::clone(&gate).try_enter().ok().unwrap();
        let closing = Arc::clone(&gate).close();
        assert!(closing.reservation.is_none());
        drop(first);
        assert!(Arc::clone(&gate).try_enter().is_err());
        drop(closing);
        assert!(Arc::clone(&gate).try_enter().is_ok());
    }

    #[test]
    fn rejected_session_ids_do_not_leave_permanent_gates() {
        let gates = Gates::default();
        let live = session_gate(&gates, "live");
        for index in 0..1000 {
            drop(session_gate(&gates, &format!("unknown-{index}")));
        }
        assert!(lock(&gates).len() <= 2);
        assert!(Arc::ptr_eq(&live, &session_gate(&gates, "live")));
        assert_eq!(lock(&gates).len(), 1);
    }
}
