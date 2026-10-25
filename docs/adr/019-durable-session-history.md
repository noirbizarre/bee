# ADR-019: Restore durable session identity, context and visible conversation

## Status

Accepted. Supersedes the connection-local/in-memory retention and no-replay portions of ADR-004,
ADR-007, ADR-012 and ADR-014. Their lifecycle, provider-selection and transactional context rules remain.

## Context

Issue [#52](https://github.com/noirbizarre/bee/issues/52) exposed that clients could reopen sessions only
inside their original Bee process and could not reconstruct visible history. Bounded successful model
context is not a transcript: it excludes unsuccessful partial turns, flattens original prompt blocks and
evicts whole old turns. Replaying inference would repeat external tool effects.

Bee's storage workload is an ordered per-session event stream, with one owning connection and no current
need for relational queries. Session files should be inspectable, portable and easy to remove individually.
SQLite transactions simplify multi-record commits but add database/WAL lifecycle, filesystem constraints,
native compilation and maintenance work. Those costs are not justified by baseline list/resume/replay.

## Decision

Persist production sessions by default under the XDG state root. Use one private, versioned JSONL journal
and an exclusive OS ownership lock per session. Separate workers serialize each session independently.
Embedded runtimes inject durable storage explicitly and otherwise stay hermetic.

The JSONL journal is the sole source of truth. Records include identity/metadata, selections, accepted user
blocks, assistant deltas and terminal outcomes. Successful terminal records also contain new model messages,
whole-turn eviction boundaries, protocol and changed guidance. Do not repeatedly append the entire retained
context or unchanged skill snapshots. Visible unsuccessful turns remain separate from successful model context.

Include a monotonic sequence, format version and SHA-256 checksum covering exact payload bytes and framing
identity. Accept only complete records during recovery. The exclusive owner may repair an incomplete final
line after validating cwd; malformed complete records, checksum/sequence failures and unknown versions fail
explicitly. Bound individual records to 64 MiB without imposing a total retention cap.
Validate encoded records with the recovery decoder before append, including its nesting limits. Synchronize the
recovered prefix after cwd validation before publishing state, even if no incomplete tail needs repair.

Synchronize prompt acceptance before its response, text batches before delivery and terminal commits before
idle. Terminal outcome and successful context advancement share one record, avoiding a cross-file transaction.
A complete unacknowledged record may survive a crash or ambiguous I/O failure; recovery treats it as data,
never as an instruction to rerun execution. Unfinished turns recover as interrupted with prior successful context.

Serve the pinned ACPv2 resume/start contract: full-message upserts with original IDs and order, then current
state, then the response. The session worker orders replay with live deltas and state transitions. Reserve
complete replay against a configurable cumulative connection budget before publication, since the SDK has
unbounded notification queues and no delivery acknowledgement. Omitted replay requests only state.

Restore no MCP authorization, permission or elicitation state. Close detaches after cleanup and persistence;
it does not delete history. No automatic expiry initially. Private storage uses Unix modes or protected
owner-only Windows DACLs. Directory publication is synchronized or uses Windows write-through rename.

Keep any future query index or checkpoint rebuildable from the journal. SQLite may be useful later as a
derived metadata/history index, following the hybrid separation used by Codex; it is not a dependency today.

## Consequences

Clients restore native sessions across child-process restarts without their own transcript database.
Repeated replay is idempotent through upserts. Context eviction no longer limits visible history. Session
deletion and offline inspection use ordinary files, with no required database or WAL maintenance.

Bee owns a small record-validation and recovery implementation. Loading context requires scanning the
journal until a measured need justifies a derived checkpoint. JSONL still requires reliable filesystem
locking, flushing and publication; it is not a distributed multiwriter store. One owner excludes conflicting
writers and does not implement concurrent subscriptions.

Disk usage grows with retained transcript, successful-turn deltas and metadata until manually removed.
History is private but unencrypted. Atomic JSON snapshots alone would rewrite growing transcripts; separate
authoritative transcript/context files would reintroduce cross-file commit recovery. Keeping commits in one
append-only journal avoids both. A hybrid cache is deferred until an actual query/performance requirement.

Verification includes real-process restart, killed/cancelled/failed/refused partials, native continuation
for every protocol, guidance/eviction projections, side-effect-free repeated replay, exclusive ownership,
torn writes, complete corruption, sequence/checksum errors, bounded records and linear delta storage.

See [session history](../sessions.md) for storage, retention and replay limits.
