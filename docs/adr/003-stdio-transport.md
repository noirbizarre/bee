# ADR-003: Serve ACPv2 over the SDK's stdio transport, behind a transport-agnostic seam

## Status

Accepted

The executor choice is superseded by [ADR-012](012-streaming-inference.md); the transport seam remains.

## Context

An ACPv2 client starts Bee as a subprocess and speaks JSON-RPC on its stdin and
stdout. Two properties follow from that:

- Stdout is the protocol. One stray line, from a log or a debug print, corrupts
  the client's stream.
- A client ends the session by closing Bee's stdin, so EOF has to end the
  process cleanly.

Remote transports (HTTP/SSE, WebSocket) are planned but are not part of the MVP,
and adding one must not mean rewriting the agent.

At the time of writing, from reading `agent-client-protocol` 3.5.0:

- `Stdio` is the SDK's stdin/stdout transport, behind the `stdio` feature. It
  implements `ConnectTo<R>` for every role and reads and writes on the `blocking`
  crate's thread pool.
- Every transport is a `ConnectTo`: `Stdio`, `Channel` and `ByteStreams` alike.
  The agent never sees which one carries it.
- The SDK is runtime-agnostic. It needs an executor to poll the connection
  future, not tokio.
- The SDK answers malformed input itself: a line that is not JSON gets -32700, a
  request with bad params gets -32602, a method sent before `initialize` gets
  -32600, and an unknown method afterwards gets -32601. The connection survives
  each of them.

## Decision

- Enable the SDK's `stdio` feature and use its `Stdio`. Bee writes no framing.
- Two layers in `src/acp.rs`: `agent()` stays the transport-free core, and
  `serve(transport)` runs it over any `ConnectTo<Agent>`. `serve_stdio()` is
  `serve(Stdio::new())`. A remote transport later supplies another `ConnectTo`
  and touches nothing else. The SDK's `ConnectTo` is the abstraction, so Bee does
  not define a transport trait of its own.
- `bee::run()` drives `serve_stdio()` with `futures::executor::block_on`. It is
  the one place an executor is chosen. `futures` becomes a direct dependency; it
  was already in the tree.
- Stdout carries protocol traffic only. `Stdio` is built without a debug
  callback, and nothing in the library prints to stdout. Diagnostics go to
  stderr, rendered by miette in the binary.
- A peer closing its end is a normal end of the connection and `run()` returns
  `Ok`, so the process exits 0. Any other end is `Error::Connection`
  (`bee::acp::connection`), exit code 1.
- Tests spawn the real binary (`tests/stdio.rs`), because a clean stdout and the
  exit codes are properties of the process. `serve` is also tested over a
  `Channel` in `tests/acp.rs`, which shows the core is not tied to stdio.
- Capabilities do not change: Bee still advertises none, because sessions are
  not implemented.

## Alternatives rejected

- **A tokio runtime in the binary.** The SDK does not need one, and it would add
  a large dependency and a runtime choice to every future transport for no gain.
  Revisit if a provider client or HTTP transport needs it; the executor is
  chosen in one line.
- **Our own line-framing loop on `std::io`.** It would be a second
  interpretation of framing and error handling that the SDK already gets right,
  and the one ADR-001 set out to avoid.
- **A `Transport` trait in Bee.** `ConnectTo` already is one, so a wrapper would
  only be a second thing to keep in step with it.

## Consequences

- The binary starts as an ACPv2 agent on stdio: a client can complete
  `initialize`. Nothing else works until sessions are implemented.
- Stdout is only protocol traffic, which `tests/stdio.rs` enforces.
- There is no protocol trace on stderr yet. The SDK offers `with_debug`, but a
  safe default needs a verbosity story that does not exist, so it waits for the
  observability work.
- Signals get the OS default. A client that kills Bee mid-write is the client's
  problem; graceful handling of SIGINT/SIGTERM waits until there is state worth
  flushing.
- Blocking stdin I/O runs on the `blocking` crate's threads, which can outlive
  `run()` by a moment at exit. That is harmless for a process that is exiting.
- The exact SDK pin still applies: an upgrade that changes the error codes shows
  up in `tests/stdio.rs`.
