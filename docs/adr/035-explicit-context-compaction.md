# ADR-035: Explicit transactional context compaction

## Status

Accepted. Extends ADR-014 and ADR-019; preserves ADR-031's private/displayable separation.

## Context

Whole-turn eviction bounds inference but forgets earlier decisions. ACP v2 can present a retained summary without
deleting the client's conversation. Summarization must not split tool batches, expose private continuation, promote
failed work, or leave visible progress unfinished during cancellation or recovery.

## Decision

Offer a no-argument local `/compact` through standard prompts and static `availableCommands`. Keep the SDK and
official wire-v2 alpha.9 specification pin. There is no compaction capability or compact RPC to negotiate in v2.
Automatic eviction remains independent.

Use the selected authenticated account/model for one bounded tool-free request. Summarize all older retained
successful turns plus previous summary, protecting the newest complete turn. Source only portable user/answer
text and content-matched bounded tool display previews. Hash original observed tool envelopes so reused call IDs
or private result additions cannot borrow another result's preview. Missing old previews are explicitly unavailable.
Do not submit hidden guidance, thoughts or native continuation; reject compacted media and oversized source.

Store the portable summary outside ordinary messages and guidance. Native system/instructions payloads explicitly
label escaped summary text as untrusted historical data. This avoids fake user turns and preserves whole-turn
eviction and native continuation semantics. Account for the actual wire representation, including cache rewrites.

Journal version 8 adds explicit compaction start and summary-only terminal commits. A single record replaces the
summary, removes the validated complete prefix, closes the command and completes its presentation. It neither
appends a model turn nor advances guidance, protocol or recency. Failure/cancellation retains previous context;
interrupted recovery closes the entity without promoting candidate text. Existing ambiguous-sync behavior remains
atomic old-or-new, rather than claiming rollback of a fully written record.

Publish standard in-progress/terminal compaction updates, buffering bounded output and publishing a full retained
summary after commit. Optional chunk streaming is omitted because provisional model output is not committed memory.
Replay full snapshots at first-observed positions; reserve cumulative replay capacity before emission. Bound source,
summary, safe tool text and historical compaction presentation independently. Preserve original visible messages and
the physical journal; semantic summary retention does not depend on presentation retention.

## Consequences

Explicit compaction needs another inference request and can fail when its source cannot fit. Missing tool previews
and unsupported media limit summary fidelity; neither permits fallback to hidden full context. A model can produce
an inaccurate summary, so original visible history remains available. Normal request accounting still includes
guidance/tools separately from the command's labelled retained-conversation occupancy estimate.

Vendor compaction endpoints, synthetic conversation turns and eager context mutation were rejected: they would
couple portable memory to native private semantics or weaken transactional guarantees. Streaming summary chunks
can be added later without changing the retained full-snapshot replay contract.

Evidence lives in `tests/compaction.rs`, compaction cases in `tests/stdio.rs`, `src/compaction.rs` tests,
`src/llm/cache/tests.rs` and summary commit/recovery/retention cases in `src/history/tests.rs`.
