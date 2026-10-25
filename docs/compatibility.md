# ACPv2 compatibility

Bee targets the draft ACP **wire version 2** over newline-delimited JSON-RPC on stdio.
The supported subset is tested against the real binary and in-process. This page records
the actual coverage and known gaps, including a passing pinned official TCK run.

## Version baseline

| Component | Version |
| --- | --- |
| ACP Rust SDK | `agent-client-protocol = 3.5.0` |
| Resolved ACP schema | `agent-client-protocol-schema = 1.12.0` in `Cargo.lock` |
| Test client MCP bridge | `agent-client-protocol-rmcp = 4.0.5` |
| MCP-over-ACP protocol | `2026-07-28` |
| Official TCK | `0.2.0`, commit `b7f40fb3c003c45926547be5a1f7e2b374035815` |
| TCK vendored draft schema | `beac4ba96190018e9f6d82e148ff275084c5aa3e` |

SDK and bridge versions are pinned because the enabled `unstable_protocol_v2` and
`unstable_mcp_over_acp` features follow a changing draft. The installed pinned schema
and SDK are the implementation source of truth; the independently versioned TCK may
expose additional compatibility requirements. Updating one does not update the others.

Initialization returns this capability object:

```json
{"session":{"mcp":{"acp":{}}}}
```

The session capability includes the baseline lifecycle methods. It does not contain
separate flags for each method. The SDK selects v2 even when asked for v1; that
negotiation behavior does **not** mean Bee implements the v1 lifecycle.

## Automated coverage

Paths below are relative to the repository root.

| Behavior | Repeatable coverage |
| --- | --- |
| Initialization, identity, version negotiation, exact capabilities | `tests/acp.rs`, `tests/stdio.rs`, `tests/e2e.rs` |
| Session new/list/close/resume, cwd checks, invalid IDs and settings | `tests/session.rs`, `tests/selection.rs`, `tests/stdio.rs` |
| Prompt acceptance, echoed message IDs, running/text/terminal ordering | `tests/session.rs`, `tests/e2e.rs` |
| Streaming before the provider completes | Gated SSE response in `tests/e2e.rs`; provider adapter suites |
| Active resume reports running/requires_action, idle resume reports idle | `tests/cancel.rs`, `tests/session.rs`, `tests/interactions.rs` |
| Cancel and direct close during model inference, recovery, no failed history | `tests/stdio.rs`, `tests/inference.rs` |
| Cancel/close during MCP discovery and invocation, isolation and late replies | `tests/mcp_integration.rs` |
| Request cancellation precedes cancelled idle; idle precedes close response | `tests/mcp_integration.rs`, `tests/cancel.rs`, `tests/stdio.rs` |
| Complete native prompt → tools → final answer → retained context | `tests/e2e.rs`; all three inference protocols in `tests/inference.rs` |
| Complete client-hosted MCP prompt → tool → final answer | Official SDK/RMCP client against the subprocess in `tests/e2e.rs`; three protocols in `tests/mcp_integration.rs` |
| Native-tool failures, limits and recovery through production ACP | `tests/e2e.rs`, `tests/inference.rs` |
| Per-tool invalid inputs, failure preservation and output limits | `tests/read.rs`, `tests/discovery.rs`, `tests/grep.rs`, `tests/write.rs`, `tests/edit.rs`, `tests/bash.rs`, `tests/tools.rs` |
| Malformed requests, errors, protocol-only stdout, EOF shutdown | `tests/stdio.rs`, `tests/cli.rs`, `tests/cancel.rs` |
| Bash permission allow/reject/cancel/unknown/error/timeout, no nonapproval side effects | `tests/interactions.rs`; malformed replies and direct-close wire ordering in `tests/stdio.rs` |
| Independent form/URL negotiation, validated form responses, model-call correlation | `tests/interactions.rs`, across all three inference protocols |
| Real provider browser callback/exchange/store workflow; URL acceptance distinct from completion | `tests/interactions.rs`, with an injected credential exchange transport |
| Login transport HTTPS-only policy, redirect refusal, bounded/framed bodies, cancellation and deadlines | Unit tests in `src/http.rs` |
| Whole browser-login deadline during URL create, callback wait and exchange; cleanup and retry | Virtual-time cases in `tests/interactions.rs` |
| Pending permission/form/URL cancellation, direct close, late replies and session isolation | `tests/interactions.rs`; cleanup-before-idle assertions in client handlers |
| Omitted/null/start resume, unknown/malformed cursor rejection, snapshot-before-response and retained model context | `tests/session.rs`, `tests/stdio.rs`, `tests/cancel.rs`, `tests/interactions.rs`, `tests/inference.rs` |

Browser-login success, concurrency and cancellation scenarios in `tests/interactions.rs`
are Unix-only; the CI platforms do not all execute the same login matrix.

The native subprocess workflow discovers files with glob/list, reads a discovered
file, writes content derived from that read, makes a targeted edit, reads it back,
runs Bash, and returns a streamed final answer. Assertions check exact tool-result
envelopes and call IDs, file contents, an untouched sentinel, Bash status/stdout/stderr,
ACP message correlation, and successful history retention. A separate scenario verifies
that a failed overwrite leaves the file intact and an oversized read becomes a tool
error before corrected calls succeed.

The subprocess tests use temporary workspaces, isolated configuration, portable Bash
builtins and local deterministic HTTP providers. They require no model credentials.
Synchronization gates make streaming and cancellation observable without depending on
model speed. Every exchange has a deadline and subprocesses are cleaned up on failure.

Run the main compatibility tests with:

```sh
cargo test --test e2e --test stdio --test acp --test session --test cancel --test inference --test mcp_integration --test interactions
mise run ci
```

The existing Linux/macOS/Windows CI nextest matrix discovers these integration targets
automatically. `mise run ci` also checks formatting, Clippy, spelling, Markdown,
workflows and the documentation build. The separate project-owned
`.github/workflows/compatibility.yaml` runs the pinned TCK on Linux for pull requests
and main pushes, fails on TCK failure, and uploads its JSON requirement/transcript report.
On 2026-10-25, `mise run ci` passed locally on Linux: **736 Rust tests**, formatting,
Clippy, spelling, workflow/Markdown linting and the documentation build.

## Official compatibility tooling

The maintained protocol documentation points to
[acp-tck](https://github.com/agentclientprotocol/acp-tck), an experimental Python
test client with an opt-in draft-v2 suite. Bee supplies a pinned, reproducible runner:

```sh
mise run test:compatibility
# Optional: write the full wire transcripts and requirement verdicts outside the repository.
mise run test:compatibility -- --report-json /tmp/bee-acp-tck.json
```

This task builds Bee and runs `tests/compatibility.py`, which starts a local mock Chat
Completions endpoint and launches the pinned TCK through `uvx`. It needs `uv`, Python
3.14 or newer and network access to install the pinned tooling on the first run.
CI provisions Python 3.14 with uv and selects it for both the runner and nested TCK.
The TCK launches a fresh Bee process per test with an isolated cwd, HOME and configuration. Its cancellation
prompt deliberately holds a model response pending; it does not use a live provider.
Additional arguments are passed to the TCK. The runner preserves the TCK exit code.

### Recorded result — 2026-10-25

On Linux, against the baseline above: **85 tests passed, 40 skipped, 0 failed**.
The TCK verdict was **CONFORMANT**. Requirement totals (distinct from test counts):

| Tier | Passed | Failed | Skipped |
| --- | --- | --- | --- |
| Mandatory | 21 | 0 | 3 |
| Capability | 40 | 0 | 25 |
| Advisory | 15 | 0 | 4 |
| Informational | 10 | 0 | 7 |

The previously failing `ACP-RESUME-202/206/209` now pass: Bee accepts the standard
start cursor and sends a schema-valid
current-state snapshot before responding. Unknown and malformed cursors still fail
with `-32602` (`ACP-RESUME-207/208`). Retained-message identity is skipped because
that revision retained no ACP conversation updates; this was permitted by the draft.
This recorded run predates durable history. Bee now retains visible messages and replays
them as full upserts; real-process restart and repeated-replay tests cover that contract.

Skipped tests include unadvertised authentication/content/session extensions and
interaction/tool-update paths this fixture does not produce. In particular, the TCK
run does not validate native mutations, MCP round trips, permissions, or elicitation.
Bee's own deterministic workflows cover native/MCP execution and production ACP
permission and elicitation callers. This passing TCK run is not an official
certification or complete protocol coverage.

## Native content search

The shipped `grep` tool implements [#30](https://github.com/noirbizarre/bee/issues/30)
with embedded ripgrep matcher crates, ignore-aware directory traversal, line-numbered
results and explicit truncation. `tests/grep.rs` covers filtering, regex errors, binary
files, bounds, symlinks and cancellation. `tests/inference.rs` covers all three provider
protocols and scoped guidance; `tests/e2e.rs` searches through the real stdio binary
with an empty executable search path and verifies retained correlated results.
See the [search contract and dependency licenses](tools.md#built-in-content-search).

## Session restoration

Production sessions persist by default. `session/list` discovers existing IDs after restart;
`session/resume` with `replayFrom: {"type":"start"}` sends retained user/agent message upserts
through `session/update`, followed by current state before the response. Omitted/null replay
requests only current state. Replay never reexecutes tools. One connection owns each session;
competing resumes fail explicitly. See [session history](sessions.md) for the storage and retention contract.

The pinned v2 schema has no separate durability/replay capability flag. Bee continues advertising
only its implemented MCP transport and baseline lifecycle, without v1 `loadSession` flags.

## Unsupported and deferred behavior

- **Replay cursors:** only omitted/null and `{"type":"start"}` are supported. Other cursor
  types and malformed values return invalid params. History from older memory-only Bee processes
  cannot be recovered after those processes exit. Missing/corrupt history fails explicitly.
- **MCP interaction:** client-hosted MCP denials are ordinary tool errors; they are not
  ACP permission outcomes. MCP `input_required` and task continuations are rejected.
  Only client-hosted ACP MCP attachments are advertised, not direct stdio/HTTP MCP servers.
- **Other optional surfaces:** image/audio/embedded prompt context, session deletion,
  forks and additional directories are not advertised. Resource links are references
  only and are never fetched.

Issue [#46](https://github.com/noirbizarre/bee/issues/46) supplied production permission and
elicitation coverage for [#13](https://github.com/noirbizarre/bee/issues/13), as mapped
above; [#30](https://github.com/noirbizarre/bee/issues/30) supplied native content search.

## Client evidence

The official Rust SDK client **3.5.0** and the independent Python TCK above have been
run against the real Bee stdio binary. These are automated interoperability evidence,
not a claim that an editor or session hub has been validated.

### Real application smoke — 2026-10-25

[Hive](https://github.com/noirbizarre/hive) **0.1.0** was exercised with its actual
terminal client and daemon: **TUI WebSocket → Hive → Bee stdio**, ACPv2 throughout.
The inspected source checkout was `0f0662b0fe77faedba2794759c65608d332f22ec`, pinning
SDK **3.4.0**. The available development executable's SHA-256 was
`84745f999318d123fc3fc0c37aefb0eea75d3f832460c9368d130a2eb8fa17cb`;
its exact build commit was not independently established.

The smoke used temporary HOME/XDG directories and a project workspace, a daemon bound
to an ephemeral loopback port, and a Hive default profile with Bee's absolute binary
path, `versions = ["v2"]` and `BEE_CONFIG` pointing to an isolated mock Chat Completions
provider. The TUI ran in a pseudo-terminal with `--address <host:port> --agent bee`;
Ctrl-N/Enter created the session, Enter submitted prompts, and Ctrl-C cancelled work.
No live credentials or existing Hive instance/configuration were used.

Observed assertions: the created session had the expected cwd; `HIVE_STREAM` appeared
in the terminal while the SSE provider was still gated and the session was running;
releasing the provider led to idle; Ctrl-C during a second pending model request led
to idle; a third prompt rendered `HIVE_RECOVERED` and completed. The third provider
request retained the successful first answer and excluded the cancelled second prompt.
All three requests used the same session. Smoke-owned processes were stopped afterward.
This establishes application session/prompt/streaming/cancellation/recovery behavior,
not Hive permission or elicitation coverage. Bee has no dependency on Hive.
