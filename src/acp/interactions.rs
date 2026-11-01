//! Client-owned authorization and turn-owned user input, confined to ACP.

use std::future::Future;
use std::pin::Pin;

use agent_client_protocol::schema::v2;
use serde_json::json;

use crate::agent::{NativeExecutor, ToolResult};
use crate::llm::{Call, Error, Tool};

use super::{Reply, lock, send_update};

#[derive(Default)]
pub(super) struct State {
    pub form: bool,
    pub url: bool,
    pub waiting: std::collections::BTreeMap<String, u64>,
    leases: std::collections::BTreeMap<(String, u64), std::collections::BTreeSet<u64>>,
    next_wait: u64,
    pub next_url: u64,
}

impl State {
    pub(super) fn finish(&mut self, id: &str, token: u64) {
        self.leases.remove(&(id.to_owned(), token));
        if self.waiting.get(id) == Some(&token) {
            self.waiting.remove(id);
        }
    }
}

// Unique leases prevent stale/double completion from clearing a sibling's wait.
struct Wait {
    reply: Reply,
    id: Option<u64>,
}
impl Wait {
    async fn begin(reply: &Reply) -> Result<Self, super::client::Error> {
        let id = {
            let sessions = lock(&reply.sessions);
            if !sessions.is_current(&reply.id, reply.token) {
                return Err(super::client::Error::Cancelled);
            }
            let mut state = lock(&reply.interactions);
            state.next_wait += 1;
            let id = state.next_wait;
            state
                .leases
                .entry((reply.id.clone(), reply.token))
                .or_default()
                .insert(id);
            state.waiting.insert(reply.id.clone(), reply.token);
            id
        };
        let lease = Self {
            reply: reply.clone(),
            id: Some(id),
        };
        reply
            .publish_phase()
            .await
            .map_err(|_| super::client::Error::Cancelled)?;
        Ok(lease)
    }
    async fn finish(mut self) {
        self.release();
        let _ = self.reply.publish_phase().await;
    }
    fn release(&mut self) {
        if let Some(id) = self.id.take() {
            let mut state = lock(&self.reply.interactions);
            let key = (self.reply.id.clone(), self.reply.token);
            if let Some(leases) = state.leases.get_mut(&key) {
                leases.remove(&id);
                if leases.is_empty() {
                    state.finish(&self.reply.id, self.reply.token);
                }
            }
        }
    }
}
impl Drop for Wait {
    fn drop(&mut self) {
        if self.id.is_some() {
            self.release();
            let reply = self.reply.clone();
            // The phase gate suppresses late publication after parent cancellation/idle.
            tokio::spawn(async move {
                let _ = reply.publish_phase().await;
            });
        }
    }
}

#[derive(Default)]
pub(super) struct DelegateUrls(
    std::sync::Mutex<std::collections::BTreeMap<String, (v2::ElicitationId, Wait)>>,
);

impl DelegateUrls {
    pub(super) async fn finish(&self) {
        let pending = std::mem::take(&mut *lock(&self.0));
        for (_, (_, wait)) in pending {
            wait.finish().await;
        }
    }
}

pub(super) fn delegate_handlers(
    reply: &Reply,
    call: &Call,
    agent: &str,
    task: &str,
) -> (super::client::HostHandlers, std::sync::Arc<DelegateUrls>) {
    use super::client::{Error as ClientError, HostHandlers};
    use std::sync::Arc;
    let mut handlers = HostHandlers::default();
    let attribution = format!("Delegate {agent} / {task}");
    let parent = reply.clone();
    let tool_id = v2::ToolCallId::new(reply.tool_id(call));
    let title = attribution.clone();
    handlers.permission = Some(Arc::new(move |mut request, cancellation| {
        let parent = parent.clone();
        let tool_id = tool_id.clone();
        let title = title.clone();
        Box::pin(async move {
            request.session_id = parent.session_id.clone();
            request.title = format!("{title}: {}", request.title);
            match &mut request.subject {
                Some(v2::RequestPermissionSubject::Command(command)) => {
                    command.tool_call_id = Some(tool_id);
                    command.terminal_id = None;
                }
                Some(v2::RequestPermissionSubject::ToolCall(subject)) => {
                    subject.tool_call.tool_call_id = tool_id;
                    // Child terminals are not published in the parent's namespace.
                    subject.tool_call.content =
                        agent_client_protocol::schema::MaybeUndefined::Undefined;
                }
                Some(_) => {
                    return Err(ClientError::Interaction {
                        reason: "unsupported child permission subject",
                    });
                }
                None => {}
            }
            let wait = Wait::begin(&parent).await?;
            let pending = {
                let sessions = lock(&parent.sessions);
                if !sessions.is_current(&parent.id, parent.token) {
                    return Err(ClientError::Cancelled);
                }
                parent.connection.send_request(request)
            };
            let response = tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(ClientError::Cancelled),
                response = pending.block_task() => response.map_err(|_| ClientError::Interaction { reason: "parent permission request failed" }),
            };
            wait.finish().await;
            response
        })
    }));
    let urls = Arc::new(DelegateUrls::default());
    let capabilities = lock(&reply.interactions);
    let (form, url) = (capabilities.form, capabilities.url);
    drop(capabilities);
    for is_url in [false, true] {
        if (is_url && !url) || (!is_url && !form) {
            continue;
        }
        let parent = reply.clone();
        let tool_id = v2::ToolCallId::new(reply.tool_id(call));
        let attribution = attribution.clone();
        let urls = urls.clone();
        let handler: super::client::Handler<
            v2::CreateElicitationRequest,
            v2::CreateElicitationResponse,
        > = Arc::new(move |mut request, cancellation| {
            let parent = parent.clone();
            let tool_id = tool_id.clone();
            let attribution = attribution.clone();
            let urls = urls.clone();
            Box::pin(async move {
                request.message = format!("{attribution}: {}", request.message);
                let scope = v2::ElicitationSessionScope::new(parent.session_id.clone())
                    .tool_call_id(tool_id);
                let wait = Wait::begin(&parent).await?;
                let mut ordinary_wait = Some(wait);
                let child_id = match &mut request.mode {
                    v2::ElicitationMode::Form(mode) if !is_url => {
                        mode.scope = scope.into();
                        None
                    }
                    v2::ElicitationMode::Url(mode) if is_url => {
                        mode.scope = scope.into();
                        let child = mode.elicitation_id.to_string();
                        let id = {
                            let mut state = lock(&parent.interactions);
                            state.next_url += 1;
                            v2::ElicitationId::new(format!("bee-delegate-{}", state.next_url))
                        };
                        mode.elicitation_id = id.clone();
                        let mut pending = lock(&urls.0);
                        if pending.len() >= 16 || pending.contains_key(&child) {
                            return Err(ClientError::Limit {
                                resource: "delegate URL interactions",
                            });
                        }
                        pending
                            .insert(child.clone(), (id, ordinary_wait.take().expect("new wait")));
                        Some(child)
                    }
                    _ => {
                        return Err(ClientError::Interaction {
                            reason: "unsupported child elicitation mode",
                        });
                    }
                };
                let pending = {
                    let sessions = lock(&parent.sessions);
                    if !sessions.is_current(&parent.id, parent.token) {
                        return Err(ClientError::Cancelled);
                    }
                    parent.connection.send_request(request)
                };
                let response = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Err(ClientError::Cancelled),
                    response = pending.block_task() => response.map_err(|_| ClientError::Interaction { reason: "parent elicitation failed" }),
                };
                if let Some(wait) = ordinary_wait {
                    wait.finish().await;
                }
                let accepted = response
                    .as_ref()
                    .is_ok_and(|r| matches!(r.action, v2::ElicitationAction::Accept(_)));
                if !accepted && let Some(child) = child_id {
                    let wait = lock(&urls.0).remove(&child);
                    if let Some((_, wait)) = wait {
                        wait.finish().await;
                    }
                }
                if response
                    .as_ref()
                    .is_ok_and(|r| serde_json::to_vec(r).map_or(true, |b| b.len() > 128 * 1024))
                {
                    return Err(ClientError::Limit {
                        resource: "delegate elicitation response",
                    });
                }
                response
            })
        });
        if is_url {
            handlers.url = Some(handler);
        } else {
            handlers.form = Some(handler);
        }
    }
    if url {
        let urls = urls.clone();
        let parent = reply.clone();
        handlers.url_complete = Some(Arc::new(move |notification, _| {
            let urls = urls.clone();
            let parent = parent.clone();
            Box::pin(async move {
                let pending = lock(&urls.0).remove(&notification.elicitation_id.to_string());
                if let Some((id, wait)) = pending {
                    {
                        let sessions = lock(&parent.sessions);
                        if sessions.is_current(&parent.id, parent.token) {
                            parent
                                .connection
                                .send_notification(v2::CompleteElicitationNotification::new(id))
                                .map_err(|_| ClientError::Cancelled)?;
                        }
                    }
                    wait.finish().await;
                }
                Ok(())
            })
        }));
    }
    (handlers, urls)
}

impl Reply {
    pub(super) async fn login(
        &self,
        profile: &crate::account::Account,
        store: &crate::auth::FileStore,
        env: &crate::config::Environment,
        http: &dyn crate::http::LoginHttp,
    ) -> Result<(), super::TurnFailure> {
        let failed = || {
            super::TurnFailure::new(
                "browser sign-in did not complete; retry or run `bee auth login`",
            )
        };
        let flow = profile.login_spec().ok_or_else(failed)?;
        if flow.is_copilot() {
            return tokio::time::timeout(crate::login::DEFAULT_TIMEOUT, async {
                let login = crate::auth::copilot::SignIn::prepare(store, profile.name(), http)
                    .await
                    .map_err(|error| super::TurnFailure::new(error.to_string()))?;
                let id = {
                    let mut state = lock(&self.interactions);
                    state.next_url += 1;
                    format!("bee-login-{}", state.next_url)
                };
                let wait = Wait::begin(self).await.map_err(|_| failed())?;
                let request = v2::CreateElicitationRequest::new(
                    v2::ElicitationUrlMode::new(
                        v2::ElicitationSessionScope::new(self.session_id.clone()),
                        id.clone(),
                        login.url.clone(),
                    ),
                    format!(
                        "Continue with GitHub Copilot. Enter code: {}",
                        login.user_code
                    ),
                );
                let pending = {
                    let sessions = lock(&self.sessions);
                    if !sessions.is_current(&self.id, self.token) {
                        return Err(failed());
                    }
                    self.connection.send_request(request)
                };
                let response = pending.block_task().await.map_err(|_| failed())?;
                if !matches!(response.action, v2::ElicitationAction::Accept(_)) || !self.current() {
                    return Err(failed());
                }
                let credential = login
                    .complete(http)
                    .await
                    .map_err(|error| super::TurnFailure::new(error.to_string()))?;
                wait.finish().await;
                login
                    .commit_guarded(store, profile.name(), &credential, || {
                        let sessions = lock(&self.sessions);
                        sessions
                            .is_current(&self.id, self.token)
                            .then_some(sessions)
                    })
                    .await
                    .map_err(|error| super::TurnFailure::new(error.to_string()))?;
                let sessions = lock(&self.sessions);
                if !sessions.is_current(&self.id, self.token) {
                    return Err(failed());
                }
                self.connection
                    .send_notification(v2::CompleteElicitationNotification::new(id))
                    .map_err(|_| failed())?;
                Ok(())
            })
            .await
            .map_err(|_| failed())?;
        }
        if flow.is_chatgpt() {
            return tokio::time::timeout(crate::login::DEFAULT_TIMEOUT, async {
                let login = crate::auth::chatgpt::SignIn::prepare(store, profile.name()).await.map_err(|e| super::TurnFailure::new(e.to_string()))?;
                let id = {
                    let mut state = lock(&self.interactions);
                    state.next_url += 1;
                    format!("bee-login-{}", state.next_url)
                };
                let wait = Wait::begin(self).await.map_err(|_| failed())?;
                let request = v2::CreateElicitationRequest::new(v2::ElicitationUrlMode::new(v2::ElicitationSessionScope::new(self.session_id.clone()), id.clone(), login.url.clone()), "Continue with ChatGPT. Eligible inference uses your ChatGPT plan; manage usage at https://chatgpt.com/settings/usage");
                let pending = {
                    let sessions = lock(&self.sessions);
                    if !sessions.is_current(&self.id, self.token) { return Err(failed()); }
                    self.connection.send_request(request)
                };
                let response = pending.block_task().await.map_err(|_| failed())?;
                if !matches!(response.action, v2::ElicitationAction::Accept(_)) || !self.current() { return Err(failed()); }
                let credential = login.complete(http).await.map_err(|e| super::TurnFailure::new(e.to_string()))?;
                wait.finish().await;
                login.commit_guarded(store, profile.name(), &credential, || {
                    let sessions = lock(&self.sessions);
                    sessions.is_current(&self.id,self.token).then_some(sessions)
                }).await.map_err(|e| super::TurnFailure::new(e.to_string()))?;
                // Cancellation may follow a committed credential; gate completion publication too.
                let sessions = lock(&self.sessions);
                if !sessions.is_current(&self.id,self.token) { return Err(failed()); }
                self.connection.send_notification(v2::CompleteElicitationNotification::new(id)).map_err(|_| failed())?;
                Ok(())
            }).await.map_err(|_| failed())?;
        }
        let attempt = async {
            let login = crate::login::AsyncSignIn::prepare(flow)
                .await
                .map_err(|_| failed())?;
            let elicitation_id = {
                let mut state = lock(&self.interactions);
                state.next_url += 1;
                format!("bee-login-{}", state.next_url)
            };
            let wait = Wait::begin(self).await.map_err(|_| failed())?;
            let request = v2::CreateElicitationRequest::new(
                v2::ElicitationUrlMode::new(
                    v2::ElicitationSessionScope::new(self.session_id.clone()),
                    elicitation_id.clone(),
                    login.url.clone(),
                ),
                format!("Sign in to provider `{}` in your browser.", profile.name()),
            );
            let pending = {
                let sessions = lock(&self.sessions);
                if !sessions.is_current(&self.id, self.token) {
                    return Err(failed());
                }
                self.connection.send_request(request)
            };
            let response = pending.block_task().await.map_err(|_| failed())?;
            if !matches!(response.action, v2::ElicitationAction::Accept(_)) || !self.current() {
                return Err(failed());
            }
            let code = login.callback().await.map_err(|_| failed())?;
            wait.finish().await;
            let secret = login
                .exchange(flow, &code, http)
                .await
                .map_err(|_| failed())?;
            // Serialize the short final filesystem commit across ACP connections.
            // Cancellation and commit are linearized by the session gate; network
            // work never holds either lock and a committed key is never rolled back.
            let _store_guard = store.lock_async().await.map_err(|_| failed())?;
            let sessions = lock(&self.sessions);
            if !sessions.is_current(&self.id, self.token) {
                return Err(failed());
            }
            match crate::auth::resolve(profile, env, store, crate::auth::unix_now()) {
                Ok(_) => {}
                Err(crate::Error::AuthNotLoggedIn { .. } | crate::Error::AuthExpired { .. }) => {
                    if secret.expose().trim().is_empty() {
                        return Err(failed());
                    }
                    store
                        .put_locked(
                            crate::auth::Audience::Provider(profile.name()),
                            &crate::auth::Credential::new(crate::auth::Secret::new(
                                secret.expose().trim(),
                            )),
                        )
                        .map_err(|_| failed())?;
                }
                // A store that became unreadable/unsafe during login is not authorization to replace it.
                Err(_) => return Err(failed()),
            }
            self.connection
                .send_notification(v2::CompleteElicitationNotification::new(elicitation_id))
                .map_err(|_| failed())?;
            Ok(())
        };
        tokio::time::timeout(crate::login::DEFAULT_TIMEOUT, attempt)
            .await
            .map_err(|_| failed())?
    }

    pub(super) fn current(&self) -> bool {
        lock(&self.sessions).is_current(&self.id, self.token)
    }

    async fn publish_phase(&self) -> Result<(), Error> {
        // Publication shares the cancellation gate, preventing late running updates.
        let history = {
            let sessions = lock(&self.sessions);
            if !sessions.is_current(&self.id, self.token) {
                return Err(Error::Cancelled);
            }
            let state = lock(&self.interactions);
            let waiting = state.waiting.get(&self.id) == Some(&self.token);
            let Some(history) = sessions.history(&self.id) else {
                return send_update(&self.connection, &self.session_id, phase(waiting))
                    .map_err(|_| Error::Cancelled);
            };
            history
        };
        let connection = self.connection.clone();
        let session_id = self.session_id.clone();
        let sessions = std::sync::Arc::clone(&self.sessions);
        let id = self.id.clone();
        let token = self.token;
        let interactions = self.interactions.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        // Await publication before sending the associated outgoing request;
        // permission/form/login requests must not overtake requires_action.
        history
            .publish(move || {
                let sessions = lock(&sessions);
                let result = if sessions.is_current(&id, token) {
                    let waiting = lock(&interactions).waiting.get(&id) == Some(&token);
                    send_update(&connection, &session_id, phase(waiting))
                        .map_err(|_| Error::Cancelled)
                } else {
                    Err(Error::Cancelled)
                };
                let _ = send.send(result);
            })
            .await;
        receive.await.unwrap_or(Err(Error::Cancelled))
    }

    pub(super) async fn permission(
        &self,
        subject: v2::RequestPermissionSubject,
        title: String,
    ) -> Result<bool, Error> {
        let wait = Wait::begin(self).await.map_err(|_| Error::Cancelled)?;
        let options = vec![
            v2::PermissionOption::new(
                "allow_once",
                "Allow once",
                v2::PermissionOptionKind::AllowOnce,
            ),
            v2::PermissionOption::new(
                "reject_once",
                "Reject once",
                v2::PermissionOptionKind::RejectOnce,
            ),
            v2::PermissionOption::new(
                "allow_always",
                "Allow always",
                v2::PermissionOptionKind::AllowAlways,
            ),
            v2::PermissionOption::new(
                "reject_always",
                "Reject always",
                v2::PermissionOptionKind::RejectAlways,
            ),
        ];
        let request =
            v2::RequestPermissionRequest::new(self.session_id.clone(), title, options.clone())
                .subject(subject);
        let pending = {
            // SDK send_request publishes immediately, so gate publication too.
            let sessions = lock(&self.sessions);
            if !sessions.is_current(&self.id, self.token) {
                return Err(Error::Cancelled);
            }
            self.connection.send_request(request)
        };
        let response =
            tokio::time::timeout(crate::login::DEFAULT_TIMEOUT, pending.block_task()).await;
        wait.finish().await;
        // “Always” changes client policy only; Bee resolves this invocation and stores nothing.
        if !self.current() {
            return Err(Error::Cancelled);
        }
        Ok(matches!(response, Ok(Ok(v2::RequestPermissionResponse {
            outcome: v2::RequestPermissionOutcome::Selected(ref selected), ..
        })) if options.iter().any(|option| option.option_id == selected.option_id
            && matches!(option.kind, v2::PermissionOptionKind::AllowOnce | v2::PermissionOptionKind::AllowAlways))))
    }

    pub(super) async fn ask(&self, call: &Call, limit: usize) -> Result<ToolResult, Error> {
        let question = call
            .arguments
            .get("question")
            .and_then(serde_json::Value::as_str);
        if call
            .arguments
            .as_object()
            .is_none_or(|object| object.len() != 1)
        {
            return Ok(failure("Supply only a question field."));
        }
        let Some(question) = question.filter(|s| !s.trim().is_empty() && s.len() <= 4096) else {
            return Ok(failure("Supply a nonempty question of at most 4096 bytes."));
        };
        if !lock(&self.interactions).form {
            return Ok(failure("The client does not support form input."));
        }
        let wait = Wait::begin(self).await.map_err(|_| Error::Cancelled)?;
        let request = v2::CreateElicitationRequest::new(
            v2::ElicitationFormMode::new(
                v2::ElicitationSessionScope::new(self.session_id.clone())
                    .tool_call_id(v2::ToolCallId::new(self.tool_id(call))),
                v2::ElicitationSchema::new().property(
                    "answer",
                    v2::StringPropertySchema::new()
                        .min_length(1_u32)
                        .max_length(4096_u32),
                    true,
                ),
            ),
            question,
        );
        let pending = {
            let sessions = lock(&self.sessions);
            if !sessions.is_current(&self.id, self.token) {
                return Err(Error::Cancelled);
            }
            self.connection.send_request(request)
        };
        let response =
            tokio::time::timeout(crate::login::DEFAULT_TIMEOUT, pending.block_task()).await;
        wait.finish().await;
        if !self.current() {
            return Err(Error::Cancelled);
        }
        if let Ok(Ok(response)) = response
            && let v2::ElicitationAction::Accept(accepted) = response.action
            && let Some(content) = accepted.content
            && content.len() == 1
            && let Some(v2::ElicitationContentValue::String(answer)) = content.get("answer")
            && !answer.is_empty()
            && answer.len() <= 4096
        {
            let output = json!({"data":{"answer":answer}}).to_string();
            if output.len() <= limit {
                return Ok(ToolResult {
                    output,
                    is_error: false,
                });
            }
        }
        Ok(failure(
            "User input was declined, cancelled, unavailable, or invalid; no answer was supplied.",
        ))
    }
}

pub(super) fn phase(waiting: bool) -> v2::SessionUpdate {
    v2::SessionUpdate::StateUpdate(if waiting {
        v2::StateUpdate::RequiresAction(v2::RequiresActionStateUpdate::new())
    } else {
        v2::StateUpdate::Running(v2::RunningStateUpdate::new())
    })
}

fn failure(message: &str) -> ToolResult {
    ToolResult {
        output: json!({"error":{"code":"interaction","message":message}}).to_string(),
        is_error: true,
    }
}

pub(super) struct Native<'a> {
    pub inner: NativeExecutor<'a>,
    pub reply: &'a Reply,
    pub permissions: &'a crate::permissions::Settings,
}

impl Native<'_> {
    fn dispatch<'a>(
        &'a self,
        call: &'a Call,
        snapshot: Option<std::sync::Arc<str>>,
        observer: Option<&'a dyn crate::agent::Observer>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>> {
        Box::pin(async move {
            use crate::agent::{Executor, Observation};
            use crate::permissions::{Effect, Resource};
            if call.name == "delegate" {
                return self
                    .reply
                    .delegation
                    .execute(
                        self.reply,
                        call,
                        self.permissions,
                        self.inner.cwd(),
                        observer,
                    )
                    .await;
            }
            if matches!(call.name.as_str(), "update_plan" | "ask_user") {
                if let Some(observer) = observer {
                    observer.observe(call, Observation::Started).await?;
                }
                return if call.name == "update_plan" {
                    self.reply.update_plan(call).await
                } else {
                    self.reply.ask(call, self.output_limit()).await
                };
            }
            if self.inner.preflight(call).is_err() {
                // Never delegate invalid input, including to an injected custom implementation.
                return Ok(ToolResult {
                    output: json!({"error":{"code":"invalid_arguments","message":"Tool arguments are invalid; follow the schema and configured host ceilings."}}).to_string(),
                    is_error: true,
                });
            }
            let checks = self.inner.permissions(call);
            match self.permissions.evaluate(&checks, self.inner.cwd()) {
                Effect::Deny => {
                    return Ok(failure(
                        "The configured permission policy denied this operation; it was not executed.",
                    ));
                }
                Effect::Allow => {}
                Effect::Ask => {
                    let (subject, title) = if let Some(command) = checks.iter().find_map(|check| {
                        if let Resource::Command(command) = &check.resource {
                            Some(command)
                        } else {
                            None
                        }
                    }) {
                        let cwd = self.reply.input.as_ref().ok_or(Error::Tool)?.cwd.clone();
                        (
                            v2::RequestPermissionSubject::Command(
                                v2::CommandPermissionSubject::new(command.clone(), cwd)
                                    .tool_call_id(v2::ToolCallId::new(self.reply.tool_id(call))),
                            ),
                            "Run Bash command".into(),
                        )
                    } else {
                        let description = self.inner.description(call);
                        (
                            self.reply.tool_permission_subject(call, &description)?,
                            format!("Run {} tool", call.name),
                        )
                    };
                    if !self.reply.permission(subject, title).await? {
                        return Ok(failure(
                            "The operation was not approved; it was not executed.",
                        ));
                    }
                }
            }
            if !self.reply.current() {
                return Err(Error::Cancelled);
            }
            if let Some(observer) = observer {
                observer.observe(call, Observation::Started).await?;
            }
            let mut execution = match snapshot {
                Some(snapshot) => self.inner.execute_snapshot(call, snapshot),
                None => self.inner.execute(call),
            };
            // Linearize native startup/publication against cancel, including snapshot execution.
            futures::future::poll_fn(|cx| {
                let sessions = lock(&self.reply.sessions);
                if !sessions.is_current(&self.reply.id, self.reply.token) {
                    return std::task::Poll::Ready(Err(Error::Cancelled));
                }
                execution.as_mut().poll(cx)
            })
            .await
        })
    }
    pub fn definitions(&self) -> Vec<Tool> {
        let mut definitions = self.inner.definitions();
        definitions.push(super::plans::definition());
        if self.reply.delegation.available() {
            definitions.push(self.reply.delegation.definition());
        }
        if lock(&self.reply.interactions).form {
            definitions.push(Tool {
                name: "ask_user".into(),
                description: "Ask the user a question needed to continue. Do not request secrets.".into(),
                parameters: json!({"type":"object","properties":{"question":{"type":"string","minLength":1,"maxLength":4096}},"required":["question"],"additionalProperties":false}),
            });
        }
        definitions
    }
}

impl Reply {
    pub(super) fn tool_permission_subject(
        &self,
        call: &Call,
        description: &crate::presentation::Description,
    ) -> Result<v2::RequestPermissionSubject, Error> {
        // Use the already-announced ID so clients can associate authorization
        // with the pending card instead of inventing an unrelated operation.
        let update = serde_json::from_value::<v2::ToolCallUpdate>(json!({
            "toolCallId": self.tool_id(call),
            "title": description.name,
            "kind": description.kind,
            "status": "pending",
            "locations": description.locations.iter().map(|path| json!({"path":path})).collect::<Vec<_>>(),
            "rawInput": crate::presentation::bounded_value(&call.arguments),
        })).map_err(|_| Error::Tool)?;
        Ok(v2::RequestPermissionSubject::from(update))
    }
}

impl crate::agent::Executor for Native<'_> {
    fn execute_observed<'a>(
        &'a self,
        call: &'a Call,
        snapshot: Option<std::sync::Arc<str>>,
        observer: Option<&'a dyn crate::agent::Observer>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>> {
        self.dispatch(call, snapshot, observer)
    }
    fn description(&self, call: &Call) -> crate::presentation::Description {
        self.inner.description(call)
    }
    fn targets(&self, call: &Call) -> Vec<crate::tool::Target> {
        self.inner.targets(call)
    }
    fn output_limit(&self) -> usize {
        self.inner.output_limit()
    }
    fn target(&self, call: &Call) -> Option<crate::tool::Target> {
        self.inner.target(call)
    }
    fn execute_snapshot<'a>(
        &'a self,
        call: &'a Call,
        snapshot: std::sync::Arc<str>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>> {
        self.dispatch(call, Some(snapshot), None)
    }
    fn execute<'a>(
        &'a self,
        call: &'a Call,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>> {
        self.dispatch(call, None, None)
    }
}
