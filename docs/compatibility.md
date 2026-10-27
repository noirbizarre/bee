# ACP / ACPv2 compatibility matrix

Bee targets the draft ACP **wire version 2** over newline-delimited JSON-RPC on stdio.
The supported subset is tested against the real binary and in-process. This matrix inventories
the applicable protocol surfaces, including features Bee does not implement. It is a compatibility
audit, not a claim of complete ACP conformance or certification.

The implementation audit was performed on **2026-10-26**, initially against Bee commit
`7cccee06a715b0711d3b522dfecd3bf0e1fe73c1`, then refreshed by code/test inspection against
`b8c3eeb` after rebasing onto main. Recorded test runs below identify their original tested revision;
they were not rerun for the rebase. Planned and not-planned dispositions reflect the
maintainer's decisions during [#59](https://github.com/noirbizarre/bee/issues/59), rather than
inferences from missing code. Existing interoperability runs below retain their own dates and scope.

## Version baseline

| Component | Version |
| --- | --- |
| ACP Rust SDK | `agent-client-protocol = 3.5.0` |
| Resolved ACP schema | `agent-client-protocol-schema = 1.12.0` in `Cargo.lock` |
| Official specification revision audited | `e2d83349bc9e28a4059ea3acefa1c91c966f74e2` |
| ACP v1 JSON schema / wire version | `schema-v1.26.0` / `1` — inventoried, not implemented |
| ACP v2 JSON schema / wire version | `schema-v2.0.0-alpha.9` / `2` — Bee's target |
| Test client MCP bridge | `agent-client-protocol-rmcp = 4.0.5` |
| MCP-over-ACP protocol | `2026-07-28` |
| Official TCK | `0.2.0`, commit `b7f40fb3c003c45926547be5a1f7e2b374035815` |
| TCK vendored draft schema | `beac4ba96190018e9f6d82e148ff275084c5aa3e` |

SDK and bridge versions are pinned because the enabled `unstable_protocol_v2` and
`unstable_mcp_over_acp` features follow a changing draft. The installed pinned schema
and SDK are the implementation source of truth; the independently versioned TCK may
expose additional compatibility requirements. Updating one does not update the others.

The #71 tool-presentation implementation retains this specification revision. Relevant draft changes
were rechecked against official main `9eaeebbe4dbadb6f3eed74aca41758960e75c6c4`: its clarification
that adjacent tool text chunks may render continuously is compatible with the pinned append semantics.
Its newer initialization version-negotiation rules are separate from this presentation implementation.

The #78 permission implementation also retains the audited revision. Permission schema
definitions and semantics were rechecked against official main
`5aa293599e554ca3a5751f27022519dd3cd4768c` and are unchanged. It introduces no
permission capability flag or newer version-negotiation behavior.

The three official tags `v1.12.0`, `schema-v1.26.0` and `schema-v2.0.0-alpha.9` resolve to
the specification commit above. These are independent version namespaces: schema crate `1.12.0`
does not mean wire version 1, and SDK `3.5.0` does not mean wire version 3.
The live protocol website can describe newer behavior, including different version-negotiation rules.
This audit uses immutable sources, not an implicit promise to follow website changes immediately.

### Specification citation key

Each table identifies its frozen schema/section and relevant definitions. **V2** means the baseline
schema within the still-draft wire-v2 protocol; **V2 experimental** means an addition in its unstable schema.
The unstable files include the baseline; enabling a local Rust feature does not negotiate every experimental feature.

| Key | Immutable official inventory or section |
| --- | --- |
| [V1][v1-schema] | Wire-v1 baseline schema; [complete method inventory][v1-methods] |
| [V1 experimental][v1-unstable] | Wire-v1 baseline plus experimental additions; [method inventory][v1-experimental-methods] |
| [V2][v2-schema] | Wire-v2 baseline schema; [complete method inventory][v2-methods] |
| [V2 experimental][v2-unstable] | Wire-v2 baseline plus experimental additions; [method inventory][v2-experimental-methods] |
| [Transport][spec-transport] | UTF-8 stdio framing, message timing and JSON-RPC batches |
| [Extensions][spec-extensions] | Negotiation, unknown fields/variants and metadata patches |
| [Lifecycle][spec-lifecycle] | Prompt acceptance, states, completion, notices, compaction and usage |
| [Migration][spec-migration] | V1-to-v2 changed and removed surfaces |

## Status and responsibility

| Status | Meaning |
| --- | --- |
| **Implemented and supported** | The stated behavior exists in production and has test evidence. This does not imply every optional producer variant is emitted. |
| **Partially supported** | A working subset exists; the row states its exact boundary and any tracked expansion or correctness gap. |
| **Planned** | Not implemented; an explicit follow-up issue tracks the desired behavior. No release date is implied. |
| **Not planned** | Bee does not intend to implement this surface under the current product decisions. |
| **Not assessed / unknown** | Evidence is insufficient; the row identifies what needs verification. This is not a synonym for unsupported. |

Directions below are relative to Bee's **agent/server** role. Clients own UI rendering, permission decisions,
MCP orchestration and elicitation presentation. A client's ability to display an event does not establish that
Bee produces it. Library embedding seams are not additional shipped binary transports.
AGENTS.md, Agent Skills, model tool names such as `ask_user` and `update_plan`, and provider inference protocols
are Bee behavior, not separate ACP methods. Hive's `_hive_activity` and gateway/UI behavior are not standard ACP.

## Connection, negotiation and errors

### Deterministic test-agent coverage

`bee --scenario <name>` is a shipped, stdio-based deterministic test-agent mode using the same ACP boundary and
session lifecycle. [The six built-ins](scenarios.md) exercise streamed text, item-plan replacement, tool-correlated
permissions, negotiated form input, cancellation/close cleanup and controlled accepted-turn errors. Real-binary
wire tests in `tests/support/scenarios.rs` cover ordering, response validation, replay and terminal states.
This evidence is separate from the recorded TCK runs below.

Scenario sessions are memory-only; provider/model selectors, inference, native execution, MCP discovery and browser
login are not exercised. No configuration, credentials or external services are required. The general capability
inventory below describes ordinary Bee unless explicitly stated otherwise.

### Connection inventory

Authority: [Transport][spec-transport], [Extensions][spec-extensions], and [V2][v2-schema]
`InitializeRequest`, `InitializeResponse`, `AgentCapabilities`, `ClientCapabilities`, `ErrorCode`.

| Feature | Status | Behavior, limitation and evidence / tracking |
| --- | --- | --- |
| UTF-8 newline-delimited stdio; protocol-only stdout | Implemented and supported | The production binary serves stdio; malformed lines do not destroy the connection. [Wire tests][test-stdio], including `stdout_carries_only_json_rpc_messages`. |
| EOF, client disconnect and turn cleanup | Implemented and supported | EOF exits successfully; disconnected clients drop active work. [Transport tests][test-acp], [wire tests][test-stdio]. |
| Optional remote / Streamable HTTP transport | Planned | No remote binary endpoint is shipped. The frozen HTTP proposal is a draft; choose a supported SDK integration in [#18](https://github.com/noirbizarre/bee/issues/18). |
| Custom/embedded transports | Implemented and supported | Library `serve` accepts SDK-compatible transports; `serve_runs_the_agent_over_any_transport_the_sdk_can_connect` in [tests/acp.rs][test-acp]. This is not a network server. |
| `initialize`, identity and version selection | Implemented and supported | SDK v2 builder returns Bee name/version and wire v2, including upgrading a v1 version field. [Initialization tests][test-acp]; no v1 lifecycle claim. |
| Agent capability advertisement | Implemented and supported | Exactly the object below; optional content/delete/directory transports are not advertised. `the_bee_agent_advertises_sessions_and_only_acp_mcp_transport` in [tests/acp.rs][test-acp]. |
| Client capability negotiation | Implemented and supported | Independently reads `elicitation.form` and `.url`; absent support disables those workflows. [Interaction negotiation tests][test-interactions]. Does not use client terminal-auth capabilities. |
| Asynchronous requests/notifications and correlation | Implemented and supported | Prompts acknowledge before completion; streaming and interaction replies are correlated independently. [Session tests][test-session], [interaction tests][test-interactions]. |
| JSON-RPC batches | Implemented and supported | Incoming parsing, aggregation, invalid-member handling and notification suppression use SDK 3.5.0's [transport implementation/tests][sdk-transport]; `ACP-BATCH-201/202/203/204` pass in the [current TCK run](#recorded-result-2026-10-26). Bee does not originate batches. |
| Parse / invalid request / method not found / invalid params / internal errors | Implemented and supported | `-32700`, `-32600`, `-32601`, `-32602`, `-32603`; pre-initialize calls and unknown methods are tested in [tests/stdio.rs][test-stdio]. Busy sessions use invalid request; bad IDs/settings use invalid params. |
| Request-cancelled / authentication-required / resource-not-found error families | Partially supported | SDK defines `-32800`, `-32000`, `-32002`; Bee does not use these as universal application mappings. Missing sessions use `-32602`; accepted-turn failures use terminal state. [Error mappings][acp-code]. |
| `$/cancel_request` | Partially supported | SDK cancellation-on-drop is exercised for outgoing permission/MCP/elicitation work; cleanup precedes idle/close. [Interaction tests][test-interactions], [wire tests][test-stdio]. Not an assertion that every incoming method is interruptible. |
| Unknown fields, optional display metadata and open unions | Implemented and supported | SDK decoding accepts and discards unknown fields on recognized objects (`ACP-EXT-204` for initialization); required operational fields remain validated. Optional display-field tolerance is SDK-owned. Unknown content envelopes are preserved as below; [wire tests][test-stdio]. |
| Unknown content discriminator fallback | Implemented and supported | Schema-valid unknown/custom blocks are accepted, echoed and retained as opaque JSON values. Malformed recognized variants cannot fall through to the SDK's open union. `invalid_and_unadvertised_content_is_rejected_before_insertion` and opaque-content replay tests in [wire tests][test-stdio]. |
| `_meta`, namespaced data and metadata patches | Partially supported | Emits `bee.dev/models` and `bee.dev/context`; preserves full MCP result envelopes. Arbitrary request metadata is not a durable session-metadata API. Clients apply standard patches; [selection][test-selection], [model][test-models] and [MCP tests][test-mcp]. |
| Custom `_` methods, notifications and extension products | Not planned | No custom RPC handlers or custom capability product is advertised. Unknown requests follow method-not-found handling; namespaced metadata is not a custom RPC implementation. [Handler registrations][acp-code]. |

### Advertised capability object

Initialization returns this capability object:

```json
{"session":{"mcp":{"acp":{}}}}
```

The session capability includes the baseline lifecycle methods. It does not contain
separate flags for each method. The SDK selects v2 even when asked for v1; that
negotiation behavior does **not** mean Bee implements the v1 lifecycle.

Itemized plans use the baseline `session/update` path; the pinned v2 schema has
no separate plan capability flag. Bee emits the standard full-replacement form:

```json
{"sessionId":"example","update":{"sessionUpdate":"plan_update","plan":{"type":"items","planId":"plan-1",
 "entries":[{"content":"Inspect","priority":"medium","status":"in_progress"}]}}}
```

`planId` is stable within the session; entries have no standard IDs. All entries
and their order replace the previous snapshot. Plans are distinct from ordinary
assistant messages, tool calls/results, and Hive-specific `_hive_activity` events.
See [execution plans](tools.md#execution-plans) for empty-plan and terminal rules.

## Sessions, prompts and configuration

Authority: [V2][v2-schema] lifecycle request/response definitions, `SessionCapabilities`,
`SessionConfigOption`, `SessionConfigOptionCategory`; [V2 method inventory][v2-methods].
All methods in this table are client → agent; updates are agent → client.

| Feature | Status | Behavior, limitation and evidence / tracking |
| --- | --- | --- |
| `session/new` | Implemented and supported | Creates independent identity/cwd/settings and validates MCP attachments. Production durably creates sessions; [session tests][test-session], [restart tests][test-stdio]. |
| `session/list`, cwd filter | Partially supported | Returns IDs/cwd in creation order, including production sessions after restart; exact cwd filtering. Single page, no titles/timestamps. [Listing tests][test-session], [history tests][test-stdio]. |
| List `cursor` / `nextCursor` pagination | Planned | Any supplied cursor is currently rejected because Bee never issues one. [#74](https://github.com/noirbizarre/bee/issues/74); `list_with_a_cursor_is_rejected_because_bee_never_issues_one` in [tests/session.rs][test-session]. |
| `session/resume`, current-state synchronization | Implemented and supported | Requires original cwd/known identity. Omitted/null `replayFrom` sends current state before response; active state remains active. [Session][test-session] and [cancellation tests][test-cancel]. |
| Start replay and stable entity identity | Implemented and supported | `replayFrom: {"type":"start"}` sends retained full messages, latest plans and tool/terminal snapshots, then current state; no execution replay. Unknown/malformed cursors rejected; cumulative replay budget can reject a whole replay. `tests/stdio.rs`, `tests/tool_presentation.rs`, [retention contract](sessions.md). |
| Durable sessions and exclusive attachment | Implemented and supported | Production survives restarts; one OS-lock owner, explicit corruption/storage failures. Embedded runtimes default to memory. [Process tests][test-stdio], [session history](sessions.md). Durability has no separate v2 capability flag. |
| Profile-scoped session ownership | Implemented and supported | Listing is limited to the active profile; cross-profile resume and removed selected connections fail before recovery/replay. `profile_ownership_isolates_history_and_models_across_process_restarts` in [wire tests][test-stdio]. This is local session policy, not an additional ACP capability. |
| `session/close` | Implemented and supported | Detaches rather than deletes. Active work/requests are cleaned up and terminal idle precedes response. Known closed sessions are idempotent; unknown IDs fail. [Close tests][test-cancel], [session tests][test-session]. |
| `session/prompt` acceptance and message IDs | Implemented and supported | Response acknowledges the accepted user message, not final completion. Echo → running → output → terminal idle; one foreground turn per session. [Ordering tests][test-session], [busy-turn tests][test-cancel]. |
| `session/cancel` notification | Implemented and supported | Cancels active work; idle/unknown IDs produce no spurious updates. No assistant output after terminal idle. [Cancellation tests][test-cancel], [MCP cancellation][test-mcp]. |
| `session/set_config_option` | Partially supported | Provider/model select IDs only, scoped per session; invalid types/IDs and active-turn changes fail. Unlisted model IDs are allowed. [Selection tests][test-selection], [option construction][acp-code]. |
| Select options, categories and background updates | Implemented and supported | Standard `model` category and custom `_provider`; no options when unconfigured. Optional discovery emits `config_option_update` without losing current custom model IDs. [Selection][test-selection], [model discovery tests][test-models]. |
| Profile/account-bound automatic model selection | Implemented and supported | New sessions prefer the profile's last successfully used connection/model/account; unsafe automatic reuse warns and requires explicit selection. Preference-write failure warns without undoing answers. [Selection tests][test-selection], [profile resolution](profiles.md#configured-default-and-last-used-model). |
| Boolean/additional settings, other standard categories | Planned | `boolean`, and concrete settings using categories such as `mode`, `model_config`, `thought_level`, are not currently offered. [#79](https://github.com/noirbizarre/bee/issues/79); not experimental provider-management RPCs. |
| Additional roots / `session.additionalDirectories` | Planned | Not advertised or integrated into session workspace state. [#69](https://github.com/noirbizarre/bee/issues/69). Native path access beyond cwd is not this capability. |
| `session/delete` / `session.delete` | Not planned | Not advertised; closing does not delete retained history. [Session retention](sessions.md#storage-and-retention), [capability tests][test-acp]. |

## Content and every session-update family

Authority: [V2][v2-schema] `ContentBlock`, `SessionUpdate`, `StateUpdate`, `IdleStateUpdate`,
`ToolCallContent`, `ToolCallStatus`, `ToolKind`, `PlanUpdateContent`, `UsageUpdate`, `Notice`;
[Lifecycle][spec-lifecycle]. Experimental additions are separated below.
All update names below are values of agent → client `session/update.sessionUpdate`, not separate RPC methods.

### Content blocks

| Feature | Status | Behavior, limitation and evidence / tracking |
| --- | --- | --- |
| Incoming `text` | Implemented and supported | Multiple blocks flatten to newline-separated model input; original accepted blocks remain visible history. [Prompt handling][acp-code], [wire context tests][test-stdio]. |
| Incoming `resource_link` | Partially supported | Accepted/echoed/retained; the model gets an explicit URI reference saying contents were not fetched. No resource retrieval claim. `a_prompt_with_a_resource_link_is_accepted` in [tests/cancel.rs][test-cancel]. |
| Incoming `image` / `session.prompt.image` | Planned | Known unadvertised content is rejected before a turn starts. [#22](https://github.com/noirbizarre/bee/issues/22), [rejection tests][test-cancel]. |
| Incoming `audio` / `session.prompt.audio` | Planned | Not advertised or passed to providers. [#22](https://github.com/noirbizarre/bee/issues/22), [content validation][acp-code]. |
| Incoming embedded `resource` / `session.prompt.embeddedContext` | Planned | Text/blob embedded resources are not accepted as prompt context. [#22](https://github.com/noirbizarre/bee/issues/22), [content validation][acp-code]. |
| Outgoing `text` | Implemented and supported | Live assistant chunks and full replay messages; streaming is observed before provider completion. [Subprocess tests][test-e2e], [session tests][test-session]. |
| Outgoing `image`, `audio`, embedded `resource` | Planned | Non-text MCP results inside JSON tool envelopes are not native ACP multimodal output. [#81](https://github.com/noirbizarre/bee/issues/81). |
| Outgoing `resource_link`, annotations and rich metadata | Partially supported | Original user links and their content-block metadata survive echo/replay; assistant live output is text, not rich links. Metadata does not imply model understanding. [History contract](sessions.md#transcript-versus-model-context), [prompt handling][acp-code]. |
| Unknown/custom content | Implemented and supported | Opaque envelopes survive live echo, memory replay and durable restart replay as JSON values, without custom semantic support. Mixed prompts send only understood text/link projections to providers; unknown-only prompts use a fixed generic marker with no payload or discriminator. `opaque_prompt_blocks_survive_memory_echo_and_repeated_start_replay` in [session tests][test-session] and `opaque_prompts_replay_after_restart_without_entering_current_or_later_model_input` in [wire tests][test-stdio]. |

### Messages and foreground state

| Feature | Status | Behavior, limitation and evidence / tracking |
| --- | --- | --- |
| `user_message` | Implemented and supported | Full accepted echo and replay upserts with the prompt response's message ID. [Identity and ordering tests][test-session]. |
| `user_message_chunk` | Planned | Bee currently echoes complete user messages only. [#81](https://github.com/noirbizarre/bee/issues/81). |
| `agent_message_chunk` | Implemented and supported | Correlated live text chunks, with no post-idle output. [Session streaming][test-session], [cancellation tests][test-cancel]. |
| `agent_message` | Partially supported | Full retained upserts on start replay; live output uses chunks without a separate final whole-message upsert. [Replay tests][test-session], [restart tests][test-stdio]. |
| `agent_thought`, `agent_thought_chunk` | Planned | Provider-native reasoning continuation is not ACP thought presentation. [#72](https://github.com/noirbizarre/bee/issues/72). |
| `state_update`: `running`, `requires_action`, `idle` | Implemented and supported | Foreground work, pending client interactions, and terminal completion/resume snapshot. Cleanup precedes terminal idle. [Lifecycle][test-session], [interaction tests][test-interactions]. |
| Idle reasons `end_turn`, `max_tokens`, `refusal`, `cancelled`, `error` | Implemented and supported | Provider completion/cancellation/failure map to terminal updates. Accepted failures do not become a second prompt response. [Finalization code][acp-code], [terminal-plan tests][test-plans], [failure tests][test-cancel]. |
| Idle `max_turn_requests`, null and open stop-reason variants | Partially supported | State-only idle snapshots can omit a reason; loop-limit failures currently use `error`, not `max_turn_requests`. Bee does not produce custom reasons. [Finalization][acp-code], [limit tests][test-plans]. |

### Tools, terminals and plans

| Feature | Status | Behavior, limitation and evidence / tracking |
| --- | --- | --- |
| `tool_call_update` | Implemented and supported | Correlated pending/in_progress/completed/failed/cancelled cards for native/MCP calls, bounded inputs/results, implementation-owned kinds, and known absolute locations. Emits read/edit/delete/move/search/execute/other; think/fetch/switch_mode are not current producers. Turn-qualified IDs also correlate permissions/forms. Evidence: `tests/tool_presentation.rs`, `tests/interactions.rs`, `tests/mcp_integration.rs`. |
| `tool_call_content_chunk`; tool `content` blocks | Implemented and supported | Bounded text/JSON result content, diffs and terminal references; full replacement content on replay. Complete MCP envelopes remain model-facing; rich multimodal presentation is separate #81. Evidence: `tests/tool_presentation.rs`, `tests/mcp_integration.rs`, `tests/stdio.rs`. |
| Tool `diff` content | Implemented and supported | Committed add/delete/modify/move/copy operations from native write/edit/delete/move/copy, including directories and partial deletion. Optional bounded git_patch with absolute Git-quoted paths; no patch for unavailable/binary/large/directory-tree content. Bash/MCP output is not interpreted as mutation evidence. Evidence: `every_file_change_operation_is_serialized_as_standard_v2_diff_content` in `tests/stdio.rs`, `src/presentation/patch.rs`, `src/tool/tree.rs`. |
| `terminal_update`, `terminal_output_chunk`; tool `terminal` references | Implemented and supported | Agent-owned Bash process observation: independent base64 raw-byte chunks, bounded prefix snapshots, command/cwd and actual or unknown exit details. No input/resize/process-control RPCs. Evidence: `raw_terminal_bytes_and_cancelled_tool_are_published_before_idle` in `tests/tool_presentation.rs`, `bash_output_is_captured_without_corrupting_the_protocol_stream` in `tests/stdio.rs`. |
| Tool/terminal retention, recovery and cancellation | Implemented and supported | Visible state survives failed turns, context eviction and restart. Full snapshot replay is idempotent; unfinished entities close before idle/close and reconcile on recovery. Bounds and append-only disk policy: [session contract](sessions.md#tool-and-terminal-retention). Evidence: `tests/tool_presentation.rs`, `src/history/tests.rs`, `src/presentation.rs`, `tests/stdio.rs`. |
| `plan_update`, `plan.type: items` | Implemented and supported | Standard ordered full replacement under a turn-owned `planId`; empty snapshots clear entries. High/medium/low priorities and pending/in_progress/completed/cancelled states. [Plan tests][test-plans], [plan implementation][plan-code]. |
| Plan finalization, retention and replay | Implemented and supported | Unfinished entries become cancelled before terminal idle, even on normal end. Latest snapshots survive failure, eviction and production restart. Bounded to 128 entries, 4096 bytes/entry, 64 KiB aggregate. [Plan tests][test-plans], [wire restart test][test-stdio]. |

### Commands, metadata, accounting and compaction

| Feature | Status | Behavior, limitation and evidence / tracking |
| --- | --- | --- |
| `available_commands_update`, response `availableCommands` | Planned | No advertised command set or command handling contract yet. [#73](https://github.com/noirbizarre/bee/issues/73). |
| `config_option_update` | Implemented and supported | Optional model discovery publishes current select options; does not update closed sessions. [Model discovery tests][test-models], [configuration](models.md#acp-and-cli). |
| `session_info_update` | Planned | No live title/timestamp/custom metadata patch updates. [#74](https://github.com/noirbizarre/bee/issues/74). |
| `usage_update`: context `used` / `size` | Partially supported | Estimated committed occupancy only when effective capacity is known; `bee.dev/context` labels method/provenance/output reservation. Not token billing. [Context wire tests][test-stdio], [usage contract](inference.md#usage-and-overflow-reporting). |
| `usage_update.cost` | Planned | No monetary cost reporting. [#80](https://github.com/noirbizarre/bee/issues/80). |
| `notice` | Implemented and supported | Truncation/interruption, skill-discovery, automatic-selection and preference-write diagnostics; not conversation history or replayed accounting. No client capability required in v2. [Wire tests][test-stdio], [selection tests][test-selection], [emission code][acp-code]. |
| `compaction_update`, `compaction_summary_chunk` | Planned | Whole-turn eviction/notices are not summary compaction or retained-summary streaming. [#75](https://github.com/noirbizarre/bee/issues/75). |

## Permissions, input requests and authentication

Authority: [V2][v2-schema] `RequestPermissionRequest`, `RequestPermissionSubject`, `PermissionOptionKind`,
`RequestPermissionOutcome`, `CreateElicitationRequest`, `ElicitationSessionScope`, `ElicitationRequestScope`,
`CreateElicitationResponse`, `AuthMethod`, `AuthCapabilities`; [V2 method inventory][v2-methods]. Requests run agent → client
except `auth/login` and `auth/logout`, which run client → agent.

| Feature | Status | Behavior, limitation and evidence / tracking |
| --- | --- | --- |
| `session/request_permission`, command subject | Implemented and supported | Validated native Bash when routing resolves to ask; exact command, absolute cwd and announced tool-call ID. No permission capability flag. [Approval tests][test-interactions], [request construction][interaction-code]. |
| `allow_once`, `reject_once`; selected/cancelled outcomes | Implemented and supported | Exact offered IDs resolve to allow/reject kinds. Denial, unknown/malformed outcome or ID, cancellation, timeout and request errors never authorize work. [Fail-closed tests][test-interactions], [wire cleanup][test-stdio]. |
| `allow_always`, `reject_always` | Implemented and supported | Client-owned persistence only. Both allow kinds approve this invocation; both reject kinds refuse it. Bee caches nothing and later ask-routed calls still request permission. `native_routing_and_snapshot_reads_preserve_client_owned_always_decisions` in [interaction tests][test-interactions]. |
| Tool-call subjects and broader native coverage | Implemented and supported | Native read/glob/grep/list/write/edit/delete/move/copy and registered custom tools support ask routing. Reads/searches/listing default allow; mutations and custom tools default ask. Correlated pending tool-call subjects include bounded input and trusted presentation. Ordinary/snapshot paths share gating. [Rules and exact mapping](tools.md#native-permission-rules), [interaction tests][test-interactions], [wire tests][test-stdio]. MCP policy remains client-owned. |
| Subjectless permission requests | Not implemented | Every current protected operation has a command or tool-call subject. No production subjectless workflow is claimed. |
| `elicitation/create`, form mode; `client.elicitation.form` | Partially supported | Negotiated `ask_user` form has one required string `answer`, bounded to 4096 bytes. Accept validates data; decline/cancel/invalid becomes tool failure. Richer forms: [#77](https://github.com/noirbizarre/bee/issues/77). [Form tests][test-interactions]. |
| `elicitation/create`, URL mode; `client.elicitation.url` | Partially supported | Configured provider-login flows for missing/expired stored credentials only. Acceptance is not login completion. Successful browser scenarios are Unix-only. [Login tests][test-interactions]; broader workflows: [#77](https://github.com/noirbizarre/bee/issues/77). |
| Provider OAuth through URL elicitation | Implemented and supported | ChatGPT browser login uses standard URL elicitation; subsequent token renewal is provider-side, not `auth/login`. `chatgpt_browser_login_renews_between_native_tool_requests_and_commits_history` in [OAuth boundary tests][test-oauth]. |
| `elicitation/complete` notification | Implemented and supported | Sent only after callback/exchange/credential commit; failures do not falsely report completion. [Login completion tests][test-interactions], [implementation][interaction-code]. |
| Session-scoped elicitation and request/tool correlation | Implemented and supported | Unique login IDs; form tool-call IDs; isolated concurrent sessions. [Correlation/cleanup tests][test-interactions]. |
| Request-scoped elicitation (`requestId`) | Planned | Current flows are session-scoped. [#77](https://github.com/noirbizarre/bee/issues/77). |
| Pending interaction cancellation, timeout and late responses | Implemented and supported | Drop pending work before terminal idle and direct-close response; late replies cannot authorize cancelled work. [Interaction tests][test-interactions], [wire tests][test-stdio]. |
| `auth/login`, `auth/logout`; agent auth methods | Not planned | No `authMethods` advertised or auth RPC handlers. Provider authentication/`bee auth` is separate, including issues #53–55. [Initialization/handlers][acp-code]. |
| Terminal auth methods / `client.auth.terminal` | Not planned | No advertised interactive terminal-auth invocation. This is neither browser URL elicitation nor terminal tool display. [Initialization][acp-code]. |

## MCP and optional experimental surfaces

Authority: [V2][v2-schema] `McpServer`, `McpCapabilities`; [V2 experimental][v2-unstable]
`MessageMcpRequest`, `MessageMcpResponse`, `SessionCapabilities`, `ProvidersCapabilities`,
`PlanUpdateContent`, `SessionUpdate`, `StateUpdate`, `IdleStateUpdate`;
[experimental method inventory][v2-experimental-methods].
MCP operations below are inner MCP functionality carried by ACP, not extra standard ACP RPC methods.

### MCP transport and operation boundaries

| Feature | Version | Status | Behavior, limitation and evidence / tracking |
| --- | --- | --- | --- |
| ACP attachments / `session.mcp.acp` | V2 experimental | Implemented and supported | Client-hosted endpoints, discovered per prompt and rebound on resume (including empty replacement); removed on close. [Production MCP tests][test-mcp], [implementation][mcp-code]. |
| `mcp/message` request carrier | V2 experimental | Partially supported | Bee sends requests to client endpoints; no Bee-hosted MCP endpoint or incoming MCP service. Unique connection-scoped IDs and protocol metadata; inner result/error JSON preserved. [Production tests][test-mcp], [envelope unit tests][mcp-code]. |
| `mcp/message` notifications | V2 experimental | Planned | No incoming catalog invalidation/subscription notifications handled yet. Per-prompt rediscovery is not subscription support. [#82](https://github.com/noirbizarre/bee/issues/82). |
| Direct `stdio` / `session.mcp.stdio` | V2 | Planned | Declarations currently rejected; no local server launches. [#70](https://github.com/noirbizarre/bee/issues/70). |
| Direct `http` / `session.mcp.http` | V2 | Planned | Declarations currently rejected; no direct HTTP MCP connections. [#70](https://github.com/noirbizarre/bee/issues/70). |
| `tools/list` / `tools/call` | MCP over ACP | Implemented and supported | Pagination/cycle detection, stable aliases, offline argument-schema validation, complete JSON envelopes and deadlines. Bounds: 32 servers, 64 pages/server, 256 tools, 1 MiB catalog, depth 32. [Production tests][test-mcp], [limits/code][mcp-code]. |
| Resources: listing/reading/templates/subscriptions | MCP over ACP | Planned | Not currently exposed; [#82](https://github.com/noirbizarre/bee/issues/82). |
| Prompts: listing/retrieval | MCP over ACP | Planned | Not currently exposed; [#82](https://github.com/noirbizarre/bee/issues/82). |
| Catalog-change notifications | MCP over ACP | Planned | Tools/resources/prompts live invalidation tracked in [#82](https://github.com/noirbizarre/bee/issues/82). |
| Interactive `input_required` / task continuation | MCP over ACP | Planned | Current executor rejects non-complete outcomes explicitly; [#82](https://github.com/noirbizarre/bee/issues/82), [failure tests][test-mcp]. |
| Sampling and roots | MCP over ACP | Not planned | No handlers or capabilities advertised. Do not infer support from using MCP tools. [MCP request metadata][mcp-code]. |

The client currently owns MCP configuration/orchestration. [#19](https://github.com/noirbizarre/bee/issues/19)
proposes Bee-owned project-local definitions; that separate future configuration proposal is not an ACP capability
or a currently supported convention. Direct attachment transport work is tracked explicitly in #70.

### Other experimental features

| Feature | Status | Behavior, limitation and evidence / tracking |
| --- | --- | --- |
| `session/fork` / `session.fork` | Planned | No fork handler/capability; durable resume is not a fork. [#76](https://github.com/noirbizarre/bee/issues/76). |
| Client `systemPrompt`: append / override | Not planned | Neither `session.systemPrompt` nor `.mode.override` advertised. Scoped AGENTS.md and skills are separate Bee instruction sources. [Initialization][acp-code]. |
| `providers/list`, `providers/set`, `providers/disable` / root `providers` | Not planned | No management handlers or capability. Provider/model selects do not permit endpoint/credential editing. [Handler/option code][acp-code]. |
| Markdown and file-backed `plan_update` representations | Not planned | Only item plans produced; no `markdown` or `file` representation. [Plan implementation][plan-code]. |
| `plan_removed` | Not planned | No removal notifications; clearing item entries uses an empty full snapshot. [Plan tests][test-plans]. |
| `subagent_update`, child mutation/cancel capabilities | Not planned | No standard child-session exposure. Outbound client/delegation issues [#20](https://github.com/noirbizarre/bee/issues/20)/[#21](https://github.com/noirbizarre/bee/issues/21) are not commitments to this experimental representation. |
| `session_message`, `session_message_chunk`, session-directed prompts | Not planned | No standard inter-session message presentation/routing. [Current session handlers][acp-code]. |
| End-of-work `idle.usage` token totals | Not planned | No total/input/output/thought/cache token counters on terminal state. Distinct from supported estimated `usage_update` and planned monetary cost. [Finalization][acp-code]. |
| `state_update.state: unknown` | Partially supported | Schema can represent it, but Bee always supplies a known running/requires_action/idle snapshot; it does not need the unknown-state producer fallback. [Resume/finalization][acp-code]. |

## ACP v1 inventory and migration boundary

Authority: [V1][v1-schema], [V1 experimental][v1-unstable], their complete
[baseline][v1-methods]/[experimental][v1-experimental-methods] method inventories, and [Migration][spec-migration].
**Bee does not intend to implement wire-v1 interoperability.** Shared method names or Rust types do not make
the following v1 surfaces available. V2 support described above applies only to the v2 shapes and lifecycle.

| V1 feature / capability | Status | Disposition and v2 boundary |
| --- | --- | --- |
| `initialize`; v1 identity/capability structure | Not planned | The SDK's v2 response to a v1 version field is not a v1 connection. [Negotiation test][test-acp]. V1 `agentCapabilities`/boolean markers differ from v2 object capabilities. |
| `session/new`, `session/prompt`, `session/cancel`, `session/update` | Not planned | No v1 lifecycle. V1 prompt returns final `stopReason`; v2 acknowledges acceptance and later publishes terminal state. |
| `session/list`, `session/resume`, `session/close`, `session/delete`, `additionalDirectories` | Not planned | V1 optional `sessionCapabilities` markers are not advertised. V2 equivalents have their own statuses above. |
| `session/load` / `loadSession` | Not planned | No v1 history-load method. V2 start-resume replay is the implemented replacement. |
| `session/set_mode`, response `modes`, `current_mode_update` | Not planned | No dedicated v1 mode API. V2 configuration options replace this surface. |
| `session/set_config_option`, client `session.configOptions` | Not planned | V1 capability-gated configuration delivery is not implemented; v2 selectors/config updates are supported without this v1 gate. |
| `authenticate`, `logout`, auth methods / terminal-auth capability | Not planned | No v1 agent-auth API. Provider credentials remain separate in either version. |
| `fs/read_text_file`, `fs/write_text_file`; client `fs` capabilities | Not planned | No client filesystem delegation. Native tools execute in Bee's host context. |
| `terminal/create`, `terminal/output`, `terminal/wait_for_exit`, `terminal/kill`, `terminal/release`; client `terminal` | Not planned | No delegated terminal execution; removed from v2. Implemented v2 terminal updates are display-only. |
| `session/request_permission`; `elicitation/create`, `elicitation/complete` | Not planned | No v1 interaction boundary. V2 command subjects and negotiated elicitation use the v2 schemas. |
| `$/cancel_request`, JSON-RPC error/transport behavior on v1 | Not planned | Shared SDK machinery does not establish a tested Bee v1 endpoint. V2 request cancellation is assessed separately. |
| `promptCapabilities`: image/audio/embeddedContext; text/resource links | Not planned | No v1 prompt ingestion. Planned #22 is for Bee's v2 prompt boundary. |
| `mcpCapabilities`: stdio/http/sse; experimental acp | Not planned | No v1 MCP attachment support; legacy SSE is absent from v2. V2 direct stdio/HTTP work is #70. |
| `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk` | Not planned | No v1 message/thought emission; v2 adds full message upserts and changed lifecycle semantics. |
| `tool_call`, `tool_call_update`; content/diff/terminal references | Not planned | No v1 tool presentation. V2 uses upserts/chunks and different structured diff/terminal shapes. |
| `plan`, `available_commands_update`, `session_info_update`, `usage_update` | Not planned | No v1 plan/command/metadata/usage emission. V2 item plans use `plan_update` with `planId`. |
| `notice`, `compaction_update`, `compaction_summary_chunk`; client `session.notices`/`.compaction` | Not planned | V1 capability-gated variants are not emitted; v2 notices/compaction do not use these client gates. |
| Experimental `plan_update`, `plan_removed`; client `plan` | Not planned | No v1 identified-plan extension, including its markdown/file representations. |
| Experimental `session/fork`, `systemPrompt`, `providers/*`, `mcp/message` | Not planned | No v1 optional handlers/gates. V2 fork/MCP plans do not imply v1 support. |
| Experimental `subagent_update`, `session_message`, `session_message_chunk`, client `subagents`, end-turn usage | Not planned | No v1 subagent/messaging/usage additions; also not planned on v2 as described above. |
| V1 `_meta`, custom methods and enum variants | Not planned | No Bee v1 extension product or v1 compliance claim. Required v2 forward compatibility remains a separate correctness obligation. |

## Maintaining the matrix

When changing SDK/schema pins, advertised capabilities, protocol handlers/emissions, retention or their tests:

1. Re-enumerate both versions' method metadata and schema capability/content/update unions, including unstable additions.
   Record the immutable specification revision and audit date; explicitly assess any newer normative differences.
2. Check every row against production code and named tests. Keep current behavior separate from roadmap and client UI.
   A partial feature stays partial until the remaining behavior has evidence. Unknown status requires an identified
   verification gap, not an assumption of rejection or a hidden product decision.
3. Link each planned addition to a matching issue with concrete scope; update the row when that work ships.
   Revisit not-planned decisions explicitly rather than treating every optional protocol feature as a commitment.
4. Run the documented checks below. Record TCK/client results with their own versions, dates, tested Bee revision and
   exercised paths; preserve historical runs as historical. A passing subset never establishes full conformance.

Code/test evidence links pin the refreshed Bee audit revision, so they work in the repository and on the published site.
Update those links with the audit provenance when rechecking implementation claims; specification links remain immutable.

[v1-schema]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v1/schema.json
[v1-unstable]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v1/schema.unstable.json
[v1-methods]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v1/meta.json
[v1-experimental-methods]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v1/meta.unstable.json
[v2-schema]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v2/schema.json
[v2-unstable]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v2/schema.unstable.json
[v2-methods]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v2/meta.json
[v2-experimental-methods]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v2/meta.unstable.json
[spec-transport]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/transports.mdx
[spec-extensions]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/extensibility.mdx
[spec-lifecycle]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/prompt-lifecycle.mdx
[spec-migration]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/migration.mdx
[sdk-transport]: https://docs.rs/crate/agent-client-protocol/3.5.0/source/src/jsonrpc/transport_actor.rs
[acp-code]: https://github.com/noirbizarre/bee/blob/b8c3eeb/src/acp.rs
[interaction-code]: https://github.com/noirbizarre/bee/blob/b8c3eeb/src/acp/interactions.rs
[mcp-code]: https://github.com/noirbizarre/bee/blob/b8c3eeb/src/acp/mcp.rs
[plan-code]: https://github.com/noirbizarre/bee/blob/b8c3eeb/src/acp/plans.rs
[test-acp]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/acp.rs
[test-stdio]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/stdio.rs
[test-session]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/session.rs
[test-cancel]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/cancel.rs
[test-selection]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/selection.rs
[test-models]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/models.rs
[test-e2e]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/e2e.rs
[test-interactions]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/interactions.rs
[test-plans]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/plans.rs
[test-mcp]: https://github.com/noirbizarre/bee/blob/b8c3eeb/tests/mcp_integration.rs
[test-oauth]: https://github.com/noirbizarre/bee/blob/b8c3eeb/src/acp/oauth_tests.rs

## Automated coverage

Paths below are relative to the repository root.

| Behavior | Repeatable coverage |
| --- | --- |
| Initialization, identity, version negotiation, exact capabilities | `tests/acp.rs`, `tests/stdio.rs`, `tests/e2e.rs` |
| Session new/list/close/resume, cwd checks, invalid IDs and settings | `tests/session.rs`, `tests/selection.rs`, `tests/stdio.rs` |
| Prompt acceptance, echoed message IDs, running/text/terminal ordering | `tests/session.rs`, `tests/e2e.rs` |
| Itemized plans, full replacement, statuses, cancellation/failure, memory/durable replay | `tests/plans.rs`, across all three inference protocols |
| Tool lifecycle, five diff operations, directory guidance, raw terminal bytes, active/idempotent memory/durable replay | `tests/tool_presentation.rs`, all three inference protocols for native mutation cycles; `tests/mcp_integration.rs` for MCP |
| Exact tool/diff/terminal wire shapes and process-restart tool snapshots | `tests/stdio.rs`; bounded reduction, patch applicability, mutation races, partial cancellation and storage faults in module tests |
| Exact serialized plan notifications, process restart and interrupted recovery without reexecution | `tests/stdio.rs`; storage faults and replay budgets in history unit tests |
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
| Native permission routing, four choices, snapshot reads, repeated always decisions, fail-closed responses | `native_routing_and_snapshot_reads_preserve_client_owned_always_decisions` and `only_an_offered_bash_approval_can_create_a_file` in `tests/interactions.rs`; ordered policy/project tightening in `tests/permissions.rs` |
| Serialized command/tool-call subjects, malformed/unknown/error/reject responses and close cleanup | `unapproved_stdio_commands_never_run_and_close_follows_request_cleanup` and `every_file_change_operation_is_serialized_as_standard_v2_diff_content` in `tests/stdio.rs` |
| Independent form/URL negotiation, validated form responses, model-call correlation | `tests/interactions.rs`, across all three inference protocols |
| Real provider browser callback/exchange/store workflow; URL acceptance distinct from completion | `tests/interactions.rs`, with an injected credential exchange transport |
| Login transport HTTPS-only policy, redirect refusal, bounded/framed bodies, cancellation and deadlines | Unit tests in `src/http.rs` |
| Whole browser-login deadline during URL create, callback wait and exchange; cleanup and retry | Virtual-time cases in `tests/interactions.rs` |
| Pending permission/form/URL cancellation, direct close, late replies and session isolation | `tests/interactions.rs`; cleanup-before-idle assertions in client handlers |
| Omitted/null/start resume, unknown/malformed cursor rejection, snapshot-before-response and retained model context | `tests/session.rs`, `tests/stdio.rs`, `tests/cancel.rs`, `tests/interactions.rs`, `tests/inference.rs` |

Browser-login success, concurrency and cancellation scenarios in `tests/interactions.rs`
are Unix-only; the CI platforms do not all execute the same login matrix.

### Hive verification handoff

Bee's plan tests consume only native ACP `PlanUpdate` messages, with no special
parsing. The subprocess case
`standard_plan_messages_survive_process_restart_without_reexecuting_model_work`
provides a deterministic real-Bee producer and validates the exact wire payload.
A real Hive gateway integration test remains a Hive-side verification step:
launch Bee with a local provider returning `update_plan` calls, observe initial
and reordered/status-revised snapshots under the same `planId`, and verify the
gateway replaces the native plan and restores its terminal snapshot on start
resume. Consume `session/update` directly; do not parse assistant text or
`_hive_activity`. Bee's test suite does not claim to execute the Hive gateway.

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
cargo test --test e2e --test stdio --test acp --test session --test cancel --test inference --test mcp_integration --test interactions --test plans --test tool_presentation --test selection
mise run ci
```

The existing Linux/macOS/Windows CI nextest matrix discovers these integration targets
automatically. `mise run ci` also checks formatting, Clippy, spelling, Markdown,
workflows and the documentation build. The separate project-owned
`.github/workflows/compatibility.yaml` runs the pinned TCK on Linux for pull requests
and main pushes, fails on TCK failure, and uploads its JSON requirement/transcript report.
On 2026-10-25, `mise run ci` passed locally on Linux: **736 Rust tests**, formatting,
Clippy, spelling, workflow/Markdown linting and the documentation build.
The 2026-10-26 audit reran `mise run ci` on Linux against the original `7cccee0` implementation plus these documentation
and comment changes: **834 Rust tests passed**, with formatting, Clippy, spelling, workflow/Markdown linting
and the documentation build also passing.

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

### #71 implementation verification: 2026-10-26

On Linux, against the #71 implementation working tree on `feat/acp-tool-lifecycle`:

These test runs preceded the submission-review corrections for encoded patch bounds, executable deletion
modes, and Windows directory unlinking. The corrections and added regressions were compiled/linted;
the test suites were not rerun during submission, as requested by the maintainer.

- `mise run ci` passed: **983 tests**, formatting, Clippy, spelling, workflow/Markdown linting,
  and the documentation build.
- `cargo test -j 2` passed: **986 tests including 3 documentation tests**.
- `mise run test:compatibility` passed: **86 passed, 39 skipped, 0 failed** using the pinned TCK above.
  Its fixture still returns text-only answers, so tool/terminal/permission scenarios remain outside
  that run's evidence. Bee's production and serialized-wire suites provide those assertions.
- Isolated production tree/patch modules type-checked for Windows MSVC and macOS ARM64 using a
  lightweight harness without TLS dependencies. Full cross-target checks were blocked in the existing
  `aws-lc-sys` C build by this Linux host's missing platform C toolchains; this is not a native-platform test run.

The new evidence includes all five committed diff operations, directory guidance before side effects,
native/MCP lifecycle correlation, raw terminal bytes and active replay, cancellation/close ordering,
idempotent memory/durable replay, restart recovery, mutation races/partial deletion, and storage failure
after execution without publishing uncommitted results. Test names and exact supported subsets are recorded
in the tool/terminal rows above. This does not establish complete ACP conformance or a real Hive UI run.

### Recorded result: 2026-10-26

The pinned runner was rerun on Linux against Bee commit `7cccee06a715b0711d3b522dfecd3bf0e1fe73c1`
plus the #59 documentation/comment changes: **86 tests passed, 39 skipped, 0 failed**.
TCK `0.2.0` reported **CONFORMANT** for its pinned requirement set, not complete ACP conformance.

| Tier | Passed | Failed | Skipped |
| --- | --- | --- | --- |
| Mandatory | 21 | 0 | 3 |
| Capability | 41 | 0 | 24 |
| Advisory | 15 | 0 | 4 |
| Informational | 10 | 0 | 7 |

Retained-message identity (`ACP-PATCH-203`) now passes, unlike the historical pre-durability run below.
Incoming batch handling and unknown initialization fields also pass their pinned requirements.
The fixture still does not produce permission, elicitation, tool-update, terminal or plan scenarios;
those paths are skipped or outside this TCK run's coverage. Bee's own suites cover its implemented interactions
and item plans. The newer specification's unknown-content fallback requirement is not established by this
older TCK's passing extensibility checks. The current matrix above records Bee's implemented unknown-content
preservation separately from this historical TCK result.
The local JSON report was written to `/tmp/bee-acp-tck.json`; it can be regenerated with the command above.

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
