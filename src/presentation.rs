//! Bounded execution observations, independent of provider context and ACP SDK types.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub(crate) mod patch;

pub(crate) const TERMINAL_BYTES: usize = 256 * 1024;
pub(crate) const PATCH_BYTES: usize = 256 * 1024;
pub(crate) const CALL_BYTES: usize = 1024 * 1024;
pub(crate) const TURN_BYTES: usize = 8 * 1024 * 1024;
const SESSION_BYTES: usize = 32 * 1024 * 1024;

/// An implementation-owned description; names alone never establish file effects.
#[derive(Clone)]
pub struct Description {
    /// Display name, which may differ from a provider-safe MCP alias.
    pub name: String,
    /// Standard tool kind, defaulting to `other` for custom operations.
    pub kind: &'static str,
    /// Absolute filesystem locations known before execution.
    pub locations: Vec<PathBuf>,
}

/// Facts observed by a native executor, never inferred from shell output.
#[derive(Clone, Serialize, Deserialize)]
pub enum Artifact {
    /// Execution detail such as an actual staging-cleanup failure.
    Note(String),
    /// Reserve retained receipt space before irreversible filesystem publication.
    Reserve(usize),
    /// Filesystem publication has already succeeded.
    Changes(Vec<Change>, Option<String>),
    /// A locally owned process was successfully spawned.
    TerminalStarted {
        /// Command supplied to the locally owned shell.
        command: String,
        /// Absolute execution directory.
        cwd: PathBuf,
    },
    /// Raw pipe bytes in observed read order, before any lossy model conversion.
    TerminalOutput(Vec<u8>),
    /// An observed exit; absent details mean exited with unknown status.
    TerminalExited {
        /// Observed process exit code, when available.
        code: Option<u32>,
        /// Observed platform signal name, when available.
        signal: Option<String>,
    },
    /// Bytes drained after the terminal prefix reached its retention bound.
    TerminalOmitted(u64),
}

/// One committed file or directory operation.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    /// Standard add/delete/modify/move/copy operation.
    pub operation: String,
    /// Absolute affected destination (deleted path for delete).
    pub path: PathBuf,
    /// Absolute source for move/copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<PathBuf>,
    /// Text, binary, or directory, determined by the implementation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Event {
    Accepted {
        id: String,
        name: String,
        kind: String,
        locations: Vec<PathBuf>,
        input: Value,
    },
    Started {
        id: String,
    },
    Finished {
        id: String,
        output: Value,
        text: String,
        failed: bool,
    },
    Artifact {
        id: String,
        artifact: Artifact,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Tool {
    pub id: String,
    name: String,
    kind: String,
    locations: Vec<PathBuf>,
    input: Value,
    output: Value,
    status: String,
    content: Vec<Value>,
    reserved: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct Terminal {
    id: String,
    command: String,
    cwd: PathBuf,
    bytes: Vec<u8>,
    omitted: u64,
    exit: Option<Value>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Turn {
    tools: BTreeMap<String, Tool>,
    order: Vec<String>,
    terminals: BTreeMap<String, Terminal>,
    pub finished: bool,
}

/// Retained snapshots, not an executable log or a model-context projection.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    turns: BTreeMap<u64, Turn>,
    sizes: BTreeMap<u64, usize>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Presentation([redacted])")
    }
}

pub(crate) fn bounded(text: &str, limit: usize) -> String {
    if serde_json::to_string(text).is_ok_and(|encoded| encoded.len() <= limit) {
        return text.into();
    }
    // A JSON control character can expand sixfold; bound encoded presentation too.
    let mut end = (limit.saturating_sub(64) / 6).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[Presentation truncated; full result remains model-owned.]",
        &text[..end]
    )
}

pub(crate) fn bounded_value(value: &Value) -> Value {
    let text = value.to_string();
    if text.len() <= CALL_BYTES / 4 {
        value.clone()
    } else {
        json!({"truncated":true,"preview":bounded(&text, CALL_BYTES / 4)})
    }
}

fn encoded(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl Tool {
    fn wire(&self) -> Value {
        json!({"sessionUpdate":"tool_call_update","toolCallId":self.id,
            "name":self.name,"title":self.name,"kind":self.kind,"status":self.status,
            "rawInput":self.input,"rawOutput":self.output,"content":self.content,
            "locations":self.locations.iter().map(|path| json!({"path":path})).collect::<Vec<_>>()})
    }
}

impl Terminal {
    fn wire(&self) -> Value {
        json!({"sessionUpdate":"terminal_update","terminalId":self.id,"command":self.command,
            "cwd":self.cwd,"output":{"data":encoded(&self.bytes)},"exitStatus":self.exit,
            "_meta":{"bee.dev/output":{"omittedBytes":self.omitted}}})
    }
}

impl State {
    pub fn snapshots(&self) -> Vec<Value> {
        self.turns
            .values()
            .flat_map(|turn| {
                // Definitions precede references; full replacements make repeated replay idempotent.
                turn.order
                    .iter()
                    .flat_map(|id| {
                        turn.terminals
                            .get(&format!("terminal-{id}"))
                            .map(Terminal::wire)
                            .into_iter()
                            .chain(turn.tools.get(id).map(Tool::wire))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn apply(&mut self, number: u64, event: &Event) -> Result<Vec<Value>, ()> {
        let mut next = self.turns.get(&number).cloned().unwrap_or_default();
        if next.finished {
            return Err(());
        }
        let updates = next.apply(event)?;
        let reserved = next
            .tools
            .values()
            .fold(0usize, |sum, t| sum.saturating_add(t.reserved));
        // Leave a fixed cleanup margin for terminal statuses after ordinary output is revoked.
        let bytes = serde_json::to_vec(&next).map_err(|_| ())?.len();
        if bytes.saturating_add(reserved) > TURN_BYTES - 64 * 1024 {
            return Err(());
        }
        self.sizes.insert(number, bytes);
        self.turns.insert(number, next);
        self.retain();
        Ok(updates)
    }

    fn retain(&mut self) {
        // Whole-turn eviction never strands a terminal reference or evicts active execution.
        while self.turns.values().filter(|t| t.finished).count() > 128
            || self.sizes.values().sum::<usize>() > SESSION_BYTES
        {
            let Some(oldest) = self.turns.iter().find(|(_, t)| t.finished).map(|(n, _)| *n) else {
                break;
            };
            self.turns.remove(&oldest);
            self.sizes.remove(&oldest);
        }
    }

    pub fn finish(&mut self, number: u64, cancelled: bool) -> Vec<Value> {
        let Some(turn) = self.turns.get_mut(&number) else {
            return Vec::new();
        };
        if turn.finished {
            return Vec::new();
        }
        let mut updates = Vec::new();
        for terminal in turn.terminals.values_mut() {
            if terminal.exit.is_none() {
                terminal.exit = Some(json!({}));
                updates.push(terminal.wire());
            }
        }
        for tool in turn.tools.values_mut() {
            tool.reserved = 0;
            if matches!(tool.status.as_str(), "pending" | "in_progress") {
                tool.status = if cancelled { "cancelled" } else { "failed" }.into();
                tool.content.push(json!({"type":"content","content":{"type":"text","text":"Execution interrupted; already committed changes were not rolled back."}}));
                updates.push(tool.wire());
            }
        }
        turn.finished = true;
        self.sizes.insert(
            number,
            serde_json::to_vec(turn).map_or(TURN_BYTES, |v| v.len()),
        );
        self.retain();
        updates
    }
}

impl Turn {
    fn apply(&mut self, event: &Event) -> Result<Vec<Value>, ()> {
        let id = match event {
            Event::Accepted { id, .. }
            | Event::Started { id }
            | Event::Finished { id, .. }
            | Event::Artifact { id, .. } => id,
        };
        if let Event::Accepted {
            name,
            kind,
            locations,
            input,
            ..
        } = event
        {
            if self.tools.contains_key(id)
                || self.tools.len() >= 1024
                || name.is_empty()
                || locations.iter().any(|p| !p.is_absolute())
            {
                return Err(());
            }
            let tool = Tool {
                id: id.clone(),
                name: name.clone(),
                kind: kind.clone(),
                locations: locations.clone(),
                input: input.clone(),
                output: Value::Null,
                status: "pending".into(),
                content: Vec::new(),
                reserved: 0,
            };
            let update = tool.wire();
            self.tools.insert(id.clone(), tool);
            self.order.push(id.clone());
            return Ok(vec![update]);
        }
        let tool = self.tools.get_mut(id).ok_or(())?;
        if !matches!(tool.status.as_str(), "pending" | "in_progress") {
            return Err(());
        }
        let terminal_id = format!("terminal-{id}");
        match event {
            Event::Started { .. } => {
                tool.status = "in_progress".into();
                Ok(vec![
                    json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":"in_progress"}),
                ])
            }
            Event::Finished {
                output,
                text,
                failed,
                ..
            } => {
                tool.reserved = 0;
                tool.status = if *failed { "failed" } else { "completed" }.into();
                tool.output = output.clone();
                let content = json!({"type":"content","content":{"type":"text","text":text}});
                tool.content.push(content.clone());
                Ok(vec![
                    json!({"sessionUpdate":"tool_call_content_chunk","toolCallId":id,"content":content}),
                    json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":tool.status,"rawOutput":output}),
                ])
            }
            Event::Artifact { artifact, .. } => match artifact {
                Artifact::Note(text) => {
                    let content = json!({"type":"content","content":{"type":"text","text":bounded(text,CALL_BYTES/4)}});
                    tool.content.push(content.clone());
                    Ok(vec![
                        json!({"sessionUpdate":"tool_call_content_chunk","toolCallId":id,"content":content}),
                    ])
                }
                Artifact::Reserve(bytes) => {
                    tool.reserved = tool.reserved.saturating_add(*bytes);
                    Ok(Vec::new())
                }
                Artifact::Changes(changes, patch) => {
                    let mut content = json!({"type":"diff","changes":changes});
                    if let Some(text) = patch {
                        content["patch"] = json!({"format":"git_patch","text":text});
                    }
                    tool.reserved = tool.reserved.saturating_sub(content.to_string().len());
                    tool.content.push(content.clone());
                    Ok(vec![
                        json!({"sessionUpdate":"tool_call_content_chunk","toolCallId":id,"content":content}),
                    ])
                }
                Artifact::TerminalStarted { command, cwd } => {
                    if self.terminals.contains_key(&terminal_id) {
                        return Err(());
                    }
                    let terminal = Terminal {
                        id: terminal_id.clone(),
                        command: command.clone(),
                        cwd: cwd.clone(),
                        bytes: Vec::new(),
                        omitted: 0,
                        exit: None,
                    };
                    let update = terminal.wire();
                    self.terminals.insert(terminal_id.clone(), terminal);
                    let content = json!({"type":"terminal","terminalId":terminal_id});
                    tool.content.push(content.clone());
                    Ok(vec![
                        update,
                        json!({"sessionUpdate":"tool_call_content_chunk","toolCallId":id,"content":content}),
                    ])
                }
                Artifact::TerminalOutput(bytes) => {
                    let terminal = self.terminals.get_mut(&terminal_id).ok_or(())?;
                    let keep = (TERMINAL_BYTES - terminal.bytes.len()).min(bytes.len());
                    terminal.bytes.extend_from_slice(&bytes[..keep]);
                    terminal.omitted = terminal.omitted.saturating_add((bytes.len() - keep) as u64);
                    if keep == 0 {
                        return Ok(Vec::new());
                    }
                    Ok(vec![
                        json!({"sessionUpdate":"terminal_output_chunk","terminalId":terminal_id,"data":encoded(&bytes[..keep])}),
                    ])
                }
                Artifact::TerminalExited { code, signal } => {
                    let terminal = self.terminals.get_mut(&terminal_id).ok_or(())?;
                    terminal.exit = Some(json!({"exitCode":code,"signal":signal}));
                    Ok(vec![terminal.wire()])
                }
                Artifact::TerminalOmitted(bytes) => {
                    let terminal = self.terminals.get_mut(&terminal_id).ok_or(())?;
                    terminal.omitted = terminal.omitted.saturating_add(*bytes);
                    Ok(Vec::new())
                }
            },
            Event::Accepted { .. } => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(id: &str) -> Event {
        Event::Accepted {
            id: id.into(),
            name: "tool".into(),
            kind: "other".into(),
            locations: Vec::new(),
            input: json!({}),
        }
    }

    #[test]
    fn interrupted_calls_close_once_without_erasing_committed_changes_or_completed_calls() {
        let mut state = State::default();
        state.apply(1, &accepted("first")).unwrap();
        state
            .apply(1, &Event::Started { id: "first".into() })
            .unwrap();
        state
            .apply(
                1,
                &Event::Artifact {
                    id: "first".into(),
                    artifact: Artifact::Changes(
                        vec![Change {
                            operation: "delete".into(),
                            path: "/root/deleted".into(),
                            old_path: None,
                            file_type: None,
                        }],
                        None,
                    ),
                },
            )
            .unwrap();
        state
            .apply(
                1,
                &Event::Finished {
                    id: "first".into(),
                    output: json!({}),
                    text: "done".into(),
                    failed: false,
                },
            )
            .unwrap();
        state.apply(1, &accepted("pending")).unwrap();
        let cleanup = state.finish(1, true);
        assert_eq!(cleanup.len(), 1);
        assert_eq!(cleanup[0]["status"], "cancelled");
        let snapshots = state.snapshots();
        assert_eq!(snapshots[0]["status"], "completed");
        assert_eq!(
            snapshots[0]["content"][0]["changes"][0]["operation"],
            "delete"
        );
        assert!(state.finish(1, true).is_empty());
        assert!(state.apply(1, &accepted("late")).is_err());
    }

    #[test]
    fn terminal_prefix_bounds_preserve_raw_bytes_and_replay_uses_authoritative_replacements() {
        use base64::Engine;
        let mut state = State::default();
        state.apply(1, &accepted("shell")).unwrap();
        state
            .apply(
                1,
                &Event::Artifact {
                    id: "shell".into(),
                    artifact: Artifact::TerminalStarted {
                        command: "command".into(),
                        cwd: "/root".into(),
                    },
                },
            )
            .unwrap();
        let bytes = [
            vec![255, 0, 0xe2],
            vec![0x82, 0xac],
            vec![b'x'; TERMINAL_BYTES],
        ]
        .concat();
        for chunk in bytes.chunks(8192) {
            state
                .apply(
                    1,
                    &Event::Artifact {
                        id: "shell".into(),
                        artifact: Artifact::TerminalOutput(chunk.to_vec()),
                    },
                )
                .unwrap();
        }
        state.finish(1, true);
        let snapshots = state.snapshots();
        assert_eq!(snapshots[0]["sessionUpdate"], "terminal_update");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(snapshots[0]["output"]["data"].as_str().unwrap())
                .unwrap(),
            bytes[..TERMINAL_BYTES]
        );
        assert_eq!(snapshots[0]["_meta"]["bee.dev/output"]["omittedBytes"], 5);
        assert_eq!(snapshots[0]["exitStatus"], json!({}));
        assert_eq!(
            snapshots[1]["content"][0]["terminalId"],
            snapshots[0]["terminalId"]
        );
    }

    #[test]
    fn reservation_fails_before_mutation_and_retention_evicts_whole_completed_turns() {
        let mut state = State::default();
        state.apply(1, &accepted("too-large")).unwrap();
        assert!(
            state
                .apply(
                    1,
                    &Event::Artifact {
                        id: "too-large".into(),
                        artifact: Artifact::Reserve(TURN_BYTES)
                    }
                )
                .is_err()
        );
        state.finish(1, false);
        for turn in 2..=131 {
            state
                .apply(turn, &accepted(&format!("tool-{turn}")))
                .unwrap();
            state.finish(turn, false);
        }
        assert_eq!(state.turns.len(), 128);
        assert_eq!(*state.turns.keys().next().unwrap(), 4);
        assert!(
            state
                .snapshots()
                .iter()
                .all(|u| u["toolCallId"] != "too-large")
        );
    }
}
