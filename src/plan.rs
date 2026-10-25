//! SDK-free full execution-plan snapshots. Identity belongs to the turn, not the model.

use serde::{Deserialize, Serialize};

pub(crate) const MAX_ENTRIES: usize = 128;
pub(crate) const MAX_CONTENT_BYTES: usize = 4096;
pub(crate) const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entries {
    pub(crate) entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub(crate) content: String,
    pub(crate) priority: Priority,
    pub(crate) status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Priority {
    High,
    Medium,
    Low,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl Entries {
    pub(crate) fn valid(&self) -> bool {
        self.entries.len() <= MAX_ENTRIES
            && self.entries.iter().all(|entry| {
                !entry.content.trim().is_empty() && entry.content.len() <= MAX_CONTENT_BYTES
            })
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_BYTES)
    }

    pub(crate) fn cancel_unfinished(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if matches!(entry.status, Status::Pending | Status::InProgress) {
                // End-of-turn is not evidence that the described work succeeded.
                entry.status = Status::Cancelled;
                changed = true;
            }
        }
        changed
    }
}

pub(crate) fn id(turn: u64) -> String {
    format!("plan-{turn}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_bounds_limit_utf8_bytes_entry_count_and_total_encoded_size() {
        let entry = Entry {
            content: "x".into(),
            priority: Priority::Medium,
            status: Status::Pending,
        };
        let mut plan = Entries {
            entries: vec![entry.clone(); MAX_ENTRIES],
        };
        assert!(plan.valid());
        plan.entries.push(entry);
        assert!(!plan.valid());
        plan.entries.truncate(1);
        plan.entries[0].content = "é".repeat(MAX_CONTENT_BYTES / 2);
        assert!(plan.valid());
        plan.entries[0].content.push('é');
        assert!(!plan.valid());
        plan.entries[0].content = "x".repeat(MAX_CONTENT_BYTES);
        plan.entries = vec![plan.entries[0].clone(); MAX_ENTRIES];
        assert!(!plan.valid());
        assert!(Entries { entries: vec![] }.valid());
    }
}
