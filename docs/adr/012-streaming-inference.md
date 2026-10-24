# ADR-012: SDK-free inference with cancellable HTTP and caller-owned tools

## Status

Accepted. Supersedes the placeholder/no-history execution in ADR-004 and the executor choice in ADR-003.

## Decision

Implementation update: [ADR-015](015-client-hosted-mcp-execution.md) wires production MCP tools into the executor
seam alongside native tools. Statements about production sending no tools below describe the original inference work.

The first backend (#8) introduces concrete normalized messages, tool calls/results, optional usage and completion
reasons. A closed protocol dispatch selects the profile's preferred endpoint, never a vendor name or model catalog.
Responses (#26) and Anthropic (#23) implement their own wire semantics against the same interface. Native continuation
items remain protocol-owned rather than being forced into Chat's message shapes.

Model HTTP uses async reqwest with rustls, streaming, finite deadlines, bounded SSE framing and no redirects/retries.
`bee::run` chooses a Tokio reactor; async inference callers must supply a Tokio runtime. SDK connection tasks are polled
within that runtime, not detached workers. Dropping a cancelled turn drops its HTTP future/response. Login retains its
blocking HTTPS-only ureq seam; configured local inference can use HTTP.

ACP still owns turn registration, stale-token checks and terminal updates (ADR-005). No registry lock spans I/O. Model
context is accumulated locally and committed only under the current turn token, together with terminal completion.
Failed, cancelled or refused turns are excluded. Context is in-memory and is distinct from ACP event replay.

The bounded loop executes complete calls through an injected executor and retains one result per ID. Production MCP
integration is deferred by scope: production sends no tools and does not advertise execution. This proves backend
tool cycles without adding client tool discovery, orchestration or local tool hosting ahead of that work.

Errors retain classifications, not raw HTTP/JSON bodies or transport messages. Prompt/tool data has redacted Debug;
generated protocol/authentication headers cannot be overridden by custom headers.

## Verification

Local HTTP tests exercise serialization, streaming, native continuation, injected-tool cycles, failure classification
and cancellation. Real ACP integration tests cover prompt wiring and transactional history. Existing lifecycle and
stdio tests continue to enforce terminal ownership and protocol-only stdout. See [smoke tests](../inference.md).
