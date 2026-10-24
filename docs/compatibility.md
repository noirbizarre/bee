# ACPv2 compatibility

Bee targets the draft ACP **wire version 2** over newline-delimited JSON-RPC on stdio.
The supported subset is tested against the real binary and in-process. This page records
the actual coverage and known gaps; Bee does not currently pass the full official TCK.

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
| Active resume reports running, idle resume reports idle | `tests/cancel.rs`, `tests/session.rs` |
| Cancel and direct close during model inference, recovery, no failed history | `tests/stdio.rs`, `tests/inference.rs` |
| Cancel/close during MCP discovery and invocation, isolation and late replies | `tests/mcp_integration.rs` |
| Request cancellation precedes cancelled idle; idle precedes close response | `tests/mcp_integration.rs`, `tests/cancel.rs`, `tests/stdio.rs` |
| Complete native prompt → tools → final answer → retained context | `tests/e2e.rs`; all three inference protocols in `tests/inference.rs` |
| Complete client-hosted MCP prompt → tool → final answer | Official SDK/RMCP client against the subprocess in `tests/e2e.rs`; three protocols in `tests/mcp_integration.rs` |
| Native-tool failures, limits and recovery through production ACP | `tests/e2e.rs`, `tests/inference.rs` |
| Per-tool invalid inputs, failure preservation and output limits | `tests/read.rs`, `tests/discovery.rs`, `tests/write.rs`, `tests/edit.rs`, `tests/bash.rs`, `tests/tools.rs` |
| Malformed requests, errors, protocol-only stdout, EOF shutdown | `tests/stdio.rs`, `tests/cli.rs`, `tests/cancel.rs` |

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
cargo test --test e2e --test stdio --test acp --test session --test cancel --test inference --test mcp_integration
mise run ci
```

The existing Linux/macOS/Windows CI nextest matrix discovers these integration targets
automatically. `mise run ci` also checks formatting, Clippy, spelling, Markdown,
workflows and the documentation build.

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
and network access to install the pinned tooling on the first run. The TCK launches a
fresh Bee process per test with an isolated cwd, HOME and configuration. Its cancellation
prompt deliberately holds a model response pending; it does not use a live provider.
Additional arguments are passed to the TCK. The runner preserves the TCK exit code.

### Recorded result — 2026-10-24

On Linux, against the baseline above: **82 tests passed, 40 skipped, 3 failed**.
All mandatory requirements that ran passed; the TCK verdict was **NOT CONFORMANT**
because three session-capability requirements failed:

| Requirement | Observed gap |
| --- | --- |
| `ACP-RESUME-202` | TCK requires accepting `replayFrom: {"type":"start"}` and ordering any replay before the response. Bee rejects it. |
| `ACP-RESUME-206` | The snapshot test also requests `replayFrom: start`; that request is rejected. Snapshot ordering without replay passes. |
| `ACP-RESUME-209` | Replay/schema validation cannot proceed because `replayFrom: start` is rejected. |

The draft permits zero retained replay updates, but this TCK still requires accepting
the start request. Bee's current explicit rejection is a compatibility gap, not a TCK
pass or an independently negotiable capability. It remains tracked under
[#13](https://github.com/noirbizarre/bee/issues/13). The TCK task therefore currently
exits nonzero and is separate from the passing Rust CI regression suite.

Skipped tests include unadvertised authentication/content/session extensions and
interaction/tool-update paths this fixture does not produce. In particular, the TCK
run does not validate native mutations, MCP round trips, permissions, or elicitation.
Bee's own deterministic workflows cover native and MCP execution. A passing TCK run
would still not be an official certification or complete protocol coverage.

## Unsupported and deferred behavior

- **ACP update replay:** model context survives close/resume in memory, but ACP event
  replay is not retained. Any supplied `replayFrom` is rejected with invalid params.
- **ACP permission and elicitation flows:** Bee does not initiate
  `session/request_permission`, `elicitation/create` (form or URL), or
  `elicitation/complete`. Advertising elicitation support as a client does not activate
  these flows. Implementation, negotiated modes, outcome handling, correlation and
  pending-interaction cancellation are tracked in
  [#46](https://github.com/noirbizarre/bee/issues/46).
- **MCP interaction:** client-hosted MCP denials are ordinary tool errors; they are not
  ACP permission outcomes. MCP `input_required` and task continuations are rejected.
  Only client-hosted ACP MCP attachments are advertised, not direct stdio/HTTP MCP servers.
- **Native content search:** the shipped catalog is `bash`, `read`, `glob`, `list`,
  `write`, `edit`. Embedded native content search awaits
  [#30](https://github.com/noirbizarre/bee/issues/30). Filename globbing and Bash do not
  satisfy that acceptance item.
- **Other optional surfaces:** image/audio/embedded prompt context, session deletion,
  forks and additional directories are not advertised. Resource links are references
  only and are never fetched.

Issue #13's native-search and permission/elicitation acceptance items remain outstanding;
current-subset coverage does not complete them.

## Client evidence

The official Rust SDK client **3.5.0** and the independent Python TCK above have been
run against the real Bee stdio binary. These are automated interoperability evidence,
not a claim that an editor or session hub has been validated.

[Hive](https://github.com/noirbizarre/hive) was checked as a real application candidate
at commit `0f0662b0fe77faedba2794759c65608d332f22ec`: its manifest targets draft v2
with SDK **3.4.0**. No Hive executable or release binary was available in the test
environment, so no runtime application-client result is recorded. Its `doctor` command
probes initialization only; its CLI has no headless prompt-submission command. A future
application smoke should record the exact client commit, transport, launch configuration,
and observed session/prompt/cancellation behavior. Bee has no dependency on Hive.
