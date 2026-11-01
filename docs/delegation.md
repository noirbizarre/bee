# Bounded task delegation

Bee can delegate focused tasks to any explicitly configured local ACPv2 agent and
use its answers in subsequent model requests. Each task creates a fresh process
and independent ACP session. Hive is optional; no Bee-specific child runtime is
required for ordinary delegation.

## Configuration and permission

Delegation is disabled by default. Configure named agents using the
[outbound client configuration](acp-client.md#named-agent-configuration), then opt in:

```toml
[delegation]
enabled = true           # default false
max_children = 4         # admitted child tasks per parent turn
max_concurrent = 2       # live children across the entire ACP connection
max_depth = 1            # default: children cannot delegate further
timeout_seconds = 600    # includes queueing, setup and execution

[agents.reviewer]
command = "compatible-agent"
args = ["--stdio"]
```

All settings merge through ordinary global → `BEE_CONFIG` → project configuration.
Project configuration may enable delegation or raise these configured limits.
Loading configuration never launches children. Unknown fields and invalid types
fail at startup even when disabled. Counts/concurrency must be integers from 1 to
64, depth from 0 to 64, and timeout from 1 to 86400 seconds. Depth zero exhausts
delegation authority.

Launch authorization uses the existing [permission rules](tools.md#native-permission-rules)
with action `delegate` and the exact configured agent name as resource, default
**ask**. A batch asks once per distinct selected agent before launching anything.
For example, this user-configured rule permits a selected delegate:

```toml
[[permissions]]
action = "delegate"
resource = "reviewer"
effect = "allow"
```

Permission rules retain their existing project-only-tightening behavior.
Approving delegation does not approve the child's subsequent operations. Child
permission requests are forwarded with their offered option IDs/kinds intact;
always decisions remain client-owned. Structured forms and URL interactions are
forwarded only when supported by the parent client. URL display acceptance is
separate from completion, with unique parent IDs even when siblings reuse child
IDs. Unsupported interactions fail closed. Child terminal references are removed
from permission presentation because they are not parent-owned terminals.

## Model-facing tool

When enabled with positive remaining depth and at least one configured agent,
Bee advertises the reserved `delegate` tool, including selectable agent names:

```json
{
  "tasks": [
    {
      "id": "review",
      "agent": "reviewer",
      "task": "Review the parser changes for correctness.",
      "context": "Focus on malformed input and error handling."
    }
  ]
}
```

The batch must be nonempty. IDs are unique within the batch, 1–64 ASCII letters,
digits, underscores or hyphens. Agent names must exactly match configuration and
fit within 128 UTF-8 bytes. Tasks are nonblank and at most 32768 UTF-8 bytes;
optional context has the same byte ceiling. Extra fields are rejected. The entire
JSON argument envelope is at most 128 KiB.

Bee sends only these task/context strings, not the parent conversation, private
reasoning, instruction payload, credentials or MCP attachments. The child's ACP
session uses the parent's session cwd. The executable still inherits the host
process cwd/environment according to the outbound launch contract: minimal prompt
context is not environment or filesystem isolation. A Bee delegate needs its own
usable model/account selection and authentication, for example through its
configured `BEE_CONFIG` file. Bee never picks an arbitrary child account/model.

Each valid, authorized batch reserves its entire child count atomically. Admitted
launch failures and timeouts still consume the turn budget; malformed or denied
batches do not. Repeated calls share that budget, which resets next turn.
The one `delegate` call also counts toward normal model/tool-call turn limits.
Children within it may overlap, while other tool calls retain sequential ordering.

## Results and progress

Results return in input order, regardless of completion order:

```json
{
  "results": [
    {
      "id": "review",
      "agent": "reviewer",
      "status": "completed",
      "stop_reason": "end_turn",
      "messages": [{"messageId": "answer", "content": [{"type": "text", "text": "Review findings"}]}],
      "partial": false,
      "truncated": false
    }
  ]
}
```

Statuses distinguish `completed`, `failed`, `refused`, `cancelled` and `timed_out`.
`stop_reason` preserves safe classifications, including `max_tokens`,
`max_turn_requests` and `unknown`; missing remote reasons do not imply `end_turn`.
Failed children include actionable error envelopes; arbitrary remote error details
and stderr are excluded. Successful sibling results and bounded observed partial
messages survive another child's failure or cleanup failure. Any unsuccessful
child marks the enclosing tool result as erroneous, allowing parent synthesis or
recovery through the normal tool-result channel.

The complete model-facing JSON result is at most 1 MiB. Each child has an encoded
content budget of at most 128 KiB, reduced for large batches; oversized updates
retain the last valid partial messages and fail the child. Omission is structural
and flagged, so the envelope always remains valid JSON. Queued progress is bounded
to 256 updates and the child content-byte ceiling. As with the outbound client,
these application limits do not bound SDK-internal channels or JSON line framing.

Parent clients see the normal correlated delegation tool card, bounded child
start/terminal notes, and the final result. Child thoughts, tools and terminals are
not copied into the parent's entity namespace. Concurrent interactions keep the
parent in `requires_action` until all outstanding waits resolve.

## Depth, cancellation and cleanup

Every delegated process receives `BEE_DELEGATION_REMAINING_DEPTH`, set by Bee after
configured environment overrides. Bee parses it as an unsigned decimal ceiling
after file configuration and uses the smaller of it and local `max_depth`.
Present empty, malformed, non-Unicode or overflowing values fail startup rather
than granting root authority. Embedded hosts can apply the same ceiling through
`Settings::with_remaining_depth` or `Settings::with_environment` before
`Runtime::with_delegation`.

A root depth of 1 launches children with remaining depth 0. Child Bee enforces
that ceiling in tool availability and execution; a model or child configuration
cannot restore exhausted authority. Higher depth is an explicit opt-in. Counts
and concurrency are local to each Bee connection, not tree-wide shared quotas.
Other implementations and wrappers can ignore this Bee-specific environment
contract; it is cooperative runtime policy, not a sandbox against arbitrary code.

Each task deadline starts at batch admission, including semaphore queueing.
Expired queued tasks never launch. Cancellation and timeout signal child work,
drain interactions, cancel unfinished foreground work and close sessions. Cleanup
uses bounded grace periods in addition to the task deadline: normally 2 seconds
per interaction-drain/cancel/close stage, an 8-second cancellation fallback for a
blocked transport, and a 2-second process-wait ceiling. Live-child permits remain
held through cleanup. Unix teardown kills the SDK-created process group; other
platforms terminate the immediate child. Escaped descendants are outside this
ownership guarantee.

Parent cancellation, close and connection shutdown retain supervision independently
of inference-future lifetime. Cleanup precedes terminal idle and direct-close
responses. Replay restores normal bounded presentation/results and never launches
children again. A delegated Bee may independently retain its own session journal
under its configured state directory, just like any other Bee session.

See [ADR-033](adr/033-bounded-acp-delegation.md) and `tests/delegation.rs` for the
ownership decisions and credential-free production integration coverage.
