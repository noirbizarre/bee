# ADR-002: Reach client-hosted tools through the SDK's MCP-over-ACP path

## Status

Accepted

Historical feasibility record. [ADR-015](015-client-hosted-mcp-execution.md) records the subsequent production
integration; statements about the unimplemented agent below describe the original spike.

## Context

Bee does not configure or orchestrate MCP; the client does. A client reaches an
agent's tool calls in one of two ways: by naming an MCP server Bee connects to
itself (HTTP or stdio), or by hosting the server and relaying every message over
the ACP connection. The second is the SDK's MCP-over-ACP path, and it is the one
that fits an agent with no configuration of its own.

This records what the SDK offers today, from reading it and from the spike in
`tests/mcp.rs`. It is current SDK behaviour, not a statement about the
protocol's future. At the time of writing:

- `agent-client-protocol` is 3.5.0. MCP-over-ACP works with the v2 API
  (`Agent.v2()`, `Client.v2()`, `schema::v2`), behind the `unstable_mcp_over_acp`
  feature on top of `unstable_protocol_v2`. Neither feature pulls in a
  dependency.
- The SDK's wire protocol for it is one method, `mcp/message`. The agent sends it
  as a request to the client and gets a `MessageMcpResponse` back, either a
  `Result` or an MCP `Error`. There is no `mcp/connect` or `mcp/disconnect`.
- There is no MCP `initialize`. The only MCP version accepted is `2026-07-28`,
  and each request must carry it, with the client capabilities, in
  `params._meta` (`io.modelcontextprotocol/protocolVersion` and
  `io.modelcontextprotocol/clientCapabilities`). Without them the call fails
  with -32602.
- The agent side has no helper. Nothing in the SDK lists or calls a tool for an
  agent: it builds a `v2::MessageMcpRequest` with a request id, a method
  (`tools/list` or `tools/call`) and params, and reads the answer itself. A
  request id must be unique while the request is active.
- The client side has no tool builder in the core crate either. The companion
  crate `agent-client-protocol-rmcp` 4.0.5 provides
  `McpServer::<Agent>::builder(..).tool_fn(..)`. It depends on `rmcp` 3.x,
  `schemars`, `tokio` and `tokio-util`.
- A client attaches the server to a session with
  `connection.build_session(cwd).with_mcp_server(server)?.start_session()`. The
  agent then finds it in `NewSessionRequest::mcp_servers` as
  `McpServer::Acp(server)`, which carries a `server_id` and a `name`. Entries of
  other transports (`Http`, `Stdio`, `Other`) can sit in the same list.
- The client polls its MCP server while `session/new` is pending, so an agent may
  send `mcp/message` from its `session/new` handler. It must spawn the call and
  answer `session/new`, or the handler waits on a response that needs it to
  return.
- The SDK does not check advertised capabilities. A well-behaved client sends
  `McpServer::Acp` only to an agent that advertises
  `session.mcp.acp`.

## Decision

- Validate the flow in-process, in `tests/mcp.rs`, with the same pattern as
  ADR-001: a v2 client connected directly to a v2 agent. The repeatable
  reproduction is `cargo test --test mcp`.
- The test client hosts one `echo` tool through `agent-client-protocol-rmcp`,
  pinned `=4.0.5` with `unstable_protocol_v2` and `unstable_mcp_over_acp`, as a
  dev-dependency only. `schemars`, `serde` and `serde_json` are dev-dependencies
  for the tool's types. Bee itself gains no dependency.
- The agent in the spike is local to the test. `src/acp.rs` is unchanged, so
  Bee's real agent still advertises no capabilities and `bee::run()` still
  reports ACPv2 as not implemented.
- The spike covers `tools/call`, `tools/list`, a call to an unknown tool (an MCP
  error, -32602, inside a successful ACP response) and a call without the
  `_meta` the SDK requires. The last two pin current SDK behaviour so an upgrade
  that changes it shows up in a test.
- No MCP registry, client or protocol layer is introduced.

## Consequences

- The tool path the SDK offers works end to end with v2, so the MCP integration
  issue can build on it without a new dependency for Bee.
- Bee has to write the agent-side calling code itself: request ids, the `_meta`
  block and parsing of the MCP result. That is small, but it is Bee's to keep
  correct across SDK bumps.
- The test dependency tree grows by the companion crate and `rmcp`. It does not
  reach the binary.
- The companion crate is a second exact pin to review on each SDK bump, and it
  has to stay compatible with the core crate's version.

### Follow-ups

Each is current SDK behaviour, not a statement about the protocol's future:

- Advertise `session.mcp.acp` once Bee implements the session lifecycle: the
  session and prompt lifecycle issue.
- Move the agent-side tool calling (listing, calling, request ids and `_meta`)
  from the test into `src/acp.rs`: the MCP tools integration issue.
- Re-check on each SDK bump whether `unstable_mcp_over_acp` is still needed,
  whether the MCP version is still `2026-07-28` only, and whether the SDK has
  gained an agent-side helper.
