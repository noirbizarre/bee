//! The authoritative append-only JSONL log. No index or snapshot sidecar is
//! necessary to recover a session; future caches can be rebuilt from this file.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    Message, MessageKind, Result, Snapshot, ThoughtId, error, open_options, open_private_file,
};

// A corrupt/malicious line must not allocate the size of the entire journal.
// This bounds one record, not transcript length or the number of sessions.
pub(super) const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const VERSION: u32 = 6;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Event {
    SessionCreated {
        id: String,
        created: u64,
        snapshot: Snapshot,
    },
    SettingsChanged {
        provider: Option<String>,
        model: Option<String>,
        #[serde(default)]
        selection_state: crate::selection::State,
        portable: bool,
    },
    InitialModelSelected {
        turn: u64,
        provider: String,
        model: String,
    },
    TurnAccepted {
        turn: u64,
        content: serde_json::Value,
    },
    AssistantDelta {
        turn: u64,
        text: String,
    },
    ThoughtDelta {
        turn: u64,
        id: ThoughtId,
        text: String,
    },
    PlanUpdated {
        turn: u64,
        plan: crate::plan::Entries,
    },
    Presentation {
        turn: u64,
        event: crate::presentation::Event,
    },
    TurnFinished {
        turn: u64,
        outcome: Outcome,
        context: Option<Context>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Outcome {
    Completed,
    Cancelled,
    Error,
    Refusal,
    Interrupted,
}

/// One successful turn's changes, not another copy of its retained predecessors.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Context {
    drop_messages: usize,
    portable: bool,
    append: Vec<crate::llm::Message>,
    protocol: Option<crate::provider::Protocol>,
    instructions: crate::instructions::Changes,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    seq: u64,
    checksum: String,
    event: Box<serde_json::value::RawValue>,
}

fn checksum(version: u32, seq: u64, event: &str) -> String {
    // Hash the exact payload bytes, not a reserialization whose key ordering or
    // float formatting might change after upgrading serde_json. Include framing
    // identity to detect damaged but syntactically valid records as well.
    let mut digest = Sha256::new();
    digest.update(b"bee-session-jsonl\0");
    digest.update(version.to_le_bytes());
    digest.update(seq.to_le_bytes());
    digest.update(event.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn encode(seq: u64, event: &Event, path: &Path) -> Result<Vec<u8>> {
    let event = serde_json::value::to_raw_value(event)
        .map_err(|_| error(path, "cannot encode journal event"))?;
    let record = Record {
        version: VERSION,
        seq,
        checksum: checksum(VERSION, seq, event.get()),
        event,
    };
    let mut bytes =
        serde_json::to_vec(&record).map_err(|_| error(path, "cannot encode journal record"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(error(
            path,
            "journal record exceeds the 64 MiB limit; reduce prompt or turn size",
        ));
    }
    // Serialization can accept deeper JSON than deserialization. Validate with
    // the exact recovery decoder before making any authoritative bytes visible.
    // Provider arguments near their standalone depth limit gain nesting here.
    decode(&bytes, seq, path).map_err(|_| {
        error(
            path,
            "journal event exceeds supported recovery nesting; reduce structured input depth",
        )
    })?;
    Ok(bytes)
}

fn decode(line: &[u8], seq: u64, path: &Path) -> Result<Event> {
    let record: Record =
        serde_json::from_slice(line).map_err(|_| error(path, "invalid complete journal record"))?;
    // Existing journals remain readable; older binaries reject new records by
    // version instead of interpreting an extended event contract as v1.
    if !matches!(record.version, 1 | 2 | 3 | 4 | 5 | VERSION) {
        return Err(error(
            path,
            "unsupported session history version; use a compatible Bee version",
        ));
    }
    if record.seq != seq
        || record.checksum != checksum(record.version, record.seq, record.event.get())
    {
        return Err(error(path, "journal sequence or checksum is invalid"));
    }
    let event = serde_json::from_str(record.event.get())
        .map_err(|_| error(path, "invalid journal event"))?;
    if let Event::ThoughtDelta { id, .. } = &event
        && (record.version < 6 || !id.valid())
    {
        return Err(error(
            path,
            "agent thoughts require valid session history version 6 coordinates",
        ));
    }
    let selection = match &event {
        Event::SessionCreated { snapshot, .. } => Some((
            &snapshot.selection_state,
            snapshot.provider.as_deref(),
            snapshot.model.as_deref(),
        )),
        Event::SettingsChanged {
            provider,
            model,
            selection_state,
            ..
        } => Some((selection_state, provider.as_deref(), model.as_deref())),
        _ => None,
    };
    if let Some((state, provider, model)) = selection {
        let raw: serde_json::Value = serde_json::from_str(record.event.get())
            .map_err(|_| error(path, "invalid journal event"))?;
        let fields = if matches!(event, Event::SessionCreated { .. }) {
            &raw["snapshot"]
        } else {
            &raw
        };
        // A safety guard cannot be retrofitted into an old version older binaries would ignore.
        if !state.valid(provider, model)
            || (record.version < 4 && *state != crate::selection::State::Explicit)
            || (record.version >= 4 && fields.get("selection_state").is_none())
        {
            return Err(error(path, "invalid session model selection state"));
        }
    }
    if let Event::SessionCreated { snapshot, .. } = &event {
        // A v3 header must bind ownership explicitly. Old binaries reject its
        // version instead of silently discarding the new isolation boundary.
        let raw: serde_json::Value = serde_json::from_str(record.event.get())
            .map_err(|_| error(path, "invalid journal event"))?;
        if (record.version >= 3 && raw["snapshot"].get("profile").is_none())
            || (record.version >= 4 && raw["snapshot"].get("selection_state").is_none())
            || (record.version < 3 && snapshot.profile != "default")
            || snapshot.profile.trim().is_empty()
        {
            return Err(error(path, "invalid session profile ownership"));
        }
    }
    if record.version < 5 && matches!(event, Event::Presentation { .. }) {
        return Err(error(
            path,
            "tool presentation requires session history version 5",
        ));
    }
    if record.version == 1 && matches!(event, Event::PlanUpdated { .. }) {
        return Err(error(
            path,
            "execution plans require session history version 2",
        ));
    }
    Ok(event)
}

/// A reader accepts only complete, checksummed, sequential records. A final
/// incomplete line is left for the exclusive owner to repair, never skipped in
/// the middle of a journal and never truncated by concurrent metadata readers.
pub(super) struct Reader {
    input: BufReader<std::io::Take<File>>,
    path: PathBuf,
    pub(super) offset: u64,
    pub(super) seq: u64,
}

impl Reader {
    pub(super) fn open(path: &Path, limit: u64) -> Result<Self> {
        let file = open_private_file(path, false, false)?;
        Ok(Self {
            input: BufReader::new(file.take(limit)),
            path: path.to_owned(),
            offset: 0,
            seq: 0,
        })
    }

    pub(super) fn next(&mut self) -> Result<Option<Event>> {
        let mut line = Vec::new();
        self.input
            .by_ref()
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|_| error(&self.path, "cannot read session journal"))?;
        if line.len() > MAX_RECORD_BYTES {
            return Err(error(&self.path, "journal record exceeds the 64 MiB limit"));
        }
        if line.is_empty() || !line.ends_with(b"\n") {
            return Ok(None);
        }
        let event = decode(&line, self.seq, &self.path)?;
        self.seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| error(&self.path, "journal sequence exhausted"))?;
        self.offset += line.len() as u64;
        Ok(Some(event))
    }
}

#[derive(Clone)]
pub(super) struct Projection {
    pub(super) snapshot: Snapshot,
    pub(super) active: Option<u64>,
    pub(super) transcript_bytes: usize,
    pub(super) created: u64,
    pub(super) plans: std::collections::BTreeMap<u64, crate::plan::Entries>,
    pub(super) presentation: crate::presentation::State,
}

fn portable(messages: &mut [crate::llm::Message]) {
    for message in messages {
        if let crate::llm::Message::Assistant { native, .. } = message {
            *native = None;
        }
    }
}

impl Projection {
    pub(super) fn header(event: Event, id: &str, path: &Path) -> Result<Self> {
        let Event::SessionCreated {
            id: stored,
            created,
            snapshot,
        } = event
        else {
            return Err(error(path, "session journal header is missing"));
        };
        if stored != id || snapshot.turns != 0 || !snapshot.cwd.is_absolute() {
            return Err(error(
                path,
                "session journal identity or initial metadata is invalid",
            ));
        }
        Ok(Self {
            snapshot,
            active: None,
            transcript_bytes: 0,
            plans: Default::default(),
            presentation: Default::default(),
            created,
        })
    }

    fn apply(&mut self, event: &Event, path: &Path) -> Result<()> {
        let invalid = || error(path, "journal event violates session ordering");
        match event {
            Event::SessionCreated { .. } => return Err(invalid()),
            Event::SettingsChanged {
                provider,
                model,
                selection_state,
                portable: clear_native,
            } => {
                if self.active.is_some() {
                    return Err(invalid());
                }
                self.snapshot.provider.clone_from(provider);
                self.snapshot.model.clone_from(model);
                self.snapshot.selection_state.clone_from(selection_state);
                if *clear_native {
                    portable(&mut self.snapshot.messages);
                }
            }
            Event::InitialModelSelected {
                turn,
                provider,
                model,
            } => {
                // First-use auth can discover a model after acceptance, but may never
                // replace a selected account/model or rewrite committed context.
                if self.active != Some(*turn)
                    || self.snapshot.model.is_some()
                    || self
                        .snapshot
                        .provider
                        .as_ref()
                        .is_some_and(|old| old != provider)
                    || crate::provider::check_model_id(model).is_err()
                {
                    return Err(invalid());
                }
                self.snapshot.provider = Some(provider.clone());
                self.snapshot.model = Some(model.clone());
            }
            Event::TurnAccepted { turn, content } => {
                if matches!(
                    self.snapshot.selection_state,
                    crate::selection::State::Blocked(_)
                ) || self.active.is_some()
                    || *turn <= self.snapshot.turns
                    || !content.is_array()
                {
                    return Err(invalid());
                }
                self.snapshot.turns = *turn;
                self.active = Some(*turn);
                let bytes = serde_json::to_vec(content).map_err(|_| invalid())?.len();
                self.transcript_bytes = self
                    .transcript_bytes
                    .checked_add(bytes)
                    .ok_or_else(invalid)?;
            }
            Event::AssistantDelta { turn, text } | Event::ThoughtDelta { turn, text, .. } => {
                if self.active != Some(*turn) {
                    return Err(invalid());
                }
                self.transcript_bytes = self
                    .transcript_bytes
                    .checked_add(text.len())
                    .ok_or_else(invalid)?;
            }
            Event::Presentation { turn, event } => {
                if self.active != Some(*turn) {
                    return Err(invalid());
                }
                self.presentation
                    .apply(*turn, event)
                    .map_err(|_| invalid())?;
            }
            Event::PlanUpdated { turn, plan } => {
                if self.active != Some(*turn) || !plan.valid() {
                    return Err(invalid());
                }
                self.plans.insert(*turn, plan.clone());
            }
            Event::TurnFinished {
                turn,
                outcome,
                context,
            } => {
                if self.active != Some(*turn)
                    || (context.is_some() && *outcome != Outcome::Completed)
                {
                    return Err(invalid());
                }
                if let Some(context) = context {
                    let old = &mut self.snapshot.messages;
                    if context.drop_messages > old.len()
                        || (context.drop_messages < old.len()
                            && !matches!(old[context.drop_messages], crate::llm::Message::User(_)))
                        || !matches!(context.append.first(), Some(crate::llm::Message::User(_)))
                        || context
                            .append
                            .iter()
                            .skip(1)
                            .any(|message| matches!(message, crate::llm::Message::User(_)))
                    {
                        return Err(invalid());
                    }
                    if context.portable {
                        portable(old);
                    }
                    old.drain(..context.drop_messages);
                    old.extend(context.append.iter().cloned());
                    self.snapshot
                        .instructions
                        .apply(context.instructions.clone());
                    self.snapshot.protocol = context.protocol;
                }
                if let Some(plan) = self.plans.get_mut(turn) {
                    // The terminal record atomically closes context and plan state,
                    // including interrupted recovery; no second append can be lost.
                    plan.cancel_unfinished();
                }
                self.active = None;
                self.presentation
                    .finish(*turn, *outcome == Outcome::Cancelled);
            }
        }
        Ok(())
    }

    pub(super) fn settings(&self, next: &Snapshot, path: &Path) -> Result<Event> {
        if self.snapshot.cwd != next.cwd
            || self.snapshot.profile != next.profile
            || self.snapshot.turns != next.turns
            || self.snapshot.protocol != next.protocol
            || self.snapshot.instructions != next.instructions
        {
            return Err(error(path, "settings mutation changed committed context"));
        }
        let clear_native = self.snapshot.messages != next.messages;
        if clear_native {
            let mut expected = self.snapshot.messages.clone();
            portable(&mut expected);
            if expected != next.messages {
                return Err(error(path, "settings mutation changed committed messages"));
            }
        }
        Ok(Event::SettingsChanged {
            provider: next.provider.clone(),
            model: next.model.clone(),
            selection_state: next.selection_state.clone(),
            portable: clear_native,
        })
    }

    pub(super) fn context(&self, next: &Snapshot, path: &Path) -> Result<Context> {
        let invalid = || error(path, "successful context is not an oldest-whole-turn delta");
        if next.cwd != self.snapshot.cwd
            || next.profile != self.snapshot.profile
            || next.turns != self.snapshot.turns
            || next.provider != self.snapshot.provider
            || next.model != self.snapshot.model
            || next.selection_state != self.snapshot.selection_state
        {
            return Err(invalid());
        }
        let start = next
            .messages
            .iter()
            .rposition(|message| matches!(message, crate::llm::Message::User(_)))
            .ok_or_else(invalid)?;
        let drop_messages = self
            .snapshot
            .messages
            .len()
            .checked_sub(start)
            .ok_or_else(invalid)?;
        let retained = &self.snapshot.messages[drop_messages..];
        let clear_native = retained != &next.messages[..start];
        if clear_native {
            let mut expected = retained.to_vec();
            portable(&mut expected);
            if expected != next.messages[..start] {
                return Err(invalid());
            }
        }
        Ok(Context {
            drop_messages,
            portable: clear_native,
            append: next.messages[start..].to_vec(),
            protocol: next.protocol,
            instructions: next.instructions.changes_from(&self.snapshot.instructions),
        })
    }
}

#[cfg(test)]
pub(super) enum Fault {
    BeforeWrite,
    PartialWrite(usize),
    BeforeSync,
}

pub(super) struct Journal {
    file: File,
    pub(super) path: PathBuf,
    pub(super) projection: Projection,
    pub(super) offset: u64,
    seq: u64,
    tail: bool,
    #[cfg(test)]
    pub(super) fault: Option<Fault>,
}

impl Journal {
    pub(super) fn create(path: &Path, id: &str, snapshot: Snapshot) -> Result<Self> {
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let header = Event::SessionCreated {
            id: id.into(),
            created: u64::try_from(created)
                .map_err(|_| error(path, "session timestamp is invalid"))?,
            snapshot,
        };
        let projection = Projection::header(header.clone(), id, path)?;
        let file = open_private_file(path, true, true)?;
        let mut journal = Self {
            file,
            path: path.to_owned(),
            projection,
            offset: 0,
            seq: 0,
            tail: false,
            #[cfg(test)]
            fault: None,
        };
        journal.write_events(&[header])?;
        Ok(journal)
    }

    pub(super) fn open(path: &Path, id: &str) -> Result<Self> {
        let file = open_private_file(path, false, true)?;
        let mut reader = Reader::open(path, u64::MAX)?;
        let header = reader
            .next()?
            .ok_or_else(|| error(path, "session journal header is unavailable"))?;
        let mut projection = Projection::header(header, id, path)?;
        while let Some(event) = reader.next()? {
            projection.apply(&event, path)?;
        }
        let size = file
            .metadata()
            .map_err(|_| error(path, "cannot inspect session journal"))?
            .len();
        Ok(Self {
            file,
            path: path.to_owned(),
            projection,
            offset: reader.offset,
            seq: reader.seq,
            tail: reader.offset != size,
            #[cfg(test)]
            fault: None,
        })
    }

    pub(super) fn needs_recovery(&self) -> bool {
        self.tail || self.projection.active.is_some()
    }

    fn unchanged_file(&self, expected_size: u64) -> Result<()> {
        let metadata = std::fs::symlink_metadata(&self.path)
            .map_err(|_| error(&self.path, "journal path is unavailable"))?;
        if !metadata.is_file() || metadata.len() != expected_size {
            return Err(error(
                &self.path,
                "journal was changed outside its owning session",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let owned = self
                .file
                .metadata()
                .map_err(|_| error(&self.path, "cannot inspect owned journal"))?;
            if owned.dev() != metadata.dev() || owned.ino() != metadata.ino() {
                return Err(error(&self.path, "journal file identity changed"));
            }
        }
        Ok(())
    }

    fn write_events(&mut self, events: &[Event]) -> Result<()> {
        self.unchanged_file(self.offset)?;
        let mut seq = self.seq;
        let mut records = Vec::new();
        for event in events {
            records.push(encode(seq, event, &self.path)?);
            seq = seq
                .checked_add(1)
                .ok_or_else(|| error(&self.path, "journal sequence exhausted"))?;
        }
        #[cfg(test)]
        {
            match self.fault.take() {
                Some(Fault::BeforeWrite) => {
                    return Err(error(&self.path, "cannot append session journal"));
                }
                Some(Fault::PartialWrite(bytes)) => {
                    let first = &records[0];
                    self.file
                        .write_all(&first[..bytes.min(first.len().saturating_sub(1))])
                        .map_err(|_| error(&self.path, "cannot append session journal"))?;
                    return Err(error(&self.path, "session journal write was interrupted"));
                }
                Some(Fault::BeforeSync) => {
                    self.fault = Some(Fault::BeforeSync);
                }
                None => {}
            }
        }
        for record in &records {
            self.file
                .write_all(record)
                .map_err(|_| error(&self.path, "cannot append session journal"))?;
        }
        self.sync()?;
        // Publication/in-memory state follows durable append, never the reverse.
        self.offset += records
            .iter()
            .map(|record| record.len() as u64)
            .sum::<u64>();
        self.seq = seq;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        #[cfg(test)]
        if matches!(self.fault, Some(Fault::BeforeSync)) {
            self.fault = None;
            return Err(error(&self.path, "cannot synchronize session journal"));
        }
        self.file
            .sync_all()
            .map_err(|_| error(&self.path, "cannot synchronize session journal"))
    }

    pub(super) fn append(&mut self, event: Event) -> Result<()> {
        let mut next = self.projection.clone();
        next.apply(&event, &self.path)?;
        self.write_events(&[event])?;
        self.projection = next;
        Ok(())
    }

    pub(super) fn deltas(&mut self, events: &[Event]) -> Result<()> {
        // Streaming never clones the bounded model context for each text chunk.
        // Only the byte count changes; all records belong to the active turn.
        let mut bytes = self.projection.transcript_bytes;
        for event in events {
            let (turn, text) = match event {
                Event::AssistantDelta { turn, text } => (turn, text),
                Event::ThoughtDelta { turn, id, text } if id.valid() => (turn, text),
                _ => return Err(error(&self.path, "invalid streamed journal event")),
            };
            if self.projection.active != Some(*turn) {
                return Err(error(&self.path, "stale assistant journal delta"));
            }
            bytes = bytes
                .checked_add(text.len())
                .ok_or_else(|| error(&self.path, "transcript byte count exhausted"))?;
        }
        self.write_events(events)?;
        self.projection.transcript_bytes = bytes;
        Ok(())
    }

    pub(super) fn recover(&mut self) -> Result<()> {
        if self.tail {
            let size = self
                .file
                .metadata()
                .map_err(|_| error(&self.path, "cannot inspect interrupted journal"))?
                .len();
            self.unchanged_file(size)?;
            // Only an incomplete final line is discardable. Complete malformed,
            // checksummed or out-of-sequence records already failed during open.
            // Windows append-only handles omit FILE_WRITE_DATA, which set_len
            // requires. Keep the normal writer append-only and repair through a
            // separate writable handle while the same OS ownership lock is held.
            let repair = open_options()
                .write(true)
                .open(&self.path)
                .map_err(|_| error(&self.path, "cannot open interrupted journal for repair"))?;
            repair
                .set_len(self.offset)
                .and_then(|()| repair.sync_all())
                .map_err(|_| error(&self.path, "cannot repair incomplete journal tail"))?;
            self.sync()?;
            self.tail = false;
        }
        if let Some(turn) = self.projection.active {
            self.append(Event::TurnFinished {
                turn,
                outcome: Outcome::Interrupted,
                context: None,
            })?;
        }
        // A complete record can be visible in page cache after its previous
        // writer died before fsync. Establish durability even for a clean-looking
        // terminal/settings prefix before the resumed connection publishes it.
        self.sync()
    }

    pub(super) fn for_each_message(
        &self,
        mut emit: impl FnMut(Message) -> Result<()>,
    ) -> Result<()> {
        self.unchanged_file(self.offset)?;
        let mut reader = Reader::open(&self.path, self.offset)?;
        let mut turn = None;
        let mut messages: Vec<Message> = Vec::new();
        let mut indices = std::collections::HashMap::new();
        while let Some(event) = reader.next()? {
            match event {
                Event::TurnAccepted {
                    turn: next,
                    content,
                } => {
                    for message in messages.drain(..) {
                        emit(message)?;
                    }
                    indices.clear();
                    turn = Some(next);
                    emit(Message {
                        id: format!("user-{next}"),
                        kind: MessageKind::User,
                        content,
                    })?;
                }
                event @ (Event::AssistantDelta { .. } | Event::ThoughtDelta { .. }) => {
                    let (next, kind, id, part) = match event {
                        Event::AssistantDelta { turn, text } => {
                            (turn, MessageKind::Answer, format!("agent-{turn}"), text)
                        }
                        Event::ThoughtDelta { turn, id, text } => {
                            (turn, MessageKind::Thought, id.message_id(turn), text)
                        }
                        _ => unreachable!(),
                    };
                    if turn != Some(next) {
                        return Err(error(&self.path, "transcript message order is invalid"));
                    }
                    if part.is_empty() {
                        continue;
                    }
                    let key = (kind, id.clone());
                    if let Some(index) = indices.get(&key).copied() {
                        let message: &mut Message = &mut messages[index];
                        let serde_json::Value::String(text) = &mut message.content[0]["text"]
                        else {
                            return Err(error(&self.path, "invalid transcript text"));
                        };
                        text.push_str(&part);
                    } else {
                        indices.insert(key, messages.len());
                        messages.push(Message {
                            id,
                            kind,
                            content: serde_json::json!([{"type":"text","text":part}]),
                        });
                    }
                }
                _ => {}
            }
        }
        for message in messages {
            emit(message)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn legacy_answer_records_remain_readable_but_thoughts_require_version_six() {
        let path = Path::new("journal");
        for version in 1..=6 {
            for (value, valid) in [
                (
                    serde_json::json!({"type":"assistant_delta","turn":1,"text":"answer"}),
                    true,
                ),
                (
                    serde_json::json!({"type":"thought_delta","turn":1,"id":{"step":0,"item":0,"part":0},"text":"thought"}),
                    version == 6,
                ),
                (
                    serde_json::json!({"type":"thought_delta","turn":1,"id":{"step":0,"item":128,"part":0},"text":"thought"}),
                    false,
                ),
            ] {
                let event = serde_json::value::to_raw_value(&value).unwrap();
                let record = Record {
                    version,
                    seq: 1,
                    checksum: checksum(version, 1, event.get()),
                    event,
                };
                assert_eq!(
                    decode(&serde_json::to_vec(&record).unwrap(), 1, path).is_ok(),
                    valid
                );
            }
        }
    }

    #[test]
    fn selection_guards_require_version_four_and_explicit_settings_fields() {
        let path = Path::new("journal");
        for (version, state, valid) in [
            (3, None, true),
            (4, None, false),
            (
                3,
                Some(crate::selection::State::Blocked(
                    crate::selection::Reason::AccountChanged,
                )),
                false,
            ),
            (
                4,
                Some(crate::selection::State::Blocked(
                    crate::selection::Reason::AccountChanged,
                )),
                true,
            ),
        ] {
            let mut value = serde_json::json!({"type":"settings_changed","provider":"account","model":null,"portable":false});
            if let Some(state) = state {
                value["selection_state"] = serde_json::to_value(state).unwrap();
            }
            let event = serde_json::value::to_raw_value(&value).unwrap();
            let record = Record {
                version,
                seq: 1,
                checksum: checksum(version, 1, event.get()),
                event,
            };
            assert_eq!(
                decode(&serde_json::to_vec(&record).unwrap(), 1, path).is_ok(),
                valid
            );
        }
    }

    #[test]
    fn profile_ownership_requires_a_new_explicit_header_contract() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal");
        for (version, profile, valid) in [
            (1, None, true),
            (2, None, true),
            (3, None, false),
            (1, Some("work"), false),
            (2, Some("work"), false),
            (3, Some("work"), true),
            (3, Some(""), false),
        ] {
            let mut event = serde_json::json!({"type":"session_created","id":"saved","created":1,"snapshot":{
                "cwd":root.path(),"turns":0,"provider":null,"model":null,"protocol":null,"messages":[],
                "instructions":serde_json::to_value(crate::instructions::State::default()).unwrap()
            }});
            if let Some(profile) = profile {
                event["snapshot"]["profile"] = profile.into();
            }
            let event = serde_json::value::to_raw_value(&event).unwrap();
            let record = Record {
                version,
                seq: 0,
                checksum: checksum(version, 0, event.get()),
                event,
            };
            assert_eq!(
                decode(&serde_json::to_vec(&record).unwrap(), 0, &path).is_ok(),
                valid
            );
        }
    }

    #[rstest::rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    fn legacy_headers_accept_new_plan_and_thought_records_without_rewriting_the_old_prefix(
        #[case] version: u32,
    ) {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("history");
        // Windows files inherit their parent's ACL; a generic temp directory
        // does not provide the user-only permissions required by journal opens.
        super::super::private_directory(&directory).unwrap();
        let path = directory.join("session.jsonl");
        let mut event = serde_json::to_value(Event::SessionCreated {
            id: "saved".into(),
            created: 1,
            snapshot: Snapshot {
                selection_state: Default::default(),
                profile: "default".into(),
                cwd: root.path().into(),
                turns: 0,
                provider: None,
                model: None,
                protocol: None,
                messages: vec![],
                instructions: Default::default(),
            },
        })
        .unwrap();
        // Historical snapshots lack recency guards; v1/v2 also lack grouping ownership.
        if version < 4 {
            event["snapshot"]
                .as_object_mut()
                .unwrap()
                .remove("selection_state");
        }
        if version < 3 {
            event["snapshot"].as_object_mut().unwrap().remove("profile");
        }
        let event = serde_json::value::to_raw_value(&event).unwrap();
        // Construct historical framing/checksum rather than editing a new
        // record's version without updating its integrity check.
        let record = Record {
            version,
            seq: 0,
            checksum: checksum(version, 0, event.get()),
            event,
        };
        let mut prefix = serde_json::to_vec(&record).unwrap();
        prefix.push(b'\n');
        let mut file = open_private_file(&path, true, true).unwrap();
        file.write_all(&prefix).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let mut journal = Journal::open(&path, "saved").unwrap();
        assert_eq!(journal.projection.snapshot.profile, "default");
        journal
            .append(Event::TurnAccepted {
                turn: 1,
                content: serde_json::json!([]),
            })
            .unwrap();
        let plan = serde_json::from_value(serde_json::json!({"entries":[{"content":"Work","priority":"medium","status":"pending"}]})).unwrap();
        journal
            .append(Event::PlanUpdated { turn: 1, plan })
            .unwrap();
        journal
            .append(Event::ThoughtDelta {
                turn: 1,
                id: ThoughtId {
                    step: 0,
                    item: 0,
                    part: 0,
                },
                text: "visible".into(),
            })
            .unwrap();
        journal
            .append(Event::TurnFinished {
                turn: 1,
                outcome: Outcome::Cancelled,
                context: None,
            })
            .unwrap();
        drop(journal);
        assert!(std::fs::read(&path).unwrap().starts_with(&prefix));
        let journal = Journal::open(&path, "saved").unwrap();
        let mut thoughts = Vec::new();
        journal
            .for_each_message(|message| {
                if message.kind == MessageKind::Thought {
                    thoughts.push(message);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(thoughts.len(), 1);
        assert_eq!(thoughts[0].id, "thought-1-0-0-0");
        assert_eq!(thoughts[0].content[0]["text"], "visible");
        assert_eq!(
            journal.projection.plans[&1].entries[0].status,
            crate::plan::Status::Cancelled
        );
    }
}
