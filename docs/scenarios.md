# Deterministic ACPv2 test agent

Bee can act as a minimal ACPv2 reference implementation and reusable deterministic test agent for any compatible
client. Built-in scenarios exercise the same protocol boundary, session handlers, streaming updates, plans and
interaction cleanup as ordinary Bee, without an LLM or external services.

## Launch

Configure your client's agent command and arguments as:

```json
{
  "command": "bee",
  "args": ["--scenario", "success"]
}
```

From a source checkout, build with `cargo build` and use the absolute path to `target/debug/bee` as the command.
For manual protocol inspection, start `bee --scenario success` and send newline-delimited JSON-RPC on stdin:

```jsonl
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"example-client","version":"0"}}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp"}}
```

Read each response before continuing. Take `sessionId` from the second response and send a prompt:

```json
{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"<returned-session-id>","prompt":[{"type":"text","text":"Run the fixture"}]}}
```

Stdout contains protocol messages only. Close stdin to stop the process; diagnostics go to stderr.
`--scenario` conflicts with `auth` and `models` subcommands. Bare `bee` retains its normal configured inference mode.

## Scenario contracts

One scenario is selected for the process lifetime. Every accepted prompt starts it again, independent of prompt text.
Use a separate process to select another scenario.

| Name | Updates after the initial running state | Terminal stop reason |
| --- | --- | --- |
| `success` | Two assistant chunks: `Hello from Bee.` (with a trailing space) and `Scenario complete.` | `end_turn` |
| `plan` | Two item-plan snapshots, then `Plan complete.` | `end_turn` |
| `permission` | Pending synthetic tool card, permission request, result card and outcome text | `end_turn` |
| `input` | Synthetic input tool lifecycle, negotiated form request and outcome text | `end_turn` |
| `cancel` | `Ready for cancellation.`, then waits for client cancel or close | `cancelled` after client action |
| `error` | `Partial output before the controlled error.` | `error` |

All prompts use the ordinary lifecycle:

1. Prompt acknowledgement, containing the accepted user message ID.
2. `user_message` echo.
3. `state_update` with `state: "running"`.
4. Scenario updates and any client interactions.
5. Exactly one terminal `state_update` with `state: "idle"` and its stop reason.

The controlled error carries `Controlled scenario failure; send another prompt to repeat it.` in the terminal
update's `error.data`, using Bee's ordinary internal-error mapping. It is an accepted-turn failure, not an error
response to the already-acknowledged prompt. The session remains usable.

Determinism applies to content and per-turn ordering for the same client actions. Session, message, plan, tool and
JSON-RPC request IDs must be taken from the wire and correlated; do not snapshot their literal values or assert
wall-clock spacing between adjacent chunks. Concurrent sessions have independent lifecycles.

### Plans

`plan` publishes `plan_update` with an itemized plan containing:

- `Inspect fixture`: high priority, initially `in_progress`.
- `Verify fixture`: medium priority, initially `pending`.

The second snapshot replaces both statuses with `completed` under the same `planId`. Another prompt has a fresh
turn-owned plan ID. Plans are structured ACP items, not assistant text or client-specific metadata.

### Permission

`permission` announces a pending `tool_call_update` for `Scenario no-op`, then publishes `requires_action` before
`session/request_permission`. Its `tool_call` subject references that card's `toolCallId`. No shell command or
filesystem mutation is executed.

The offered option IDs are `allow_once`, `reject_once`, `allow_always` and `reject_always`. Reply using the request ID:

```json
{"jsonrpc":"2.0","id":"<permission-request-id>","result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}
```

Either allow choice produces `running`, an `in_progress` card, then a completed card and `Permission approved.`
Rejection, an unknown option, a cancelled outcome or a request error produces `running`, a failed card and
`Permission denied.` An explicit cancelled dialog outcome is:

```json
{"jsonrpc":"2.0","id":"<permission-request-id>","result":{"outcome":{"outcome":"cancelled"}}}
```

Always decisions belong to the client. Bee stores no decision; the next prompt requests permission again.

### User input

Advertise `"capabilities":{"elicitation":{"form":{}}}` in `initialize.params` to receive form requests.
`input` announces and starts a tool card, publishes `requires_action`, then sends `elicitation/create` with
`mode: "form"`, `sessionId`, `toolCallId`, and a required string `answer` property of 1–4096 characters.
Bee additionally bounds accepted answers to 4096 UTF-8 bytes. The question is `Enter a non-secret scenario answer.`

```json
{"jsonrpc":"2.0","id":"<elicitation-request-id>","result":{"action":"accept","content":{"answer":"blue"}}}
```

After the response, Bee publishes `running`, finishes the tool card with the bounded result, then emits
`Input accepted.` A declined/cancelled dialog, request error or invalid answer instead produces a failed tool card
and `Input unavailable, declined, or invalid.` Use `{"action":"decline"}` or `{"action":"cancel"}` as the result
to exercise those paths. The answer is retained in the tool result rather than repeated in assistant text.

Without negotiated form support, the scenario sends no elicitation request and reports the unsupported-input tool
result followed by the same unavailable summary. Dialog rejection ends the turn normally.

### Cancellation and close

For `cancel`, wait for the readiness chunk, then send:

```json
{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"<returned-session-id>"}}
```

There is no automatic completion or sleep. A second prompt while waiting is refused as busy. Cancel and close also
work while `permission` or `input` awaits a reply: pending outgoing requests receive `$/cancel_request`, unfinished
tool cards become `cancelled`, and cleanup precedes terminal idle. A late reply cannot restart the cancelled turn.
`session/close` additionally responds after cleanup and idle. After cancellation, another prompt restarts the scenario.
Client interaction requests retain Bee's normal bounded interaction timeout; tests should respond or cancel explicitly.

## Isolation and supported subset

Scenario mode ignores global/project configuration, `BEE_CONFIG`, `BEE_PROFILE`, credentials, instructions and skills.
It opens no persistent session store, preference file or catalog cache, discovers no MCP tools, and makes no provider
or external network requests. There are no provider/model selection options.

Sessions support the normal new/list/resume/close/prompt/cancel lifecycle. Visible messages, tool presentation and
plans can be replayed with `session/resume` and `replayFrom: {"type":"start"}` within the same process. Sessions
do not survive process restart. This mode does not exercise LLM inference, native execution, browser login, terminal
bytes or file diffs. See the [compatibility matrix](compatibility.md) for Bee's broader supported protocol subset.

The real-binary tests in `tests/stdio.rs` and `tests/support/scenarios.rs` assert serialized messages, ordering,
response validation, cleanup and terminal states. Consumers can use the documented event boundaries to synchronize
their own assertions without timing-sensitive sleeps or Bee-specific UI conventions.
