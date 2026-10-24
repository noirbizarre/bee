# ADR-015: Execute client-hosted MCP tools through the ACP boundary

## Status

Accepted. Extends ADR-002's feasibility spike and the tool-executor seam in ADR-012;
updates ADR-001's single-file boundary and ADR-004's initial capability subset.

## Context

The bounded loop already executes native tools and preserves provider-native continuation. ADR-002 established
`tools/list` and `tools/call` over `mcp/message`, but the production agent ignored session MCP declarations.
Issue #10 requires the model to use this validated path without moving server configuration or permission policy
into Bee. SDK handlers dispatch serially, so waiting for an MCP response inside a session handler would deadlock.

## Decision

- Keep SDK imports within `acp` and its private `mcp` submodule. Explicitly enable the runtime SDK's
  `unstable_mcp_over_acp` feature; rmcp remains dev-only.
- Advertise only ACP MCP transport. Validate attachments before new/resume changes; successful resume replaces
  attachments and close removes them. Snapshot declarations at prompt acceptance.
- Discover the catalog on each spawned prompt, outside dispatch and without locks held during I/O. Bound pagination,
  catalog size, schema depth and elapsed discovery time. Reject failed discovery instead of exposing partial catalogs.
- Compose native and remote execution at the ACP boundary; leave the SDK-free loop and provider adapters unchanged.
  Use offline schema validation and stable digest aliases with explicit routing, avoiding name collisions across
  servers and native tools. Include original names in descriptions for model usability.
- Generate fresh connection-wide logical request IDs and include version/capabilities in inner MCP metadata.
- Preserve the entire JSON result/error envelope and its metadata. Feed errors and client/server denials back to the
  model with the original call ID; use sanitized local envelopes for outer failures, invalid results and timeouts.
  Replace oversized results with complete errors rather than truncating JSON.
- Leave permission decisions with the client/server; do not add per-call approval. Support completed results only:
  interactive continuation, tasks and unknown outcome types produce explicit unsupported errors.
- Own pending requests within the turn so cancel/close/timeout drops them and triggers SDK cancellation. Never retry
  remote side effects automatically.

## Consequences

The production loop can use client-hosted MCP tools with all three inference protocols, alongside native tools.
No separate MCP configuration or runtime MCP client dependency is needed. Per-prompt discovery reflects changing
availability but adds latency and fails a turn if any declared server cannot be discovered.

Aliases are stable under ordering changes but change when a server's routing ID changes. Results preserve non-text
content as JSON, without adding provider-native multimodal support. Limits apply after SDK decoding and do not promise
a transport-level memory or synchronous schema-compilation sandbox. Remote cancellation remains cooperative.

`tests/mcp.rs` retains the feasibility contract; `tests/mcp_integration.rs` drives actual production ACP inference,
client-hosted tools and local fake provider HTTP. See [tools](../tools.md#client-hosted-mcp-tools) for concrete limits.
