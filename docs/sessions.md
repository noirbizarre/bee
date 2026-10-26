# Session history and restoration

The production `bee` binary persists sessions by default. A client can launch a new Bee process,
discover an existing native session ID and restore its conversation without keeping its own transcript copy.

## Storage and retention

Each session belongs to the [profile](profiles.md) selected when it was created. Listing returns only that profile's
sessions. Resume under another profile fails before recovery or replay, even if both reference the same connection.
An explicitly selected connection that has since been removed from the owning profile also prevents resume, rather
than silently choosing a different account. Legacy sessions belong to `default`. Select that profile and include the
original connections to resume them; Bee does not migrate session ownership between profiles.

Sessions live under `$XDG_STATE_HOME/bee/sessions`, falling back to `~/.local/state/bee/sessions`.
An empty or relative `XDG_STATE_HOME` is ignored. On Windows, USERPROFILE supplies home when HOME is unset.
If neither a state root nor home is available,
startup fails explicitly. Configuration and disposable model caches are separate from this state.

Each opaque session ID has a private directory containing `session.jsonl` and an `owner.lock` file.
Creation publishes a synchronized temporary journal by atomic file rename while keeping the directory and ownership
handle in place. Incomplete creations without a published `session.jsonl` do not appear in `session/list`.
Unix directories use mode 0700; journal and lock files use mode 0600. Windows directories use protected,
owner-only DACLs inherited by files. Existing unsafe permissions are refused.
History is not encrypted at rest: it contains prompts, assistant answers,
successful tool results, native continuation and committed instruction/skill snapshots.

There is **no automatic age, message-count or disk-size expiry**. Model-context eviction does not remove
the visible transcript. `session/close` detaches the session; it does not delete it. Session deletion is not
advertised. For manual removal, first close the session and stop any Bee process that owns it, then remove
that session's entire directory. Do not edit individual journal/lock files while attached. Ordinary tools can inspect
or archive the JSONL; a reader of an active journal must accept only its complete record prefix.

Embedded `Runtime` constructors default to isolated memory storage. Embedder-supplied `Runtime::with_history`
enables the same durable store explicitly. Memory-only sessions do not survive destruction of their registry.

### Journal and recovery format

`session.jsonl` is the **only authoritative session store**. Each line has a format version, a monotonically increasing
sequence number, a SHA-256 checksum and a readable JSON `event`. The checksum covers the exact event bytes together
with the format version and sequence; it does not depend on reserializing values with a particular library version.

The version 1 event types are:

- `session_created`: original identity, cwd, creation time and initial state.
- `settings_changed`: explicit provider/model choices and any removal of native continuation from future context.
- `turn_accepted`: allocated turn number and original user content blocks.
- `assistant_delta`: ordered visible answer text.
- `turn_finished`: terminal outcome and, only on success, new context messages, oldest-whole-turn eviction,
  protocol and changed instruction/skill payloads.

Version 2 adds `plan_updated` events. Version 3 makes profile ownership explicit in the immutable `session_created`
snapshot. Writers emit version 3; readers also accept versions 1 and 2, whose missing profile means `default`.
New records can append to legacy journals without rewriting their prefix. Older binaries reject version 3 rather than
ignoring profile ownership.

The terminal outcome and successful-context delta share one record. Unchanged retained context and guidance are not
copied into the journal after every turn. Full visible history remains available after model-context eviction.

The owner appends and synchronizes acceptance before acknowledging the prompt, text batches before emitting chunks,
and terminal commits before idle. On reopening, Bee reconstructs state by streaming complete records. Only an incomplete
final line may be truncated, after profile, connection-membership, exclusive ownership and cwd validation.
Invalid complete records, checksum failures, sequence
gaps and unsupported versions fail explicitly; Bee never skips them to fabricate an empty session.
After validation, Bee synchronizes the accepted prefix before publishing restored state, even when the last
complete record looks clean: its previous writer may have died before synchronizing it.

One serialized record is limited to **64 MiB**, including framing, to bound recovery allocations. This is not a total
journal-size or retention limit. A prompt or successful-context delta exceeding it fails explicitly. The writer also
checks that every encoded event can be read by the recovery decoder before appending it; overly deep structured
arguments fail explicitly instead of making a successfully acknowledged session unreadable on restart.

There is no database, shared-memory file or required index. If session discovery or resume eventually needs acceleration,
a rebuildable cache/index can be added without becoming the source of truth. Normal filesystem guarantees for exclusive
locking, append/synchronization and publication still apply; JSONL does not make unreliable distributed locking safe.

### Replay output budget

Replay has a **64 MiB cumulative output budget per connection**, including conservative message framing and queue
accounting. A complete replay reserves its budget before sending any messages; if it does not fit, resume fails
explicitly without sending partial history. Retention on disk is unaffected. Resuming without replay remains available.

The pinned SDK queues notifications without delivery acknowledgements. Bee therefore does not refund replay bytes when
notifications are queued: repeatedly requesting replay on a stalled connection cannot multiply buffered history. A fresh
connection resets the budget. For larger transcripts or more repeated restores, configure a larger budget and reconnect:

```toml
[history]
replay_max_bytes = 134217728 # 128 MiB per connection
```

The value must be a positive integer. Embedded runtimes can inject parsed `history::Settings` through
`Runtime::with_history_settings`. Replay memory assembly is bounded to one message, and both transcript measurement
and publication are serialized with live session state and text updates.

## Client flow

1. Initialize a Bee ACPv2 connection.
2. Call `session/list`, optionally filtered by the original absolute `cwd`. Bee returns one page in creation
   order; pagination cursors are unsupported. Save the native `sessionId` and original `cwd`.
3. Install session-update handlers **before** requesting resume: replay arrives before the response.
4. Send `session/resume` with the existing ID, original cwd, current MCP attachments and a start cursor:

   ```json
   {
     "sessionId": "bee-session-0123456789abcdef0123456789abcdef",
     "cwd": "/absolute/project",
     "replayFrom": {"type": "start"}
   }
   ```

5. Apply full `user_message` and `agent_message` updates as upserts by `messageId`. Their content replaces
   that message's prior content; subsequent live chunks append. Repeated start replay preserves IDs and
   order and does not duplicate stored messages. No nonstandard replay marker is required: the resume
   request/response brackets restoration.
6. Apply retained `plan_update` snapshots by `planId`, replacing the entire ordered entry list.
   These follow message replay and precede the current state. Empty lists are meaningful snapshots.
7. Consume the current `state_update`, which follows replay and precedes the resume response. Then continue
   the session normally. The response alone does not imply idle.

Omitting `replayFrom`, or passing null, synchronizes current state **without replay**. Only the start cursor
is supported; unknown/malformed cursors return `-32602` before reopening a session. Resuming the wrong cwd
also fails. Resume never silently creates a new session for an unavailable ID.

One connection owns a durable session at a time. Another process can list it, but resume fails while the
owner remains attached. Close, disconnect or process exit releases ownership after outstanding writes and
turn cleanup. Different sessions remain independently usable. Resuming a running session on its owning
connection preserves its running state and orders replay with live output and state transitions.

## Transcript versus model context

The visible transcript preserves original accepted user blocks, including resource-link references, and
assistant text already emitted. It also retains partial text from cancelled, failed and refused turns.
Prompt acceptance follows durable user-message insertion; live text follows durable delta commits.

The successful model-context projection is separate. Only successful, non-refused inference advances it; cancelled,
failed and refused inference never enters later model context. Whole-turn context eviction can remove old inference
messages while leaving their visible transcript intact. Successful native tool-call/result continuation and
committed guidance snapshots survive restart. Provider/model selection survives; provider configuration and
credentials are resolved from current configuration when inference next runs.

Replay sends historical user/assistant messages and latest execution-plan snapshots.
It does not execute tools, query providers, request
permissions, reopen login flows or restore previous MCP authority. Tool calls/results retained for model
continuation are data, not executable restoration instructions. Existing live behavior does not emit ACP
tool-lifecycle updates, so restoration does not invent historical tool events.

## Execution-plan retention

An observed turn plan is visible session history, separate from successful model
context. Latest snapshots survive close/resume, failed or cancelled turns,
whole-turn context eviction and production restarts. Embedded memory sessions
retain them only for the lifetime of that runtime. Start replay emits one latest
snapshot per observed turn in turn order, including explicitly empty plans;
omitted/null replay cursors do not resend plans. Replay budgets include plans.

Live revisions follow durable journal commits. Terminal records atomically
cancel unfinished entries alongside turn completion, regardless of stop reason;
an interrupted turn receives the same reconciliation during recovery. No plan
is emitted for a turn that never explicitly created one. Failures are conveyed
through the existing stop reason, since ACP has no standard failed entry status.
If terminal persistence fails, Bee still cancels unfinished client-visible
entries before error idle; reconnect recovery reconciles the last durable plan.

New journal records use version 2; Bee can read existing version-1 records and
append version-2 records to those sessions. Older Bee binaries cannot read the
new records and report unsupported history versions. Plans are stored in the
existing private journal, with no additional sidecar or executable replay data.

## Interruptions and unavailable history

After an unclean exit, the next owner marks any unfinished turn interrupted, keeps its durable partial
transcript and restores the previous successful context. Bee reports a warning and idle state; inference,
pending interactions and tool execution are never restarted automatically. Retried prompts are new turns.

Storage failures are explicit. Before prompt acceptance they produce a request error. After acceptance they
stop the turn with one error idle after cleanup; Bee does not publish a new in-memory context or emit uncommitted
text after a failed write. A 4 MiB per-session output queue (including command accounting) fails explicitly if storage cannot
keep up. Reconnect after fixing storage
to reload authoritative state. A crash between commit and delivery may leave durable text that the client
first sees during replay. An I/O failure can also leave a complete but unacknowledged record, including a successful
terminal commit; recovery uses the valid journal prefix rather than retrying the original execution. Incomplete records
never advance successful context.

Unknown, removed, unreadable, corrupt or unsupported-version history is not treated as an empty session.
Check the state root, file permissions, free space and Bee version. Sessions from an older memory-only Bee
process cannot be recovered after that process exits. A saved provider that is no longer configured does
not prevent transcript replay; configure/select an available provider before continuing inference.
