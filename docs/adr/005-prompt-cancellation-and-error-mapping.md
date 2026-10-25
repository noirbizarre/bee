# ADR-005: Run turns off the dispatch loop so prompts can be cancelled

## Status

Accepted. Supersedes parts of [ADR-004](004-session-and-prompt-lifecycle.md).

## Context

These lifecycle guarantees now apply to real inference and model history; see [ADR-012](012-streaming-inference.md).

ADR-004 ran the placeholder turn inside the `session/prompt` handler and made
`session/cancel` a no-op. That held only because the turn never waited. From
reading `agent-client-protocol` 3.5.0 and `agent-client-protocol-schema` 1.12.0:

- The SDK dispatch loop waits for each handler to return before it reads the
  next message. A turn that awaits inside the handler would hide a
  `session/cancel` behind itself until the turn it should stop had ended.
- A task started with `V2ConnectionTo::spawn` runs for as long as the connection
  is served. If it returns an error, the whole connection shuts down.
- A request handler that returns an error after it has responded makes the SDK
  answer the same request a second time.
- Cancelling active work must end with an `idle` update whose stop reason is
  `cancelled`, even if aborting the work raised errors. `session/close` must send
  that update *before* its response. A failure after insertion is an `idle` with
  the `error` stop reason; the prompt response stays successful.
- `$/cancel_request` does not apply: Bee answers a prompt as soon as the user
  message is inserted, and in ACPv2 only `session/cancel` stops the work after
  that.

## Decision

- **Turns run in a spawned task.** The prompt handler validates, registers the
  turn, responds, sends `user_message` and `running`, spawns the turn and
  returns. The turn is registered in the handler, not in the task, because the
  SDK dispatches a `session/cancel` the moment the handler returns.
- **One terminal state per turn, owned by whoever takes the turn out of the
  registry.** The turn finishing, `session/cancel` and `session/close` all go
  through `Sessions`, under one lock that is held while the update is sent. The
  one that takes the active turn sends the `idle`; the others stay silent. This
  is what keeps a cancelled turn from also reporting `end_turn`, and makes a
  late agent message impossible: output is only sent while the turn is still the
  session's current one, checked under the same lock.
- **Cancel aborts the turn future** (`futures::future::Abortable`) and reports
  `cancelled`. Cancelling an idle or unknown session does nothing and replies
  nothing: it is a notification.
- **Close cancels the running turn**, sends the `cancelled` idle, then responds.
- **A turn that fails or panics ends as `idle` with the `error` stop reason**,
  carrying a `-32603` error whose data says what failed. The spawned task always
  returns `Ok`, so one failed turn cannot shut the connection down, and the
  session is usable again.
- **Overlapping prompts are refused**, not queued: `-32600` (invalid request)
  with a message saying to wait or cancel. The request is well formed and
  arrives at a moment it cannot be honoured. Queueing adds protocol surface
  (queued prompts, `-32800` on close) with no need yet.
- **Content Bee did not advertise is refused before a turn starts.** Only
  `text` and `resource_link` blocks are accepted; `image`, `audio`, embedded
  `resource` and unrecognized blocks get `-32602` naming the block. Supporting
  them is [#22](https://github.com/noirbizarre/bee/issues/22).
- **Connection close drops the turns.** The SDK drops spawned tasks when the
  connection ends, so a turn cannot keep Bee alive; a test pins it.
- **Test seam.** `acp::agent_with` takes the turn body, and is `#[doc(hidden)]`.
  The placeholder turn never takes time, so no test could otherwise observe a
  running, cancelled or failed turn. It is not a provider abstraction.

### Error mapping

| Situation                                                      | Reported as                          |
| -------------------------------------------------------------- | ------------------------------------ |
| Invalid JSON; unknown method; method before `initialize`       | `-32700`, `-32601`, `-32600` (SDK)   |
| Request params that do not parse                               | `-32602` (SDK)                       |
| Unknown or closed session; `cwd` mismatch                      | `-32602`                             |
| `cursor` on list, unknown/malformed resume cursor               | `-32602`                             |
| Prompt content Bee does not support                            | `-32602`                             |
| Prompt while the session's turn is running                     | `-32600`                             |
| Failure or panic of a turn after insertion                     | `idle` with the `error` stop reason  |
| Malformed or unknown-session `session/cancel` (notification)   | nothing on the wire                  |

Resume clarification from #13 (2026-10-25): omitted/null `replayFrom` and
`{"type":"start"}` succeed with a current-state snapshot before the response.
No ACP history is retained, independently of successful model context.

`-32800` is not used: a prompt is never pending, because it is answered on
insertion.

## Alternatives rejected

- **Running the turn in the handler.** Blocks the dispatch loop, so a cancel is
  never seen while the turn runs.
- **Queueing overlapping prompts.** See above.
- **Letting the turn task return its error.** The SDK would shut the connection
  down for every failed turn.

## Consequences

- A client can cancel a running prompt, close a running session, and prompt
  again after a cancel or a failure.
- Cancellation is cooperative at await points: a turn blocked in synchronous
  code is only stopped once it yields. Real work must be async.
- Dropping a future gives no async cleanup. A provider integration that needs
  some (closing a stream) must do it in a drop guard.
- A turn that panics is reported to the client, but the panic message also goes
  to stderr through the default panic hook.
