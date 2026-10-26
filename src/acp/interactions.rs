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
    pub next_url: u64,
}

impl Reply {
    pub(super) async fn login(
        &self,
        profile: &crate::provider::Profile,
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
                self.phase(true).await.map_err(|_| failed())?;
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
                self.phase(false).await.map_err(|_| failed())?;
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
                self.phase(true).await.map_err(|_| failed())?;
                let request = v2::CreateElicitationRequest::new(v2::ElicitationUrlMode::new(v2::ElicitationSessionScope::new(self.session_id.clone()), id.clone(), login.url.clone()), "Continue with ChatGPT. Eligible inference uses your ChatGPT plan; manage usage at https://chatgpt.com/settings/usage");
                let pending = {
                    let sessions = lock(&self.sessions);
                    if !sessions.is_current(&self.id, self.token) { return Err(failed()); }
                    self.connection.send_request(request)
                };
                let response = pending.block_task().await.map_err(|_| failed())?;
                if !matches!(response.action, v2::ElicitationAction::Accept(_)) || !self.current() { return Err(failed()); }
                let credential = login.complete(http).await.map_err(|e| super::TurnFailure::new(e.to_string()))?;
                self.phase(false).await.map_err(|_| failed())?;
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
            self.phase(true).await.map_err(|_| failed())?;
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
            self.phase(false).await.map_err(|_| failed())?;
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

    pub(super) async fn phase(&self, waiting: bool) -> Result<(), Error> {
        // Publication shares the cancellation gate, preventing late running updates.
        let history = {
            let sessions = lock(&self.sessions);
            if !sessions.is_current(&self.id, self.token) {
                return Err(Error::Cancelled);
            }
            let mut state = lock(&self.interactions);
            if waiting {
                state.waiting.insert(self.id.clone(), self.token);
            } else {
                state.waiting.remove(&self.id);
            }
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
        let (send, receive) = tokio::sync::oneshot::channel();
        // Await publication before sending the associated outgoing request;
        // permission/form/login requests must not overtake requires_action.
        history
            .publish(move || {
                let sessions = lock(&sessions);
                let result = if sessions.is_current(&id, token) {
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

    async fn permission(
        &self,
        subject: v2::RequestPermissionSubject,
        title: String,
    ) -> Result<bool, Error> {
        self.phase(true).await?;
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
        self.phase(false).await?;
        // “Always” changes client policy only; Bee resolves this invocation and stores nothing.
        Ok(matches!(response, Ok(Ok(v2::RequestPermissionResponse {
            outcome: v2::RequestPermissionOutcome::Selected(ref selected), ..
        })) if options.iter().any(|option| option.option_id == selected.option_id
            && matches!(option.kind, v2::PermissionOptionKind::AllowOnce | v2::PermissionOptionKind::AllowAlways))))
    }

    async fn ask(&self, call: &Call, limit: usize) -> Result<ToolResult, Error> {
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
        self.phase(true).await?;
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
        self.phase(false).await?;
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
                        let update = serde_json::from_value::<v2::ToolCallUpdate>(json!({
                            "toolCallId": self.reply.tool_id(call),
                            "title": description.name,
                            "kind": description.kind,
                            "status": "pending",
                            "locations": description.locations.iter().map(|path| json!({"path":path})).collect::<Vec<_>>(),
                            "rawInput": crate::presentation::bounded_value(&call.arguments),
                        })).map_err(|_| Error::Tool)?;
                        (
                            v2::RequestPermissionSubject::from(update),
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
