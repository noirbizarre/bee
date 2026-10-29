# ADR-031: Displayable provider thoughts

## Status

Accepted

## Context

Issue [#72](https://github.com/noirbizarre/bee/issues/72) requires standard ACP thought output with streaming,
cancellation and durable replay. Native reasoning continuation already exists, but contains encrypted state,
signatures and redacted payloads whose presence does not authorize display. A text-only callback cannot distinguish
thoughts from answers or retain multiple reasoning entities across a tool loop.

## Decision

Adapters allowlist display fields into an SDK-free typed stream: Responses summary text and Anthropic returned
thinking text. Opaque continuation stays in native successful context; Chat reasoning extensions remain unsupported.
Keep text-only embedding entry points as answer-only wrappers. The loop supplies step identity; adapters supply local
item/part ordinals. ACP assigns turn-qualified IDs, publishes standard thought chunks, and replays full replacements
under the pinned v2 schema. No new capability or custom protocol message is required.

Visible history distinguishes user, answer and thought entities, preserving first-observation order while reducing
interleaved updates by kind and ID. Journal v6 adds thought deltas; v1–v5 remain readable without migration or inferred
historical thoughts. Thoughts share answer publication's persistence, queue, cancellation authority and terminal
barriers. Failed/cancelled/refused partials remain visible without entering successful context.

Generation controls are explicit endpoint options with unchanged defaults: automatic Responses summaries on direct
OpenAI API-key routes, and adaptive summarized thinking on direct Anthropic Messages. Reject explicit controls on
unverified routes rather than probe, strip options, retry or switch accounts. Receiving recognized display fields
does not require enabling generation controls. Bound display text/entities independently of native continuation.

## Consequences

The transcript can display safe provider output without treating all native JSON as public. Successful continuation
and context accounting still use the original native request representation exactly once. Full thought upserts make
restart/repeated replay idempotent; streaming and replay must both understand interleaving.

Separating callbacks and journal events adds parser/reducer coverage and a new history version. Older binaries reject
v6 journals rather than silently discard thoughts. Replay assembles interleaved entities for one turn within the
connection replay budget, rather than buffering just one answer. Private files still contain sensitive native context
and visible thoughts without encryption at rest; the existing retention policy applies.

We reject extracting arbitrary reasoning from native snapshots, provider IDs as display identity, and reasoning tags
inside ordinary text: none establishes a display contract. Supporting a new provider's reasoning or generation controls
requires explicit field/continuation and route evidence, not catalog reasoning flags or protocol compatibility alone.
