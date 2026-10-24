# ADR-001: Build on the ACP Rust SDK's draft v2 API

## Status

Accepted

## Context

Implementation update: [ADR-015](015-client-hosted-mcp-execution.md) extends the ACP boundary to private submodules
and enables production MCP-over-ACP. The original SDK decision below is retained as history.

Bee is the agent side of an ACPv2 connection. Writing its own JSON-RPC framing
and protocol types would mean maintaining a second interpretation of a protocol
that is still moving.

The official Rust SDK is `agent-client-protocol`
([repository](https://github.com/agentclientprotocol/rust-sdk), Apache-2.0).
At the time of writing:

- The version is 3.5.0, with `rust-version = 1.88`, which is Bee's MSRV.
- `Agent.builder()` is the stable v1 surface. ACPv2 is reached through
  `Agent.v2()` and the `schema::v2` types, behind the `unstable_protocol_v2`
  feature.
- The SDK is runtime-agnostic. Its examples use tokio. A `Channel::duplex()`
  in-memory transport connects a client to an agent in one process.
- A stdio transport (`Stdio`) needs the `stdio` feature.
- Advertising `SessionCapabilities` commits an agent to the whole baseline
  session lifecycle.
- MCP-over-ACP is behind the separate `unstable_mcp_over_acp` feature.

## Decision

- Depend on `agent-client-protocol` with the exact pin `=3.5.0` and the single
  feature `unstable_protocol_v2`. Draft APIs behind an `unstable_*` flag may
  change in a minor release, so every upgrade is a deliberate change.
- Import the SDK from one module only, `src/acp.rs`, so an upgrade touches one
  file.
- Validate the integration in-process with `Channel`, from a v2 client in
  `tests/acp.rs`. This is the repeatable reproduction: `cargo test --test acp`.
- The spike answers `initialize` and advertises no capabilities. It is not wired
  to `bee::run()` or the binary, which still reports ACPv2 as not implemented.

## Consequences

- Bee tracks the SDK's reading of the protocol instead of its own. A gap
  surfaces as an SDK upgrade to adapt to, not as a protocol rewrite.
- The exact pin means Dependabot proposes each SDK bump, and each is reviewed.
- The runtime dependency tree grows by about 70 crates, mostly `futures`, `serde`
  and the SDK's schema crates.
- `tokio` is a dev-dependency only. The runtime for the binary is not decided.

### Follow-ups

Each is current SDK behaviour, not a statement about the protocol's future:

- Session methods (`session/new`, `prompt`, `cancel`, `close` and the rest) and
  the `SessionCapabilities` they require: the session and prompt lifecycle issue.
- The stdio entry point, which needs the SDK's `stdio` feature and a runtime
  choice: the stdio agent entry point issue.
- MCP-over-ACP, which needs `unstable_mcp_over_acp`: the MCP validation issue.
- Re-check whether v2 is still behind `unstable_protocol_v2` on each SDK bump,
  and drop the feature and the exact pin once it is stable.
