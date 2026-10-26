//! A bounded inference loop with caller-owned tools, independent of ACP.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use crate::llm::{Call, Error, Message, Model, Stop, Tool, Usage};

/// Per-turn budgets, validated before any model or tool work begins.
#[derive(Clone, Copy)]
pub struct Limits {
    max_steps: usize,
    max_tool_calls: usize,
}

impl std::fmt::Debug for Limits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Limits([redacted])")
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 16,
            max_tool_calls: 64,
        }
    }
}

impl Limits {
    /// Construct positive model-request and individual-tool-call budgets.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::AgentSetting`] if either budget is zero.
    pub fn new(max_steps: usize, max_tool_calls: usize) -> crate::Result<Self> {
        for (key, value) in [("max_steps", max_steps), ("max_tool_calls", max_tool_calls)] {
            if value == 0 {
                return Err(crate::Error::AgentSetting {
                    key: format!("agent.{key}"),
                    path: "<runtime>".into(),
                    reason: "must be a positive integer",
                    instruction: "choose a value greater than zero",
                });
            }
        }
        Ok(Self {
            max_steps,
            max_tool_calls,
        })
    }

    /// Read only the agent-owned table from merged configuration.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::AgentSetting`] for unknown keys, incorrect types,
    /// or budgets which are not positive integers representable on this platform.
    pub fn from_config(config: &crate::config::Config) -> crate::Result<Self> {
        let mut limits = Self::default();
        // Leaf provenance avoids blaming a later layer that changed another budget.
        let failure = |key: &str, reason, instruction| crate::Error::AgentSetting {
            key: key.into(),
            path: config.origin(key).map_or_else(
                || "<configuration>".into(),
                |path| path.display().to_string(),
            ),
            reason,
            instruction,
        };
        let Some(value) = config.get("agent") else {
            return Ok(limits);
        };
        let table = value
            .as_table()
            .ok_or_else(|| failure("agent", "must be a table", "use an [agent] table"))?;
        for (key, value) in table {
            let target = match key.as_str() {
                "max_steps" => &mut limits.max_steps,
                "max_tool_calls" => &mut limits.max_tool_calls,
                _ => {
                    return Err(failure(
                        &format!("agent.{key}"),
                        "is unknown",
                        "use max_steps or max_tool_calls",
                    ));
                }
            };
            *target = value
                .as_integer()
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    failure(
                        &format!("agent.{key}"),
                        "must be a positive integer",
                        "choose a positive integer representable on this platform",
                    )
                })?;
        }
        Ok(limits)
    }

    /// Maximum model requests, including the final response.
    pub fn max_steps(self) -> usize {
        self.max_steps
    }

    /// Maximum individual tool invocations across all batches in a turn.
    pub fn max_tool_calls(self) -> usize {
        self.max_tool_calls
    }
}

/// Tool output; an ordinary tool failure is data for the next model step.
pub struct ToolResult {
    /// Text or JSON-encoded output.
    pub output: String,
    /// Whether the tool failed.
    pub is_error: bool,
}

/// Implemented by a caller that owns actual tool execution.
pub trait Executor: Send + Sync {
    /// Publish execution start at dispatch time, after any implementation-owned authorization.
    fn execute_observed<'a>(
        &'a self,
        call: &'a Call,
        snapshot: Option<std::sync::Arc<str>>,
        observer: Option<&'a dyn Observer>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(observer) = observer {
                observer.observe(call, Observation::Started).await?;
            }
            match snapshot {
                Some(snapshot) => self.execute_snapshot(call, snapshot).await,
                None => self.execute(call).await,
            }
        })
    }
    /// Implementation-owned presentation, without interpreting arbitrary output.
    fn description(&self, call: &Call) -> crate::presentation::Description {
        crate::presentation::Description {
            name: call.name.clone(),
            kind: "other",
            locations: Vec::new(),
        }
    }
    /// All affected scopes; default preserves existing custom executors.
    fn targets(&self, call: &Call) -> Vec<crate::tool::Target> {
        self.target(call).into_iter().collect()
    }
    /// Bound synthesized guidance results by the same budget as native output.
    fn output_limit(&self) -> usize {
        crate::tool::MAX_OUTPUT_BYTES
    }

    /// Optional structured target; defaults preserve caller-owned executor behavior.
    fn target(&self, _call: &Call) -> Option<crate::tool::Target> {
        None
    }

    /// Execute a skill read against the same complete snapshot retained as guidance.
    fn execute_snapshot<'a>(
        &'a self,
        call: &'a Call,
        _snapshot: std::sync::Arc<str>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>> {
        self.execute(call)
    }
    /// Execute a complete call. Dropping the future must stop pending work.
    fn execute<'a>(
        &'a self,
        call: &'a Call,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>>;
}

/// Bind a registered native-tool catalog to one session/turn's execution context.
///
/// Obtain advertised definitions from this same adapter so dispatch and the
/// model's catalog cannot accidentally use different registries.
pub struct NativeExecutor<'a> {
    registry: &'a crate::tool::Registry,
    context: crate::tool::Context,
}

impl<'a> NativeExecutor<'a> {
    /// Bind tools to an explicit session directory, cancellation, and byte budget.
    pub fn new(registry: &'a crate::tool::Registry, context: crate::tool::Context) -> Self {
        Self { registry, context }
    }

    /// Convert the registered catalog to provider-neutral inference definitions.
    pub fn definitions(&self) -> Vec<Tool> {
        self.registry
            .definitions()
            .map(|definition| Tool {
                name: definition.name.clone(),
                description: definition.description.clone(),
                parameters: definition.input_schema.clone(),
            })
            .collect()
    }

    pub(crate) fn preflight(&self, call: &Call) -> Result<(), crate::tool::Failure> {
        self.registry.preflight(&call.name, &call.arguments)
    }

    pub(crate) fn permissions(&self, call: &Call) -> Vec<crate::permissions::Check> {
        self.registry.permissions(&call.name, &call.arguments)
    }

    pub(crate) fn cwd(&self) -> &std::path::Path {
        self.context.cwd()
    }
}

impl Executor for NativeExecutor<'_> {
    fn description(&self, call: &Call) -> crate::presentation::Description {
        self.registry.description(
            &call.name,
            &call.arguments,
            &self.context.for_call(&call.id),
        )
    }
    fn targets(&self, call: &Call) -> Vec<crate::tool::Target> {
        self.registry.targets(
            &call.name,
            &call.arguments,
            &self.context.for_call(&call.id),
        )
    }
    fn output_limit(&self) -> usize {
        self.context.output_limit()
    }

    fn target(&self, call: &Call) -> Option<crate::tool::Target> {
        self.registry.target(&call.name, &call.arguments)
    }

    fn execute_snapshot<'a>(
        &'a self,
        call: &'a Call,
        snapshot: std::sync::Arc<str>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>> {
        Box::pin(async move {
            let result = self
                .registry
                .invoke_snapshot(
                    &call.name,
                    &call.arguments,
                    &self.context.for_call(&call.id),
                    snapshot,
                )
                .await
                .map_err(|_| Error::Cancelled)?;
            Ok(ToolResult {
                output: result.json,
                is_error: result.is_error,
            })
        })
    }
    fn execute<'a>(
        &'a self,
        call: &'a Call,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>> {
        Box::pin(async move {
            let result = self
                .registry
                .invoke(
                    &call.name,
                    &call.arguments,
                    &self.context.for_call(&call.id),
                )
                .await
                .map_err(|error| match error {
                    crate::tool::Error::Cancelled => Error::Cancelled,
                    // Registration/context errors cannot arise during invocation;
                    // keep this boundary classified if the contract later grows.
                    _ => Error::Tool,
                })?;
            Ok(ToolResult {
                output: result.json,
                is_error: result.is_error,
            })
        })
    }
}

/// Successful turn additions, committed by the caller only if still current.
#[derive(Debug)]
pub struct Outcome {
    /// Semantic guidance committed with successful history, never orphaned tool results.
    pub instructions: crate::instructions::State,
    /// Complete conversation, including all correlated tool results.
    pub messages: Vec<Message>,
    /// Final completion classification.
    pub stop: Stop,
    /// Summed known usage; absent when no step supplied that counter.
    pub usage: Usage,
    /// Estimated occupancy and completeness of the summed known usage counters.
    pub context: crate::context::Report,
}

/// Run with the default sixteen model steps and sixty-four tool calls.
pub async fn run(
    model: &Model,
    messages: Vec<Message>,
    tools: &[Tool],
    executor: Option<&dyn Executor>,
    text: &mut (dyn FnMut(&str) + Send),
) -> Result<Outcome, Error> {
    run_with_limits(model, messages, tools, executor, text, Limits::default()).await
}

/// Run with explicit budgets; a last-step tool batch never executes.
pub async fn run_with_limits(
    model: &Model,
    messages: Vec<Message>,
    tools: &[Tool],
    executor: Option<&dyn Executor>,
    text: &mut (dyn FnMut(&str) + Send),
    limits: Limits,
) -> Result<Outcome, Error> {
    run_with_guidance(model, messages, tools, executor, text, limits, None).await
}

pub(crate) async fn run_with_guidance(
    model: &Model,
    messages: Vec<Message>,
    tools: &[Tool],
    executor: Option<&dyn Executor>,
    text: &mut (dyn FnMut(&str) + Send),
    limits: Limits,
    guidance: Option<crate::instructions::Turn>,
) -> Result<Outcome, Error> {
    run_observed(
        model, messages, tools, executor, text, limits, guidance, None,
    )
    .await
}

/// Bounded tool lifecycle publication, awaited before execution advances.
pub trait Observer: Send + Sync {
    /// Publish an accepted invocation, actual start, or completed result.
    fn observe<'a>(
        &'a self,
        call: &'a Call,
        observation: Observation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'a>>;
}

/// Implementation-independent tool lifecycle events.
pub enum Observation<'a> {
    /// The invocation was admitted but has not started execution.
    Accepted(crate::presentation::Description),
    /// Authorization is complete and execution is starting.
    Started,
    /// Execution or pre-execution refusal produced a model-facing result.
    Finished(&'a ToolResult),
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_observed(
    model: &Model,
    mut messages: Vec<Message>,
    tools: &[Tool],
    executor: Option<&dyn Executor>,
    text: &mut (dyn FnMut(&str) + Send),
    limits: Limits,
    mut guidance: Option<crate::instructions::Turn>,
    observer: Option<&dyn Observer>,
) -> Result<Outcome, Error> {
    let mut model = model.clone();
    let mut usage = Usage::default();
    // ACP adds exactly one user message per turn; no user means nothing is evictable.
    let mut active = messages
        .iter()
        .rposition(|message| matches!(message, Message::User(_)))
        .unwrap_or(0);
    let mut removed_turns = 0;
    let mut input_steps = 0;
    let mut output_steps = 0;
    let mut calls_seen = HashSet::new();
    let names: HashSet<_> = tools.iter().map(|tool| &tool.name).collect();
    if names.len() != tools.len() {
        return Err(Error::Tool);
    }
    for index in 0..limits.max_steps {
        if let Some(guidance) = &mut guidance {
            // Final/targetless operations can change an already-observed instruction source.
            guidance.refresh().await?;
            model = model.with_instructions(guidance.render()?);
        }
        removed_turns += crate::context::fit(&model, &mut messages, tools, &mut active)?;
        let step = model.step(&messages, tools, text).await?;
        input_steps += usize::from(step.usage.input.is_some());
        output_steps += usize::from(step.usage.output.is_some());
        usage.accumulate(&step.usage);
        let Message::Assistant { calls, .. } = &step.message else {
            return Err(Error::Malformed);
        };
        if step.stop != Stop::Tools {
            if !calls.is_empty() {
                return Err(Error::Malformed);
            }
            messages.push(step.message);
            let mut context = model.context(&messages, tools)?;
            context.removed_turns = removed_turns;
            context.steps = index + 1;
            context.input_steps = input_steps;
            context.output_steps = output_steps;
            return Ok(Outcome {
                instructions: guidance.map(|g| g.state).unwrap_or_default(),
                messages,
                stop: step.stop,
                usage,
                context,
            });
        }
        if calls.is_empty() {
            return Err(Error::Malformed);
        }
        // Never perform side effects whose results cannot reach another model step.
        if index == limits.max_steps - 1 {
            return Err(Error::Limit);
        }
        let executor = executor.ok_or(Error::Tool)?;
        // Validate the entire batch before side effects: a malformed second call
        // must not cause the first call to execute and then vanish from history.
        for call in calls {
            if !names.contains(&call.name) || !calls_seen.insert(call.id.clone()) {
                return Err(Error::Tool);
            }
            if calls_seen.len() > limits.max_tool_calls {
                return Err(Error::Limit);
            }
        }
        let calls = calls.clone();
        if let Some(observer) = observer {
            for call in &calls {
                observer
                    .observe(call, Observation::Accepted(executor.description(call)))
                    .await?;
            }
        }
        messages.push(step.message);
        // Inspect the whole batch before side effects: a later path can reveal
        // instructions that invalidate arguments generated for an earlier call.
        let mut defer = false;
        if let Some(guidance) = &mut guidance {
            defer |= guidance.refresh().await?;
            for call in &calls {
                for target in executor.targets(call) {
                    defer |= guidance.preflight(&target).await?;
                }
            }
        }
        for call in calls {
            let target = executor.target(&call);
            if let Some(guidance) = &mut guidance {
                // Earlier operations can rewrite AGENTS.md, including via bash.
                for target in executor.targets(&call) {
                    defer |= guidance.preflight(&target).await?;
                }
            }
            let result = if defer {
                deferred()
            } else if let Some((guidance, skill)) = guidance.as_mut().and_then(|guidance| {
                let skill = target.as_ref().and_then(|target| guidance.skill(target))?;
                Some((guidance, skill))
            }) {
                match skill.snapshot() {
                    Ok(body) => {
                        let result = executor
                            .execute_observed(
                                &call,
                                Some(std::sync::Arc::from(body.as_str())),
                                observer,
                            )
                            .await?;
                        if !result.is_error {
                            defer |= guidance.activate(&skill, body)?;
                        }
                        result
                    }
                    Err(error) => {
                        let mut output = serde_json::json!({"error":{"code":"skill_activation","message":error.to_string()}}).to_string();
                        // Source paths can exceed a tiny budget; preserve a complete envelope.
                        if output.len() > executor.output_limit() {
                            output = "{\"error\":{\"code\":\"skill_activation\",\"message\":\"Skill activation failed; check SKILL.md metadata, encoding and size.\"}}".into();
                        }
                        ToolResult {
                            output,
                            is_error: true,
                        }
                    }
                }
            } else {
                executor.execute_observed(&call, None, observer).await?
            };
            if result.output.len() > crate::tool::MAX_OUTPUT_BYTES {
                return Err(Error::Limit);
            }
            if let Some(observer) = observer {
                observer
                    .observe(&call, Observation::Finished(&result))
                    .await?;
            }
            messages.push(Message::Tool {
                id: call.id,
                output: result.output,
                is_error: result.is_error,
            });
            if let Some(guidance) = &mut guidance {
                // This barrier also covers a following Bash or custom call with no target.
                defer |= guidance.refresh().await?;
            }
        }
    }
    Err(Error::Limit)
}

fn deferred() -> ToolResult {
    ToolResult {
        output: "{\"error\":{\"code\":\"guidance_required\",\"message\":\"Not executed. New scoped guidance is in the next request. Reconsider this batch and reissue needed calls with fresh IDs.\"}}".into(),
        is_error: true,
    }
}
