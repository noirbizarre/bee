//! Turn-owned model operations and the sole plan-to-ACP conversion seam.

use super::{Reply, lock, send_update, v2};
use crate::agent::ToolResult;
use crate::llm::{Call, Error, Tool};
use crate::plan::{Entries, Priority, Status};
use serde_json::json;

pub(super) fn definition() -> Tool {
    Tool {
        name: "update_plan".into(),
        description: "Maintain an execution plan for substantive multi-step work. Omit plans for simple answers. Each call replaces the entire ordered list for this turn; keep descriptions and statuses current, and explicitly mark completed work. Empty entries clears the plan. Unfinished work is cancelled when the turn ends.".into(),
        parameters: json!({"type":"object","properties":{"entries":{"type":"array","maxItems":crate::plan::MAX_ENTRIES,"items":{"type":"object","properties":{"content":{"type":"string","minLength":1,"maxLength":crate::plan::MAX_CONTENT_BYTES},"priority":{"type":"string","enum":["high","medium","low"]},"status":{"type":"string","enum":["pending","in_progress","completed","cancelled"]}},"required":["content","priority","status"],"additionalProperties":false}}},"required":["entries"],"additionalProperties":false}),
    }
}

pub(super) fn update(turn: u64, plan: Entries) -> v2::SessionUpdate {
    let entries = plan
        .entries
        .into_iter()
        .map(|entry| {
            v2::PlanEntry::new(
                entry.content,
                match entry.priority {
                    Priority::High => v2::PlanEntryPriority::High,
                    Priority::Medium => v2::PlanEntryPriority::Medium,
                    Priority::Low => v2::PlanEntryPriority::Low,
                },
                match entry.status {
                    Status::Pending => v2::PlanEntryStatus::Pending,
                    Status::InProgress => v2::PlanEntryStatus::InProgress,
                    Status::Completed => v2::PlanEntryStatus::Completed,
                    Status::Cancelled => v2::PlanEntryStatus::Cancelled,
                },
            )
        })
        .collect();
    v2::SessionUpdate::PlanUpdate(v2::PlanUpdate::new(v2::PlanUpdateContent::items(
        crate::plan::id(turn),
        entries,
    )))
}

impl Reply {
    pub(super) async fn update_plan(&self, call: &Call) -> Result<ToolResult, Error> {
        let plan = serde_json::from_value::<Entries>(call.arguments.clone())
            .ok()
            .filter(Entries::valid);
        let Some(plan) = plan else {
            return Ok(ToolResult {
                output: "{\"error\":{\"code\":\"invalid_arguments\",\"message\":\"Supply entries with nonblank content (at most 4096 UTF-8 bytes), priority and status; at most 128 entries and 64 KiB total, with no extra fields.\"}}".into(),
                is_error: true,
            });
        };
        let receive = {
            let mut sessions = lock(&self.sessions);
            if !sessions.accepts_output(&self.id, self.token) {
                return Err(Error::Cancelled);
            }
            let connection = self.connection.clone();
            let session_id = self.session_id.clone();
            let state = self.plan.clone();
            let token = self.token;
            let emitted = plan.clone();
            let emit = move || {
                let mut current = lock(&state);
                if current.as_ref() != Some(&emitted) {
                    *current = Some(emitted.clone());
                    let _ = send_update(&connection, &session_id, update(token, emitted));
                }
            };
            if let Some(history) = sessions.history(&self.id) {
                // Enqueue while holding cancellation's gate; await without it.
                Some(history.plan(token, plan, emit))
            } else {
                sessions.record_plan(&self.id, token, plan);
                emit();
                None
            }
        };
        if let Some(receive) = receive {
            receive
                .await
                .map_err(|_| Error::Tool)?
                .map_err(|_| Error::Tool)?;
        }
        Ok(ToolResult {
            output: "{\"updated\":true}".into(),
            is_error: false,
        })
    }
}
