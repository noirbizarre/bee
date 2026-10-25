# ADR-004: Serve the baseline session lifecycle with a placeholder turn

## Status

Accepted. Superseded in part by [ADR-005](005-prompt-cancellation-and-error-mapping.md): `session/cancel`
is no longer a no-op, turns no longer run inside the handler, and overlapping prompts are refused.

Placeholder execution and lack of model context are superseded by [ADR-012](012-streaming-inference.md).

## Context

Implementation update: [ADR-015](015-client-hosted-mcp-execution.md) adds ACP MCP capability and session attachments;
the capability and ignored-server statements below describe this initial lifecycle implementation.

Bee answered `initialize` and nothing else. The next step is sessions and
prompts, without an LLM: the protocol shape has to be right before anything
real is put behind it.

From reading `agent-client-protocol` 3.5.0 and `agent-client-protocol-schema`
1.12.0:

- Advertising `session: {}` commits an agent to the whole baseline:
  `session/new`, `list`, `resume`, `close`, `prompt`, `cancel` and
  `session/update`. A client acts on that promise (ADR-001).
- The `session/prompt` response only acknowledges that the user message was
  inserted, and carries its `messageId`. Progress and completion arrive as
  `session/update` notifications: `state_update` (`running`, then `idle` with a
  stop reason) and message updates.
- A resume must send an idle snapshot before it responds. A cancel is a
  notification, and requires an update only if foreground work was active.
- Handlers run one at a time in the SDK's event loop.

## Decision

- Implement the whole baseline, minimally, and advertise exactly `session: {}`.
  Nothing else is advertised (`prompt`, `mcp`, `delete`,
  `additionalDirectories`).
- Every prompt is a fixed turn: `user_message` (the id the response returned),
  `state_update: running`, one `agent_message_chunk` with a constant
  placeholder text, then `state_update: idle` with `stopReason: end_turn`. The
  text does not depend on the prompt, so it cannot be mistaken for model
  output. **It is temporary**: the agentic loop and a provider replace it, and
  the README says so.
- Session state is `bee::session::Sessions`, free of the SDK, owned per
  connection: `agent()` builds a fresh registry, so connections share nothing.
  Within one, each session has its own message counters.
- Ids are counters (`bee-session-N`, `user-K`, `agent-K`), not UUIDs. They are
  deterministic for tests and need no dependency; a stdio child process serves
  one client, so unguessability buys nothing.
- Closed sessions are retained and can be resumed. `close` is idempotent;
  `resume` requires the session's own `cwd`.
- Initially, all `replayFrom` and `session/list` cursors were rejected with
  `-32602`. **Superseded for resume by #13 (2026-10-25):** the draft permits zero
  retained ACP history, so omitted/null and the standard `start` cursor succeed
  with a current-state snapshot before the response. Unknown/malformed replay
  cursors and list cursors still return `-32602`; Bee never issues a list cursor.
- `session/cancel` is accepted and does nothing. The placeholder turn ends
  inside its own handler, so no work is ever in flight. Real cancellation is
  the cancellation issue.
- `mcp_servers` in `session/new` is accepted and ignored.
- A prompt for an unknown or closed session is an `-32602` error response with
  an actionable message, and no update: no turn started.

## Alternatives rejected

- **`session/new` and `session/prompt` only.** The smallest diff, but while
  advertising `session: {}` it would send clients to methods that answer
  `-32601`, the failure ADR-001 warns about.
- **UUIDs.** A new dependency, and tests could no longer assert exact ids.
- **Spawning a task per turn.** Needed once a turn awaits a provider, but
  without one it only adds a window where the response and the turn race.

## Consequences

- A client can create a session, prompt it and see a valid, terminated turn.
- There is no "busy" state: a second prompt is never refused for overlapping.
  That arrives with asynchronous turns, and cancellation with it.
- Session history is not retained, so resume cannot replay.
- The error mapping (`-32602` for unknown sessions) is a first cut, to be
  revisited with the error-handling work.
- An SDK upgrade that changes the update shapes shows up in `tests/session.rs`.
