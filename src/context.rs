//! Approximate context occupancy and whole-turn eviction, independent of ACP.

use std::collections::HashSet;

use crate::llm::{Error, Message, Model, Tool};

/// Content-free accounting; estimates and actual provider counters stay separate.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Estimated retained input tokens, including tool definitions.
    pub estimated_input: u64,
    /// Explicit model capacity, absent when unknown.
    pub window: Option<u32>,
    /// Output capacity reserved for the next request.
    pub reserved_output: u64,
    /// Complete historical turns removed during this attempt.
    pub removed_turns: usize,
    /// Completed requests in this turn.
    pub steps: usize,
    /// Requests supplying actual input usage; fewer than steps means a partial total.
    pub input_steps: usize,
    /// Requests supplying actual output usage; fewer than steps means a partial total.
    pub output_steps: usize,
}

impl Report {
    /// Validate a request without pretending the estimate is a tokenizer bound.
    pub fn check(&self) -> Result<(), Error> {
        if self.window.is_some_and(|window| {
            self.estimated_input.saturating_add(self.reserved_output) > u64::from(window)
        }) {
            Err(Error::ContextLimit)
        } else {
            Ok(())
        }
    }
}

pub(crate) fn estimate(body: &serde_json::Value, window: Option<u32>) -> Report {
    // Estimate the representation actually sent, not both native and portable copies.
    let bytes = ["messages", "input", "tools"]
        .iter()
        .filter_map(|key| body.get(key))
        .fold(0u64, |total, value| {
            total.saturating_add(value.to_string().len() as u64)
        });
    Report {
        estimated_input: bytes.div_ceil(3).saturating_add(256),
        reserved_output: body
            .get("max_output_tokens")
            .or_else(|| body.get("max_completion_tokens"))
            .or_else(|| body.get("max_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        window,
        ..Report::default()
    }
}

/// Trim only complete historical turns, leaving the active turn indivisible.
pub(crate) fn fit(
    model: &Model,
    messages: &mut Vec<Message>,
    tools: &[Tool],
    active: &mut usize,
) -> Result<usize, Error> {
    let mut removed = 0;
    while model.context(messages, tools)?.check().is_err() {
        if *active == 0 {
            return Err(Error::ContextLimit);
        }
        let end = messages
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, message)| matches!(message, Message::User(_)).then_some(index))
            .ok_or(Error::Malformed)?;
        if end > *active {
            return Err(Error::ContextLimit);
        }
        validate_turn(&messages[..end])?;
        messages.drain(..end);
        *active -= end;
        removed += 1;
    }
    Ok(removed)
}

fn validate_turn(messages: &[Message]) -> Result<(), Error> {
    if !matches!(messages.first(), Some(Message::User(_))) {
        return Err(Error::Malformed);
    }
    let mut pending = HashSet::new();
    let mut final_answer = false;
    for message in &messages[1..] {
        match message {
            Message::Assistant { calls, .. } if pending.is_empty() && !final_answer => {
                final_answer = calls.is_empty();
                for call in calls {
                    if !pending.insert(call.id.as_str()) {
                        return Err(Error::Malformed);
                    }
                }
            }
            Message::Tool { id, .. } if pending.remove(id.as_str()) => {}
            _ => return Err(Error::Malformed),
        }
    }
    if final_answer && pending.is_empty() {
        Ok(())
    } else {
        Err(Error::Malformed)
    }
}
