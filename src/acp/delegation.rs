//! Bounded child tasks owned outside abortable parent inference.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt, Shared};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{Reply, client, lock, v2};
use crate::agent::{Observation, Observer, ToolResult};
use crate::llm::{Call, Error, Tool};
use crate::permissions::{Check, Effect};

type Key = (String, u64);
type Completion = Shared<BoxFuture<'static, ()>>;

struct Owner {
    cancellation: CancellationToken,
    done: Completion,
}
#[derive(Default)]
struct State {
    closing: bool,
    turns: BTreeMap<Key, Vec<Owner>>,
}

pub(super) struct Manager {
    agents: client::config::Agents,
    settings: crate::delegation::Settings,
    permits: Arc<tokio::sync::Semaphore>,
    state: Mutex<State>,
}

impl Default for Manager {
    fn default() -> Self {
        Self::new(Default::default(), Default::default())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    tasks: Vec<Task>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    id: String,
    agent: String,
    task: String,
    #[serde(default)]
    context: String,
}

pub(super) fn definition() -> Tool {
    Tool {
        name: "delegate".into(),
        description: "Delegate focused independent tasks to explicitly named configured ACP agents, then synthesize their structured results. Supply only the context each task needs. Child tasks may run concurrently.".into(),
        parameters: json!({"type":"object","additionalProperties":false,"required":["tasks"],"properties":{"tasks":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","additionalProperties":false,"required":["id","agent","task"],"properties":{"id":{"type":"string","minLength":1,"maxLength":64},"agent":{"type":"string","minLength":1,"maxLength":128},"task":{"type":"string","minLength":1,"maxLength":32768},"context":{"type":"string","maxLength":32768}}}}}}),
    }
}

fn failure(code: &str, message: &str) -> ToolResult {
    ToolResult {
        output: json!({"error":{"code":code,"message":message}}).to_string(),
        is_error: true,
    }
}

impl Manager {
    pub fn new(agents: client::config::Agents, settings: crate::delegation::Settings) -> Self {
        Self {
            agents,
            settings,
            permits: Arc::new(tokio::sync::Semaphore::new(settings.max_concurrent)),
            state: Mutex::default(),
        }
    }

    pub fn available(&self) -> bool {
        self.settings.available() && self.agents.names().next().is_some()
    }

    pub fn definition(&self) -> Tool {
        let mut tool = definition();
        tool.parameters["properties"]["tasks"]["items"]["properties"]["id"]["pattern"] =
            json!("^[A-Za-z0-9_-]+$");
        tool.parameters["properties"]["tasks"]["maxItems"] = json!(self.settings.max_children);
        tool.parameters["properties"]["tasks"]["items"]["properties"]["agent"]["enum"] =
            json!(self.agents.names().collect::<Vec<_>>());
        tool
    }

    pub async fn execute(
        &self,
        reply: &Reply,
        call: &Call,
        permissions: &crate::permissions::Settings,
        cwd: &Path,
        observer: Option<&dyn Observer>,
    ) -> Result<ToolResult, Error> {
        if !self.available() {
            return Ok(failure(
                "delegation_denied",
                "Delegation is disabled or the remaining depth is exhausted; complete the task locally.",
            ));
        }
        // Bound deserialization and the aggregate prompt payload independently of schema hints.
        if call.arguments.to_string().len() > 128 * 1024 {
            return Ok(failure(
                "invalid_arguments",
                "Supply at most 128 KiB of explicit task context.",
            ));
        }
        let Ok(batch) = serde_json::from_value::<Batch>(call.arguments.clone()) else {
            return Ok(failure(
                "invalid_arguments",
                "Supply a tasks list containing only id, agent, task and optional context.",
            ));
        };
        let mut ids = BTreeSet::new();
        if batch.tasks.is_empty()
            || batch.tasks.len() > self.settings.max_children
            || batch.tasks.iter().any(|t| {
                t.id.is_empty()
                    || t.id.len() > 64
                    || !t
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    || !ids.insert(t.id.clone())
                    || t.agent.len() > 128
                    || self.agents.get(&t.agent).is_err()
                    || t.task.trim().is_empty()
                    || t.task.len() > 32768
                    || t.context.len() > 32768
            })
        {
            return Ok(failure(
                "invalid_arguments",
                "Use unique short task IDs, configured agent names and bounded nonempty tasks; reduce the batch to the child limit.",
            ));
        }
        let key = (reply.id.clone(), reply.token);
        {
            let state = lock(&self.state);
            if state.closing {
                return Err(Error::Cancelled);
            }
            // Reject exhausted turns before asking the client for unusable launch consent.
            if state.turns.get(&key).map_or(0, Vec::len) + batch.tasks.len()
                > self.settings.max_children
            {
                return Ok(failure(
                    "child_limit",
                    "The per-turn child budget is exhausted; synthesize existing results or continue locally.",
                ));
            }
        }
        for name in batch
            .tasks
            .iter()
            .map(|t| &t.agent)
            .collect::<BTreeSet<_>>()
        {
            let effect =
                permissions.evaluate(&[Check::value("delegate", name.clone(), Effect::Ask)], cwd);
            let allowed = match effect {
                Effect::Allow => true,
                Effect::Deny => false,
                Effect::Ask => {
                    reply
                        .permission(
                            reply.tool_permission_subject(
                                call,
                                &crate::presentation::Description {
                                    name: format!("Delegate to {name}"),
                                    kind: "other",
                                    locations: Vec::new(),
                                },
                            )?,
                            format!("Delegate task to {name}"),
                        )
                        .await?
                }
            };
            if !allowed {
                return Ok(failure(
                    "permission_denied",
                    "Delegation was not approved; no child tasks were launched.",
                ));
            }
        }
        if !reply.current() {
            return Err(Error::Cancelled);
        }
        if let Some(observer) = observer {
            observer.observe(call, Observation::Started).await?;
        }
        let cancellation = CancellationToken::new();
        // Dropped parent execution revokes workers, but their supervisors still await teardown.
        let _cancel = cancellation.clone().drop_guard();
        let mut receivers = Vec::new();
        {
            let mut state = lock(&self.state);
            if state.closing {
                return Err(Error::Cancelled);
            }
            let owners = state.turns.entry(key).or_default();
            // Consent/publication awaited above; recheck admission atomically before any spawn.
            if owners.len() + batch.tasks.len() > self.settings.max_children {
                return Ok(failure(
                    "child_limit",
                    "The per-turn child budget is exhausted; synthesize existing results or continue locally.",
                ));
            }
            // Reserve per-task identity/status/error overhead before allocating answer content.
            let content_budget = (crate::tool::MAX_OUTPUT_BYTES / batch.tasks.len())
                .saturating_sub(4096)
                .min(128 * 1024);
            let deadline = tokio::time::Instant::now() + self.settings.timeout;
            for task in batch.tasks {
                let (send, receive) = tokio::sync::oneshot::channel();
                let (finished, completion) = tokio::sync::oneshot::channel();
                let token = cancellation.child_token();
                owners.push(Owner {
                    cancellation: token.clone(),
                    done: async move {
                        let _ = completion.await;
                    }
                    .boxed()
                    .shared(),
                });
                let definition = self
                    .agents
                    .get(&task.agent)
                    .expect("validated name")
                    .clone();
                let permits = self.permits.clone();
                let settings = self.settings;
                let reply = reply.clone();
                let call = call.clone();
                let cwd = cwd.to_path_buf();
                receivers.push(receive);
                let failed = json!({"id":task.id,"agent":task.agent,"status":"failed","messages":[],"partial":true,"truncated":false,"error":{"code":"child_panic","message":"Child supervision failed; retry the task."}});
                tokio::spawn(async move {
                    let result = std::panic::AssertUnwindSafe(run_task(
                        task,
                        definition,
                        settings,
                        permits,
                        token,
                        reply,
                        call,
                        cwd,
                        content_budget,
                        deadline,
                    ))
                    .catch_unwind()
                    .await;
                    let result = result.unwrap_or(failed);
                    let _ = send.send(result);
                    let _ = finished.send(());
                });
            }
        }
        let mut results = Vec::new();
        for receiver in receivers {
            results.push(receiver.await.map_err(|_| Error::Tool)?);
        }
        let is_error = results.iter().any(|r| r["status"] != "completed");
        let output = json!({"results":results}).to_string();
        if output.len() > crate::tool::MAX_OUTPUT_BYTES {
            return Ok(failure(
                "result_limit",
                "Delegated results exceeded the envelope limit; request smaller answers.",
            ));
        }
        Ok(ToolResult { output, is_error })
    }

    pub async fn finish(&self, id: &str, token: u64) {
        let key = (id.to_owned(), token);
        let completions = {
            let state = lock(&self.state);
            state
                .turns
                .get(&key)
                .map(|owners| {
                    owners
                        .iter()
                        .map(|owner| {
                            owner.cancellation.cancel();
                            owner.done.clone()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        for completion in completions {
            completion.await;
        }
        lock(&self.state).turns.remove(&key);
    }

    pub async fn shutdown(&self) {
        let keys = {
            let mut state = lock(&self.state);
            state.closing = true;
            // Revoke every turn before awaiting any one cleanup; queued siblings must not launch.
            for owners in state.turns.values() {
                for owner in owners {
                    owner.cancellation.cancel();
                }
            }
            state.turns.keys().cloned().collect::<Vec<_>>()
        };
        for (id, token) in keys {
            self.finish(&id, token).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    task: Task,
    definition: client::config::Definition,
    settings: crate::delegation::Settings,
    permits: Arc<tokio::sync::Semaphore>,
    cancellation: CancellationToken,
    reply: Reply,
    call: Call,
    cwd: std::path::PathBuf,
    budget: usize,
    deadline: tokio::time::Instant,
) -> Value {
    let mut result = json!({"id":task.id,"agent":task.agent,"status":"failed","messages":[],"partial":true,"truncated":false,"error":{"code":"child_not_completed","message":"The child task did not complete; narrow the task or check the delegate and configured limits."}});
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => { result["status"] = json!("cancelled"); return result },
        permit = tokio::time::timeout_at(deadline, permits.acquire_owned()) => match permit {
            Ok(Ok(permit)) => permit,
            _ => { result["status"] = json!("timed_out"); return result },
        }
    };
    let _permit = permit;
    // A permit and queue deadline may become ready in the same poll; expiry wins launch.
    if tokio::time::Instant::now() >= deadline {
        result["status"] = json!("timed_out");
        return result;
    }
    if !reply.current() || cancellation.is_cancelled() {
        result["status"] = json!("cancelled");
        return result;
    }
    progress(
        &reply,
        &call,
        format!("Delegate {} / {} started.", task.agent, task.id),
    );
    let capture = Arc::new(Mutex::new(None::<client::Capture>));
    let save = capture.clone();
    let prompt = if task.context.is_empty() {
        task.task
    } else {
        format!(
            "Task:\n{}\n\nTask-specific context:\n{}",
            task.task, task.context
        )
    };
    let (handlers, urls) =
        super::interactions::delegate_handlers(&reply, &call, &task.agent, &task.id);
    let token = cancellation.clone();
    let launch_token = cancellation.child_token();
    let options = client::ClientOptions {
        max_bytes: budget,
        prompt_timeout: settings.timeout,
        request_timeout: settings.timeout.min(std::time::Duration::from_secs(30)),
        ..Default::default()
    };
    let connection = client::connect_with_overlay(
        &definition,
        handlers,
        options,
        launch_token.clone(),
        Some(settings.depth - 1),
        move |connection| {
            Box::pin(async move {
                connection.new_session(&cwd).await?;
                let prompt = connection.start_prompt(
                    vec![v2::ContentBlock::Text(v2::TextContent::new(prompt))],
                    token,
                )?;
                *lock(&save) = Some(prompt.capture());
                prompt.finish().await?;
                Ok(())
            })
        },
    );
    tokio::pin!(connection);
    let mut timed_out = false;
    let outcome = tokio::select! {
        biased;
        outcome = &mut connection => outcome,
        () = tokio::time::sleep_until(deadline) => {
            timed_out = true;
            launch_token.cancel();
            // Keep the SDK scope alive so normal cancel/close runs before releasing capacity.
            connection.await
        }
    };
    let observed = lock(&capture).take().map(|c| c.take()).unwrap_or_default();
    urls.finish().await;
    result["messages"] = serde_json::to_value(observed.messages).unwrap_or_else(|_| json!([]));
    result["truncated"] =
        json!(observed.truncated || matches!(outcome, Err(client::Error::Limit { .. })));
    let reason = observed.idle.and_then(|idle| idle.stop_reason);
    let stop = match reason {
        Some(v2::StopReason::EndTurn) => "end_turn",
        Some(v2::StopReason::MaxTokens) => "max_tokens",
        Some(v2::StopReason::MaxTurnRequests) => "max_turn_requests",
        Some(v2::StopReason::Refusal) => "refusal",
        Some(v2::StopReason::Cancelled) => "cancelled",
        Some(v2::StopReason::Error(_)) => "error",
        _ => "unknown",
    };
    let status = if timed_out || matches!(outcome, Err(client::Error::Timeout { .. })) {
        "timed_out"
    } else if cancellation.is_cancelled() {
        "cancelled"
    } else if outcome.is_err() {
        "failed"
    } else {
        match stop {
            "refusal" => "refused",
            "cancelled" => "cancelled",
            "error" | "max_turn_requests" => "failed",
            _ => "completed",
        }
    };
    result["status"] = json!(status);
    result["stop_reason"] = json!(stop);
    result["partial"] = json!(status != "completed" || stop != "end_turn");
    progress(
        &reply,
        &call,
        format!("Delegate {} / {}: {status}.", task.agent, task.id),
    );
    if let Err(error) = outcome {
        // This typed client's Display is safe; arbitrary remote stop error data is never echoed.
        result["error"] = json!({"code":"child_execution","message":error.to_string()});
    } else if status != "completed" {
        result["error"] = json!({"code":"child_stopped","message":"The delegate did not complete successfully; use partial output or retry with a narrower task."});
    } else {
        result
            .as_object_mut()
            .expect("result envelope")
            .remove("error");
    }
    // Capture bounds include structured messages; account separately for the enclosing JSON.
    if result.to_string().len() > budget + 2048 {
        result["messages"] = json!([]);
        result["truncated"] = json!(true);
        result["partial"] = json!(true);
    }
    result
}

fn progress(reply: &Reply, call: &Call, note: String) {
    // Child execution is supervised outside inference; explicitly gate its presentation.
    let sessions = lock(&reply.sessions);
    if sessions.is_current(&reply.id, reply.token) {
        let _ = reply.artifact(&call.id, crate::presentation::Artifact::Note(note));
    }
}
