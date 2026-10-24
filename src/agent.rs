//! A bounded inference loop with caller-owned tools, independent of ACP.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use crate::llm::{Call, Error, Message, Model, Stop, Tool, Usage};

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

/// Run at most sixteen model steps and sixty-four tool calls.
pub async fn run(
    model: &Model,
    mut messages: Vec<Message>,
    tools: &[Tool],
    executor: Option<&dyn Executor>,
    text: &mut (dyn FnMut(&str) + Send),
) -> Result<Outcome, Error> {
    let mut usage = Usage::default();
    let mut calls_seen = HashSet::new();
    let names: HashSet<_> = tools.iter().map(|tool| &tool.name).collect();
    if names.len() != tools.len() {
        return Err(Error::Tool);
    }
    for _ in 0..16 {
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
        let executor = executor.ok_or(Error::Tool)?;
        // Validate the entire batch before side effects: a malformed second call
        // must not cause the first call to execute and then vanish from history.
        for call in calls {
            if !names.contains(&call.name) || !calls_seen.insert(call.id.clone()) {
                return Err(Error::Tool);
            }
            if calls_seen.len() > 64 {
                return Err(Error::Limit);
            }
        }
        let calls = calls.clone();
        messages.push(step.message);
        for call in calls {
            let result = executor.execute(&call).await?;
            if result.output.len() > crate::http::MAX_EVENT {
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
