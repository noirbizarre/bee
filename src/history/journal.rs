//! The authoritative append-only JSONL log. No index or snapshot sidecar is
//! necessary to recover a session; future caches can be rebuilt from this file.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Message, Result, Snapshot, error, open_options, open_private_file};

// A corrupt/malicious line must not allocate the size of the entire journal.
// This bounds one record, not transcript length or the number of sessions.
pub(super) const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const VERSION: u32 = 1;

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
        portable: bool,
    },
    TurnAccepted {
        turn: u64,
        content: serde_json::Value,
    },
    AssistantDelta {
        turn: u64,
        text: String,
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
    if record.version != VERSION {
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
    serde_json::from_str(record.event.get()).map_err(|_| error(path, "invalid journal event"))
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
                portable: clear_native,
            } => {
                if self.active.is_some() {
                    return Err(invalid());
                }
                self.snapshot.provider.clone_from(provider);
                self.snapshot.model.clone_from(model);
                if *clear_native {
                    portable(&mut self.snapshot.messages);
                }
            }
            Event::TurnAccepted { turn, content } => {
                if self.active.is_some() || *turn <= self.snapshot.turns || !content.is_array() {
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
            Event::AssistantDelta { turn, text } => {
                if self.active != Some(*turn) {
                    return Err(invalid());
                }
                self.transcript_bytes = self
                    .transcript_bytes
                    .checked_add(text.len())
                    .ok_or_else(invalid)?;
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
                self.active = None;
            }
        }
        Ok(())
    }

    pub(super) fn settings(&self, next: &Snapshot, path: &Path) -> Result<Event> {
        if self.snapshot.cwd != next.cwd
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
            portable: clear_native,
        })
    }

    pub(super) fn context(&self, next: &Snapshot, path: &Path) -> Result<Context> {
        let invalid = || error(path, "successful context is not an oldest-whole-turn delta");
        if next.cwd != self.snapshot.cwd
            || next.turns != self.snapshot.turns
            || next.provider != self.snapshot.provider
            || next.model != self.snapshot.model
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
            let Event::AssistantDelta { turn, text } = event else {
                return Err(error(&self.path, "invalid streamed journal event"));
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
        let mut text = String::new();
        while let Some(event) = reader.next()? {
            match event {
                Event::TurnAccepted {
                    turn: next,
                    content,
                } => {
                    if !text.is_empty() {
                        emit(Message {
                            id: format!("agent-{}", turn.unwrap_or_default()),
                            user: false,
                            content: serde_json::json!([{"type":"text","text":std::mem::take(&mut text)}]),
                        })?;
                    }
                    turn = Some(next);
                    emit(Message {
                        id: format!("user-{next}"),
                        user: true,
                        content,
                    })?;
                }
                Event::AssistantDelta {
                    turn: next,
                    text: part,
                } => {
                    if turn != Some(next) {
                        return Err(error(&self.path, "transcript message order is invalid"));
                    }
                    text.push_str(&part);
                }
                _ => {}
            }
        }
        if !text.is_empty() {
            emit(Message {
                id: format!("agent-{}", turn.unwrap_or_default()),
                user: false,
                content: serde_json::json!([{"type":"text","text":text}]),
            })?;
        }
        Ok(())
    }
}
