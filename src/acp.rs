//! The ACPv2 boundary: the only module that imports the ACP Rust SDK.
//!
//! Keeping the SDK behind one file means an upgrade, which is likely while v2
//! is a draft, touches one file instead of the whole crate.
//!
//! Two layers, so the agent never learns which transport carries it (ADR-003):
//! [`agent`] is the transport-free core, [`serve`] runs it over any transport
//! the SDK can connect, and [`serve_stdio`] picks stdin/stdout. A remote
//! transport later supplies another [`ConnectTo`] and changes nothing else.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Agent, Client, ConnectTo, Responder, Stdio, V2ConnectionTo};
use futures::future::{AbortHandle, AbortRegistration, Abortable, FutureExt};

use crate::provider::Providers;
use crate::session::{SessionError, Sessions, Settings};
use crate::{Error, Result};

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
    input: Option<(crate::provider::Profile, String, Vec<crate::llm::Message>)>,
    completed: Arc<Mutex<Option<crate::agent::Outcome>>>,
}

impl Reply {
    /// Send `text` as a chunk of the agent's answer, unless the turn was
    /// cancelled or has ended.
    pub fn text(&self, text: &str) {
        // The check and the send share one lock guard. A cancel takes the same
        // lock to stop the turn and to send its `idle`, so a chunk can never be
        // sent after that `idle` even if this runs on another thread.
        let sessions = lock(&self.sessions);
        if sessions.is_current(&self.id, self.token) {
            // A failed send means the peer is gone, and the connection is
            // already shutting down: nothing is left to tell.
            let _ = send_update(
                &self.connection,
                &self.session_id,
                v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                    text.into(),
                    self.message_id.clone(),
                )),
            );
        }
    }
}

/// Credentials are resolved at request time, not while loading configuration.
async fn inference(reply: Reply) -> std::result::Result<(), TurnFailure> {
    let (profile, model, messages) = reply.input.clone().ok_or_else(|| TurnFailure::new(
        "configure a provider and model in Bee's configuration or select a model over ACP before prompting"
    ))?;
    let env = crate::config::Environment::from_process();
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
    let credential = crate::auth::resolve(&profile, &env, &store, crate::auth::unix_now())
        .map_err(|_| TurnFailure::new("cannot resolve the model credential; check its environment variable or run `bee auth login`"))?;
    let credential = credential.map(|value| value.secret().clone());
    let model = crate::llm::Model::new(profile, model, credential)
        .map_err(|error| TurnFailure::new(error.to_string()))?;
    let outcome = crate::agent::run(&model, messages, &[], None, &mut |text| reply.text(text))
        .await
        .map_err(|error| TurnFailure::new(error.to_string()))?;
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
/// transport error. A peer that simply closes its end is not an error.
pub async fn serve(transport: impl ConnectTo<Agent>, providers: Providers) -> Result<()> {
    // `agent()` is `ConnectTo<Client>`, and the transport is the client's end,
    // so it is the transport that the agent connects to.
    agent_with_providers(providers)
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
    agent_with_providers(Providers::default())
}

/// [`agent`], with sessions that choose among `providers`.
///
/// Each session is offered a `provider` and a `model` configuration option,
/// and `session/set_config_option` changes them for that session only. The
/// choice is a session setting; the profile's endpoint, headers and credential
/// are configuration, and no request can change them (ADR-007).
pub fn agent_with_providers(providers: Providers) -> impl ConnectTo<Client> {
    build(providers, inference)
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
    build(Providers::default(), body)
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
    build(providers, body)
}

fn build<F, Fut>(providers: Providers, body: F) -> impl ConnectTo<Client>
where
    F: Fn(Reply) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<(), TurnFailure>> + Send + 'static,
{
    let body = Arc::new(body);
    let sessions = Arc::new(Mutex::new(Sessions::with_providers(providers)));

    // `Agent.v2()`, not `Agent.builder()`: the latter is the stable v1 surface.
    Agent
        .v2()
        .name("bee")
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                // Only the baseline session lifecycle. `prompt`, `mcp`, `delete`
                // and `additionalDirectories` stay unset: advertising one is a
                // promise a client acts on, and none of them is implemented.
                responder.respond(
                    v2::InitializeResponse::new(
                        // Hand back the version the SDK gives us rather than
                        // choosing one: the v2 builder negotiates the version on
                        // the wire, so a hard-coded value here would fight it.
                        request.protocol_version,
                        v2::Implementation::new("bee", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(
                        v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()),
                    ),
                )
            },
            // The SDK requires this macro to tie the handler to the request type.
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            _connection: V2ConnectionTo<Client>| {
                    // `mcp_servers` is accepted and ignored: Bee advertises no
                    // MCP capability, and wiring them up is a later issue.
                    // One guard, so the options describe the session just made.
                    let mut sessions = lock(&sessions);
                    let id = sessions.create(request.cwd.into_inner());
                    let options = config_options(sessions.settings(&id).ok().flatten());
                    responder.respond(v2::NewSessionResponse::new(id).config_options(options))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: v2::ListSessionsRequest,
                            responder: Responder<v2::ListSessionsResponse>,
                            _connection: V2ConnectionTo<Client>| {
                    // Bee never returns a `nextCursor`, so a cursor can only be
                    // made up; silently ignoring it would hide a client bug.
                    if request.cursor.is_some() {
                        return responder.respond_with_error(invalid_params(
                            "`cursor` is not supported: `session/list` returns a single page",
                        ));
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
                async move |request: v2::ResumeSessionRequest,
                            responder: Responder<v2::ResumeSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    // Model context is retained, but not ACP update events. The
                    // protocol requires rejecting a cursor rather than resuming
                    // without the replay the client asked for.
                    if request.replay_from.is_some() {
                        return responder.respond_with_error(invalid_params(
                            "`replayFrom` is not supported: Bee does not retain ACP update history",
                        ));
                    }
                    let id = request.session_id.to_string();
                    let cwd: &Path = request.cwd.as_ref();
                    let options = {
                        let mut sessions = lock(&sessions);
                        if let Err(error) = sessions.resume(&id, cwd) {
                            return responder.respond_with_error(session_error(error));
                        }
                        // The session keeps what the client chose before it
                        // was closed, so the options report that, not the defaults.
                        config_options(sessions.settings(&id).ok().flatten())
                    };
                    // The protocol requires a current-state snapshot before the
                    // response: the response alone does not imply idle.
                    send_update(&connection, &request.session_id, idle(None))?;
                    responder.respond(v2::ResumeSessionResponse::new().config_options(options))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: v2::SetSessionConfigOptionRequest,
                            responder: Responder<v2::SetSessionConfigOptionResponse>,
                            _connection: V2ConnectionTo<Client>| {
                    // Every option Bee offers is a selector. Anything else is a
                    // client that did not read the options it was given.
                    let Some(value) = request.value.as_id() else {
                        return responder.respond_with_error(invalid_params(
                            "this option takes a selected value (`type: \"id\"`), not a boolean",
                        ));
                    };
                    let value = value.to_string();
                    let id = request.session_id.to_string();

                    // One guard from the change to the snapshot, so the reply
                    // describes exactly the state this request produced.
                    let mut sessions = lock(&sessions);
                    let changed = match &*request.config_id.to_string() {
                        PROVIDER_OPTION => sessions.set_provider(&id, &value),
                        MODEL_OPTION => sessions.set_model(&id, &value),
                        other => {
                            return responder.respond_with_error(invalid_params(format!(
                                "unknown configuration option `{other}`; \
                                 Bee offers `{PROVIDER_OPTION}` and `{MODEL_OPTION}`"
                            )));
                        }
                    };
                    if let Err(error) = changed {
                        return responder.respond_with_error(session_error(error));
                    }
                    let options = config_options(sessions.settings(&id).ok().flatten());
                    responder.respond(v2::SetSessionConfigOptionResponse::new(options))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: v2::CloseSessionRequest,
                            responder: Responder<v2::CloseSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    // Held until the update is sent: a turn finishing at this
                    // moment waits here, then finds it was taken and stays silent,
                    // so the client gets exactly one terminal state.
                    let mut sessions = lock(&sessions);
                    match sessions.close(&request.session_id.to_string()) {
                        Ok(cancelled) => {
                            if cancelled {
                                // Closing cancels running work, and the protocol
                                // wants its `cancelled` idle *before* the close
                                // response. None for an idle session. A failed
                                // send means the peer is gone, so the response
                                // below is moot too.
                                let _ = send_update(
                                    &connection,
                                    &request.session_id,
                                    idle(Some(v2::StopReason::Cancelled)),
                                );
                            }
                            responder.respond(v2::CloseSessionResponse::new())
                        }
                        Err(error) => responder.respond_with_error(session_error(error)),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: v2::PromptRequest,
                            responder: Responder<v2::PromptResponse>,
                            connection: V2ConnectionTo<Client>| {
                    // Before anything is inserted: a prompt that carries content
                    // Bee did not advertise support for is a bad request, not a
                    // turn that fails halfway.
                    if let Some(reason) = unsupported_content(&request.prompt) {
                        return responder.respond_with_error(invalid_params(reason));
                    }

                    // Registered here, in the handler, and not in the spawned
                    // task: the SDK dispatches a `session/cancel` as soon as this
                    // handler returns, and a turn the registry does not know yet
                    // could not be cancelled.
                    let (abort, registration) = AbortHandle::new_pair();
                    let id = request.session_id.to_string();
                    let mut registry = lock(&sessions);
                    let turn = match registry.begin_turn(&id, move || abort.abort()) {
                        Ok(turn) => turn,
                        // A prompt refused before insertion is an error
                        // response, not an idle update: no turn ever started.
                        Err(error) => return responder.respond_with_error(session_error(error)),
                    };
                    let mut input = registry.inference(&id);
                    if let Some((_, _, history)) = &mut input {
                        let content = request
                            .prompt
                            .iter()
                            .filter_map(|block| match block {
                                v2::ContentBlock::Text(text) => Some(text.text.clone()),
                                v2::ContentBlock::ResourceLink(link) => Some(format!(
                                    "[Resource link, contents not fetched: {}]",
                                    link.uri
                                )),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        history.push(crate::llm::Message::User(content));
                    }
                    drop(registry);
                    let session_id = request.session_id;

                    // Acceptance only: the response says the user message was
                    // inserted, not that anything has been answered. Everything
                    // after it travels as `session/update`.
                    if let Err(error) =
                        responder.respond(v2::PromptResponse::new(turn.user_message_id.clone()))
                    {
                        // The turn was registered but will never run; leaving it
                        // would keep the session busy for good.
                        lock(&sessions).finish_turn(&id, turn.token);
                        return Err(error);
                    }

                    // Sent from the handler, not the spawned turn, so both
                    // always precede the `idle` of a cancel that arrives next.
                    // Send failures mean the peer is gone and the response is
                    // already out: returning them would make the SDK answer the
                    // same request a second time.
                    let _ = send_update(
                        &connection,
                        &session_id,
                        v2::SessionUpdate::UserMessage(
                            // The same id the response returned, so a client can
                            // match the echoed message to its prompt.
                            v2::UserMessage::new(turn.user_message_id).content(request.prompt),
                        ),
                    );
                    let _ = send_update(
                        &connection,
                        &session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                            v2::RunningStateUpdate::new(),
                        )),
                    );

                    let reply = Reply {
                        sessions: Arc::clone(&sessions),
                        connection: connection.clone(),
                        id,
                        session_id,
                        message_id: turn.agent_message_id,
                        token: turn.token,
                        input,
                        completed: Arc::new(Mutex::new(None)),
                    };
                    // A turn runs off the dispatch loop: handlers block it, and
                    // a `session/cancel` queued behind a running prompt would
                    // only be seen once the turn it should stop had ended. If
                    // the spawn fails the connection is already closing, and
                    // there is nobody left to run the turn for.
                    let _ = connection.spawn(run_turn(Arc::clone(&body), reply, registration));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = Arc::clone(&sessions);
                async move |notification: v2::CancelSessionNotification,
                            connection: V2ConnectionTo<Client>| {
                    // Held until the update is sent, as in `session/close`.
                    let mut sessions = lock(&sessions);
                    // Cancelling an idle or unknown session is not an error: a
                    // notification has no reply to carry one, and the protocol
                    // asks for no update when there was no work to cancel.
                    if sessions.cancel(&notification.session_id.to_string()) {
                        // `cancelled`, even though aborting dropped the work
                        // mid-flight: the protocol forbids reporting that as an
                        // error, and the client asked for it.
                        send_update(
                            &connection,
                            &notification.session_id,
                            idle(Some(v2::StopReason::Cancelled)),
                        )?;
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// The id of the option that selects the provider profile.
const PROVIDER_OPTION: &str = "provider";

/// The id of the option that selects the model.
const MODEL_OPTION: &str = "model";

/// The configuration options a session offers, from its effective settings.
///
/// Empty without a provider: an option nothing can act on is a promise Bee
/// cannot keep. The model selector lists the profile's suggestions, but
/// `session/set_config_option` accepts any well-formed id, so a model absent
/// from every list is still usable.
fn config_options(settings: Option<Settings>) -> Vec<v2::SessionConfigOption> {
    let Some(settings) = settings else {
        return Vec::new();
    };
    let select = |names: &[String]| -> Vec<v2::SessionConfigSelectOption> {
        names
            .iter()
            .map(|name| v2::SessionConfigSelectOption::new(name.as_str(), name.as_str()))
            .collect()
    };

    let mut options = vec![
        v2::SessionConfigOption::select(
            PROVIDER_OPTION,
            "Provider",
            settings.provider.as_str(),
            select(&settings.providers),
        )
        .description("The configured provider profile that answers this session's prompts.")
        // No spec category fits a connection profile; `_` marks it custom, as
        // the protocol requires for non-spec categories.
        .category(v2::SessionConfigOptionCategory::Other(
            "_provider".to_owned(),
        )),
    ];
    // A selector needs a current value; a profile with no model has none to show.
    if let Some(model) = &settings.model {
        options.push(
            v2::SessionConfigOption::select(
                MODEL_OPTION,
                "Model",
                model.as_str(),
                select(&settings.models),
            )
            .description("The model id sent to the provider. Any id may be set, listed or not.")
            .category(v2::SessionConfigOptionCategory::Model),
        );
    }
    options
}

/// Run one turn to its terminal state.
///
/// Always returns `Ok`: the SDK shuts the whole connection down when a spawned
/// task fails, and one failed turn must not take the other sessions with it.
async fn run_turn<F, Fut>(
    body: Arc<F>,
    reply: Reply,
    registration: AbortRegistration,
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

    // `catch_unwind` so a panicking turn ends as a failed turn and not as a
    // dead task whose session stays busy forever. The `async` block also
    // covers a body that panics before it returns its future.
    let attempt = AssertUnwindSafe(async move { body(reply).await }).catch_unwind();
    let stop_reason = match Abortable::new(attempt, registration).await {
        // Cancelled or closed: whoever stopped it already sent the terminal
        // `idle`, and sending another would be a second one.
        Err(_aborted) => return Ok(()),
        Ok(Ok(Ok(()))) => match lock(&completed).as_ref().map(|outcome| outcome.stop) {
            Some(crate::llm::Stop::MaxTokens) => v2::StopReason::MaxTokens,
            Some(crate::llm::Stop::Refusal) => v2::StopReason::Refusal,
            _ => v2::StopReason::EndTurn,
        },
        Ok(Ok(Err(failure))) => failed(&failure.to_string()),
        Ok(Err(_panic)) => failed("the turn panicked; retry or report this failure"),
    };

    // Held across the send, as in `session/cancel`: if a cancel took this turn
    // in the meantime it reports the terminal state and this stays silent. Never
    // `end_turn` for a turn the client saw cancelled.
    let mut sessions = lock(&sessions);
    if let Some(outcome) = lock(&completed).take()
        && outcome.stop != crate::llm::Stop::Refusal
    {
        sessions.commit(&id, token, outcome.messages);
    }
    if sessions.finish_turn(&id, token) {
        // A failed send means the peer is gone; there is nobody to tell.
        let _ = send_update(&connection, &session_id, idle(Some(stop_reason)));
    }
    Ok(())
}

/// The `error` stop reason, carrying what failed as a JSON-RPC error.
fn failed(reason: &str) -> v2::StopReason {
    v2::StopReason::Error(
        v2::ErrorStopReason::new().error(v2::Error::internal_error().data(reason.to_owned())),
    )
}

/// Why `prompt` cannot be served, if it has a block Bee does not accept.
///
/// Bee advertises no prompt capability, and the protocol only requires text
/// and resource links without one. Anything else would be echoed back as if it
/// had been understood.
fn unsupported_content(prompt: &[v2::ContentBlock]) -> Option<String> {
    prompt.iter().find_map(|block| {
        let kind = match block {
            v2::ContentBlock::Text(_) | v2::ContentBlock::ResourceLink(_) => return None,
            v2::ContentBlock::Image(_) => "image",
            v2::ContentBlock::Audio(_) => "audio",
            v2::ContentBlock::Resource(_) => "resource",
            // Future block types, and the custom ones: not understood either.
            _ => "unrecognized",
        };
        Some(format!(
            "`{kind}` prompt content is not supported: Bee does not advertise that prompt \
             capability; send `text` or `resource_link` blocks instead"
        ))
    })
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
        // A well-formed request, sent at a moment it cannot be honoured: not a
        // problem with its parameters, so not `-32602`.
        SessionError::Busy(_) | SessionError::SettingsBusy(_) => {
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
