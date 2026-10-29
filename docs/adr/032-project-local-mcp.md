# ADR-032: Discover project MCP definitions and own direct connections per session

## Status

Accepted. Extends ADR-015's client-hosted execution and ADR-016's project-resource
boundary. Supersedes the ACP-only transport and dev-only rmcp restrictions of
ADR-015, while retaining its client-hosted authorization contract.

## Context

Issue #19 needs portable project-local server definitions, secret references
and explicit consent. Issue #70 tracks the direct
stdio/HTTP attachment runtime needed to execute those definitions. Parsing files
alone would provide no usable server. The current typed SDK response union also
loses extensions and explicit-null error data if round-tripped through its types.

## Decision

- Use `.agents/` for project agent resources; reserve `.agents/skills/` for
  standard Agent Skills. Share the nearest-Git-marker root rule with instructions.
- Use TOML-only `.agents/mcp.toml` with a `servers` envelope for all project servers.
  Use `.mcp.json`/`mcpServers` only as whole-project fallback.
  Reject duplicate IDs rather than assembling executable settings by precedence.
- Infer transport from exactly one `command` string array or `url`. Stdio's
  array includes executable and arguments, without redundant `type`/`args` fields.
  Claude fallback retains its standard string command and separate arguments.
- Normalize definitions without launching anything. Resolve explicit whole-value
  env references, freeze private effective settings, redact Debug/diagnostics,
  and keep provider credentials out of the default child environment.
- Keep project, client ACP and client direct identities distinct. Same-name
  servers coexist with source-qualified labels and explicit alias routing.
- Use a pinned direct MCP SDK for stdio/Streamable HTTP; preserve bounded raw
  envelopes before typed decoding. Retain the existing common catalog checks.
- Authorize direct launch/connect before initialization and authorize tool calls
  separately, default ask under existing permission rules. Client-hosted ACP
  retains client-owned policy. Always decisions remain client-owned.
- Own connections and initializing transports per session, outside inference.
  Track retired owners until teardown completes, so cancellation while reloading
  cannot abandon cleanup. Drain uncertain operations before idle, all connections
  on close, and connection state on disconnect. Never retry uncertain side effects.

## Consequences

Projects can execute MCP tools without requiring a client to implement local
configuration discovery or transport hosting. Bee now has a production direct
MCP dependency and process/network lifecycle responsibilities. Client approval
must handle the connection permission-subject extension. Sessions can keep
healthy processes alive between prompts; history is not authorization to restore
them after restart.

A single TOML source avoids speculative format proliferation; Claude JSON
is a concrete compatibility exception. Strict duplicate errors prevent silent
shadowing at the cost of requiring users to resolve conflicting definitions. There is
no registry, UI, filesystem watcher, MCP OAuth or generic plugin framework.

See [MCP configuration](../mcp.md), `tests/project_mcp.rs`,
`tests/mcp_integration.rs` and `src/mcp/runtime.rs` for contract and evidence.
