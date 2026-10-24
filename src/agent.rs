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
}

impl Executor for NativeExecutor<'_> {
    fn execute<'a>(
        &'a self,
        call: &'a Call,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, Error>> + Send + 'a>> {
        Box::pin(async move {
            let result = self
                .registry
                .invoke(&call.name, &call.arguments, &self.context)
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
    /// Complete conversation, including all correlated tool results.
    pub messages: Vec<Message>,
    /// Final completion classification.
    pub stop: Stop,
    /// Summed known usage; absent when no step supplied that counter.
    pub usage: Usage,
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
    mut messages: Vec<Message>,
    tools: &[Tool],
    executor: Option<&dyn Executor>,
    text: &mut (dyn FnMut(&str) + Send),
    limits: Limits,
) -> Result<Outcome, Error> {
    let mut usage = Usage::default();
    let mut calls_seen = HashSet::new();
    let names: HashSet<_> = tools.iter().map(|tool| &tool.name).collect();
    if names.len() != tools.len() {
        return Err(Error::Tool);
    }
    for index in 0..limits.max_steps {
        let step = model.step(&messages, tools, text).await?;
        for (total, count) in [
            (&mut usage.input, step.usage.input),
            (&mut usage.output, step.usage.output),
        ] {
            if let Some(count) = count {
                *total = Some(total.unwrap_or(0).saturating_add(count));
            }
        }
        let Message::Assistant { calls, .. } = &step.message else {
            return Err(Error::Malformed);
        };
        if step.stop != Stop::Tools {
            if !calls.is_empty() {
                return Err(Error::Malformed);
            }
            messages.push(step.message);
            return Ok(Outcome {
                messages,
                stop: step.stop,
                usage,
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
        messages.push(step.message);
        for call in calls {
            let result = executor.execute(&call).await?;
            if result.output.len() > crate::tool::MAX_OUTPUT_BYTES {
                return Err(Error::Limit);
            }
            messages.push(Message::Tool {
                id: call.id,
                output: result.output,
                is_error: result.is_error,
            });
        }
    }
    Err(Error::Limit)
}
