//! Host callbacks run as bounded connection-owned work, never on receive dispatch.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use agent_client_protocol::{Agent, Responder, V2ConnectionTo};
use futures::future::BoxFuture;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{Error, Result, Shared, lock, v2};

/// A cancellation-safe structured host callback. Returning an error never auto-approves.
pub type Handler<Request, Response> =
    Arc<dyn Fn(Request, CancellationToken) -> BoxFuture<'static, Result<Response>> + Send + Sync>;

/// Optional host-owned interactions; only installed elicitation modes are advertised.
#[derive(Clone, Default)]
pub struct HostHandlers {
    /// Select an offered permission option, or explicitly cancel.
    pub permission: Option<Handler<v2::RequestPermissionRequest, v2::RequestPermissionResponse>>,
    /// Render a structured form and return accept/decline/cancel.
    pub form: Option<Handler<v2::CreateElicitationRequest, v2::CreateElicitationResponse>>,
    /// Present a URL; acceptance does not imply completion of external work.
    pub url: Option<Handler<v2::CreateElicitationRequest, v2::CreateElicitationResponse>>,
    /// Receive later completion of an accepted URL interaction.
    pub url_complete: Option<Handler<v2::CompleteElicitationNotification, ()>>,
}

impl HostHandlers {
    pub(super) fn capabilities(&self) -> v2::ClientCapabilities {
        let mut elicitation = v2::ElicitationCapabilities::new();
        if self.form.is_some() {
            elicitation = elicitation.form(v2::ElicitationFormCapabilities::new());
        }
        if self.url.is_some() && self.url_complete.is_some() {
            elicitation = elicitation.url(v2::ElicitationUrlCapabilities::new());
        }
        v2::ClientCapabilities::new().elicitation(elicitation)
    }
}

pub(super) struct State {
    permits: Arc<Semaphore>,
    urls: Mutex<HashSet<v2::ElicitationId>>,
}

impl State {
    pub(super) fn new(max: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max)),
            urls: Mutex::new(HashSet::new()),
        }
    }

    pub(super) async fn drain(&self, max: usize) {
        // Admission is synchronous; acquiring all permits joins already admitted workers.
        if let Ok(count) = u32::try_from(max) {
            let _ = self.permits.acquire_many(count).await;
        }
    }
}

fn cancelled_permission() -> v2::RequestPermissionResponse {
    v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled)
}

pub(super) fn permission(
    request: v2::RequestPermissionRequest,
    responder: Responder<v2::RequestPermissionResponse>,
    connection: V2ConnectionTo<Agent>,
    handlers: HostHandlers,
    shared: Arc<Shared>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    if lock(&shared.inbox).session.as_ref() != Some(&request.session_id) {
        shared.fail(Error::Interaction {
            reason: "permission references an unknown session",
        });
        return responder.respond(cancelled_permission());
    }
    if serde_json::to_vec(&request).map_or(true, |v| v.len() > shared.options.max_bytes) {
        shared.fail(Error::Limit {
            resource: "interaction bytes",
        });
        return responder.respond(cancelled_permission());
    }
    let Some(handler) = handlers.permission else {
        shared.fail(Error::Interaction {
            reason: "permission handler is unavailable",
        });
        return responder.respond(cancelled_permission());
    };
    let Ok(permit) = shared.interactions.permits.clone().try_acquire_owned() else {
        shared.fail(Error::Limit {
            resource: "interactions",
        });
        return responder.respond(cancelled_permission());
    };
    let session_cancel = lock(&shared.session_cancel).clone();
    let peer_cancel = responder.cancellation();
    connection.spawn(async move {
        let _permit = permit;
        let token = CancellationToken::new();
        let _cancel_on_drop = token.clone().drop_guard();
        let response = tokio::select! {
            biased;
            () = shared.shutdown.cancelled() => None,
            () = session_cancel.cancelled() => None,
            () = peer_cancel.cancelled() => None,
            result = tokio::time::timeout(shared.options.interaction_timeout, handler(request.clone(), token.clone())) => {
                match result {
                    Ok(Ok(response)) => Some(response),
                    Ok(Err(error)) => { shared.fail(error); None }
                    Err(_) => { shared.fail(Error::Timeout { stage: "permission interaction" }); None }
                }
            }
        };
        token.cancel();
        let response = match response {
            Some(response) if matches!(&response.outcome, v2::RequestPermissionOutcome::Cancelled)
                || matches!(&response.outcome, v2::RequestPermissionOutcome::Selected(selected)
                    if request.options.iter().any(|o| o.option_id == selected.option_id)) => response,
            Some(_) => {
                shared.fail(Error::Interaction { reason: "selected an option that was not offered" });
                cancelled_permission()
            }
            None => cancelled_permission(),
        };
        responder.respond(response)
    })
}

fn unsupported() -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params()
        .data("unsupported or unavailable host interaction")
}

pub(super) fn elicit(
    request: v2::CreateElicitationRequest,
    responder: Responder<v2::CreateElicitationResponse>,
    connection: V2ConnectionTo<Agent>,
    handlers: HostHandlers,
    shared: Arc<Shared>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    if let v2::ElicitationScope::Session(scope) = request.scope()
        && lock(&shared.inbox).session.as_ref() != Some(&scope.session_id)
    {
        shared.fail(Error::Interaction {
            reason: "elicitation references an unknown session",
        });
        return responder.respond_with_error(unsupported());
    }
    if serde_json::to_vec(&request).map_or(true, |v| v.len() > shared.options.max_bytes) {
        shared.fail(Error::Limit {
            resource: "interaction bytes",
        });
        return responder.respond_with_error(unsupported());
    }
    let handler = match &request.mode {
        v2::ElicitationMode::Form(_) => handlers.form,
        v2::ElicitationMode::Url(_) if handlers.url_complete.is_some() => handlers.url,
        _ => None,
    };
    let Some(handler) = handler else {
        shared.fail(Error::Unsupported {
            capability: "requested elicitation mode",
        });
        return responder.respond_with_error(unsupported());
    };
    let Ok(permit) = shared.interactions.permits.clone().try_acquire_owned() else {
        shared.fail(Error::Limit {
            resource: "interactions",
        });
        return responder.respond_with_error(unsupported());
    };
    let peer_cancel = responder.cancellation();
    // Closing this session ends its interactions; request-scoped setup remains independent.
    let session_cancel = match request.scope() {
        v2::ElicitationScope::Session(_) => lock(&shared.session_cancel).clone(),
        _ => CancellationToken::new(),
    };
    connection.spawn(async move {
        let _permit = permit;
        let token = CancellationToken::new();
        let _cancel_on_drop = token.clone().drop_guard();
        let result = tokio::select! {
            biased;
            () = shared.shutdown.cancelled() => None,
            () = session_cancel.cancelled() => None,
            () = peer_cancel.cancelled() => None,
            result = tokio::time::timeout(shared.options.interaction_timeout, handler(request.clone(), token.clone())) => {
                match result {
                    Ok(Ok(response)) => Some(response),
                    Ok(Err(error)) => { shared.fail(error); None }
                    Err(_) => { shared.fail(Error::Timeout { stage: "elicitation interaction" }); None }
                }
            }
        };
        token.cancel();
        let response = result.unwrap_or_else(|| v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel));
        if serde_json::to_vec(&response).map_or(true, |v| v.len() > shared.options.max_bytes) {
            shared.fail(Error::Limit { resource: "interaction response bytes" });
            return responder.respond_with_error(unsupported());
        }
        if !valid_response(&request, &response) {
            shared.fail(Error::Interaction { reason: "returned invalid form content or action" });
            return responder.respond_with_error(unsupported());
        }
        if let v2::ElicitationMode::Url(url) = &request.mode
            && matches!(response.action, v2::ElicitationAction::Accept(_))
        {
            let mut urls = lock(&shared.interactions.urls);
            if urls.len() >= shared.options.max_interactions {
                shared.fail(Error::Limit { resource: "URL interactions" });
                return responder.respond_with_error(unsupported());
            }
            if !urls.insert(url.elicitation_id.clone()) {
                shared.fail(Error::Interaction { reason: "reused a pending URL interaction ID" });
                return responder.respond_with_error(unsupported());
            }
        }
        responder.respond(response)
    })
}

fn valid_response(
    request: &v2::CreateElicitationRequest,
    response: &v2::CreateElicitationResponse,
) -> bool {
    match &response.action {
        v2::ElicitationAction::Decline | v2::ElicitationAction::Cancel => true,
        v2::ElicitationAction::Accept(accepted) => match &request.mode {
            v2::ElicitationMode::Form(form) => {
                // Schema is the protocol's inline restricted form schema, never a remote reference.
                let Ok(schema) = serde_json::to_value(&form.requested_schema) else {
                    return false;
                };
                let Ok(content) =
                    serde_json::to_value(accepted.content.clone().unwrap_or_default())
                else {
                    return false;
                };
                jsonschema::validator_for(&schema)
                    .is_ok_and(|validator| validator.is_valid(&content))
            }
            v2::ElicitationMode::Url(_) => accepted.content.is_none(),
            _ => false,
        },
        _ => false,
    }
}

pub(super) fn complete(
    notification: v2::CompleteElicitationNotification,
    connection: V2ConnectionTo<Agent>,
    handlers: HostHandlers,
    shared: Arc<Shared>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    if !lock(&shared.interactions.urls).remove(&notification.elicitation_id) {
        return Ok(());
    }
    if let Some(handler) = handlers.url_complete {
        let Ok(permit) = shared.interactions.permits.clone().try_acquire_owned() else {
            shared.fail(Error::Limit {
                resource: "interactions",
            });
            return Ok(());
        };
        connection.spawn(async move {
            let _permit = permit;
            let token = shared.shutdown.child_token();
            let _cancel_on_drop = token.clone().drop_guard();
            tokio::select! {
                () = token.cancelled() => {},
                result = tokio::time::timeout(shared.options.interaction_timeout, handler(notification, token.clone())) => {
                    match result {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => shared.fail(error),
                        Err(_) => shared.fail(Error::Timeout { stage: "URL completion" }),
                    }
                }
            }
            token.cancel();
            Ok(())
        })?;
    }
    Ok(())
}
