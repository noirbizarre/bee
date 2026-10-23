//! The ACPv2 boundary: the only module that imports the ACP Rust SDK.
//!
//! Keeping the SDK behind one file means an upgrade, which is likely while v2
//! is a draft, touches one file instead of the whole crate.
//!
//! Two layers, so the agent never learns which transport carries it (ADR-003):
//! [`agent`] is the transport-free core, [`serve`] runs it over any transport
//! the SDK can connect, and [`serve_stdio`] picks stdin/stdout. A remote
//! transport later supplies another [`ConnectTo`] and changes nothing else.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Agent, Client, ConnectTo, Responder, Stdio, V2ConnectionTo};

use crate::session::Sessions;
use crate::{Error, Result};

/// What every prompt is answered with, until an LLM is integrated.
///
/// Fixed on purpose: a reply that varied with the prompt would look like model
/// output, and a client or a user could come to rely on it.
const PLACEHOLDER_REPLY: &str = "This is a placeholder response: Bee has no LLM integrated yet, \
                                 so it cannot answer your prompt.";

/// Run the agent over `transport` until the peer disconnects.
///
/// # Errors
///
/// Returns [`Error::Connection`] if the connection ends with a protocol or
/// transport error. A peer that simply closes its end is not an error.
pub async fn serve(transport: impl ConnectTo<Agent>) -> Result<()> {
    // `agent()` is `ConnectTo<Client>`, and the transport is the client's end,
    // so it is the transport that the agent connects to.
    agent()
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
pub async fn serve_stdio() -> Result<()> {
    // No debug callback on purpose: stdout carries protocol traffic only, and a
    // trace line written there would corrupt the client's JSON-RPC stream.
    serve(Stdio::new()).await
}

/// Build the ACPv2 agent side of a connection.
///
/// It serves the baseline session lifecycle (`session/new`, `list`, `resume`,
/// `close`, `prompt`, `cancel`) and answers every prompt with a fixed
/// placeholder. No LLM is integrated yet.
///
/// The result connects to any [`Client`] transport, which is how the tests
/// drive it in-process without a subprocess. Each call builds its own session
/// registry, so two connections never share a session.
pub fn agent() -> impl ConnectTo<Client> {
    let sessions = Arc::new(Mutex::new(Sessions::new()));

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
                    let id = lock(&sessions).create(request.cwd.into_inner());
                    responder.respond(v2::NewSessionResponse::new(id))
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
                        return responder
                            .respond_with_error(invalid_params("`cursor` is not supported: `session/list` returns a single page"));
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
                    // Nothing is retained, so there is nothing to replay. The
                    // protocol requires rejecting a cursor rather than resuming
                    // without the replay the client asked for.
                    if request.replay_from.is_some() {
                        return responder.respond_with_error(invalid_params(
                            "`replayFrom` is not supported: Bee does not retain conversation history",
                        ));
                    }
                    let id = request.session_id.to_string();
                    let cwd: &Path = request.cwd.as_ref();
                    if let Err(error) = lock(&sessions).resume(&id, cwd) {
                        return responder.respond_with_error(invalid_params(error));
                    }
                    // The protocol requires a current-state snapshot before the
                    // response: the response alone does not imply idle.
                    send_update(&connection, &request.session_id, idle(None))?;
                    responder.respond(v2::ResumeSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: v2::CloseSessionRequest,
                            responder: Responder<v2::CloseSessionResponse>,
                            _connection: V2ConnectionTo<Client>| {
                    // No foreground work can be pending (the placeholder turn
                    // ends inside its own handler), so there is nothing to
                    // cancel or wait for before responding.
                    match lock(&sessions).close(&request.session_id.to_string()) {
                        Ok(()) => responder.respond(v2::CloseSessionResponse::new()),
                        Err(error) => responder.respond_with_error(invalid_params(error)),
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
                    let turn = match lock(&sessions).begin_turn(&request.session_id.to_string()) {
                        Ok(turn) => turn,
                        // A prompt refused before insertion is an error
                        // response, not an idle update: no turn ever started.
                        Err(error) => return responder.respond_with_error(invalid_params(error)),
                    };
                    let session_id = request.session_id;

                    // Acceptance only: the response says the user message was
                    // inserted, not that anything has been answered. Everything
                    // after it travels as `session/update`.
                    responder.respond(v2::PromptResponse::new(turn.user_message_id.clone()))?;

                    send_update(
                        &connection,
                        &session_id,
                        v2::SessionUpdate::UserMessage(
                            // The same id the response returned, so a client can
                            // match the echoed message to its prompt.
                            v2::UserMessage::new(turn.user_message_id).content(request.prompt),
                        ),
                    )?;
                    send_update(
                        &connection,
                        &session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                            v2::RunningStateUpdate::new(),
                        )),
                    )?;
                    send_update(
                        &connection,
                        &session_id,
                        v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                            PLACEHOLDER_REPLY.into(),
                            turn.agent_message_id,
                        )),
                    )?;
                    // The terminal state of the turn. `end_turn`, because the
                    // placeholder always completes; cancellation and failure
                    // arrive with the agentic loop.
                    send_update(&connection, &session_id, idle(Some(v2::StopReason::EndTurn)))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async |_notification: v2::CancelSessionNotification,
                   _connection: V2ConnectionTo<Client>| {
                // Deliberately nothing: the placeholder turn finishes inside its
                // own handler, so no work is ever in flight to cancel, and the
                // protocol asks for no update when none is. Real cancellation
                // arrives with the agentic loop.
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// Lock the registry.
fn lock(sessions: &Mutex<Sessions>) -> MutexGuard<'_, Sessions> {
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

/// The client sent something this agent cannot act on; the text says what.
fn invalid_params(reason: impl ToString) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(reason.to_string())
}
