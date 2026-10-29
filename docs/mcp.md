# Project-local MCP servers

Bee discovers project MCP definitions in `.agents/`, the canonical project-local
directory for agent resources. `.agents/skills/` remains reserved for standard
Agent Skills; MCP definitions are configuration, not skills or instructions.

## Discovery and filenames

The project root is the nearest ancestor of the session cwd containing a `.git`
directory or worktree marker file. Without one, the session cwd is the root.
Bee reads:

1. `<root>/.agents/mcp.toml`, containing all project servers. Each `servers.KEY`
   determines a stable server identifier.
2. `<root>/.mcp.json` as a Claude compatibility fallback **only when the canonical
   TOML file is absent**. Its envelope is `mcpServers`.

Duplicate identifiers are errors. An empty canonical file suppresses fallback;
malformed or unreadable canonical configuration never triggers fallback.
`.agents/mcp.json` is not a Bee configuration source. Identifiers use 1–128 ASCII
letters, digits, `.`, `_` or `-`.

The selected source must be a regular UTF-8 file resolving inside the project root,
with at most 32 combined project/client servers and 1 MiB of source text.
Config symlinks escaping the root are rejected.
Checks do not isolate against concurrent filesystem replacement.

## TOML configuration

For example, commit `.agents/mcp.toml` with references to credentials:

```toml
[servers.files]
type = "stdio"
command = "uvx"
args = ["example-mcp-server"]
cwd = "."

[servers.files.env]
SERVICE_TOKEN = { env = "SERVICE_TOKEN" }
MODE = "readonly"

[servers.docs]
type = "http"
url = "https://mcp.example.test/mcp"

[servers.docs.headers]
Authorization = { env = "DOCS_MCP_AUTHORIZATION" }
```

All servers belong in this file under the `servers` envelope. Transport type is
required. Unknown fields and fields belonging to another transport are errors.

Stdio commands are executables, not shell commands. Bee resolves bare names on
its injected PATH before approval; relative executable paths and `cwd` resolve
against the project root. Default cwd is the project
root. Arguments are literal strings. No shell, tilde, command substitution or
dotenv expansion occurs.

Children receive only configured environment entries plus a platform baseline:
`PATH`, `HOME`, `USERPROFILE`, `TMPDIR`, `TMP`, `TEMP`, `SystemRoot`, `WINDIR`,
`COMSPEC` and `PATHEXT`, when set. Bee does not automatically forward provider
credentials. Explicit entries replace baseline entries. Child stderr is discarded.

Environment/header values are literal strings or whole-value `{ env = "NAME" }`
references. Export the complete header value, for example `Bearer ...`, rather
than embedding a secret in the project file. Missing/empty referenced variables
fail with a value-free diagnostic. Environment references are resolved before
consent, and the approved effective definition is frozen for execution.

HTTP endpoints require HTTPS, or HTTP on localhost/a loopback IP, with no URL
userinfo or fragment. Redirects are disabled. Header names are case-insensitive;
duplicates, invalid headers and protocol/routing overrides are errors. Put
credentials in explicit headers, not endpoint URLs. Bee does not reuse LLM
credentials or perform MCP OAuth/browser login.

## Claude fallback

Bee accepts the command/args/env and Streamable HTTP URL/headers subset:

```json
{
  "mcpServers": {
    "docs": {
      "type": "http",
      "url": "https://mcp.example.test/mcp",
      "headers": { "Authorization": { "env": "DOCS_MCP_AUTHORIZATION" } }
    }
  }
}
```

In this compatibility format only, omitted `type` is inferred from `command`
or `url`. Bee's explicit env-reference object is supported in JSON too; Claude
string interpolation is not performed. Duplicate JSON keys, unsupported fields
and legacy SSE transport definitions fail explicitly.

## Client attachments and identity

Clients can also declare ACP-hosted, direct stdio or Streamable HTTP servers in
`session/new` and `session/resume`. Direct stdio attachments require an absolute
executable path and use session cwd. Their environment follows the same baseline
policy. Validation never launches or connects.

Project keys, client ACP routing IDs, and client direct `(transport, name)` pairs
are distinct identities. Duplicate IDs within a namespace fail. Same-named
project and client servers coexist; neither replaces the other. Labels distinguish
`project/NAME`, `client/acp/NAME`, `client/stdio/NAME` and `client/http/NAME`.
Tool aliases include source identity and the original tool name, never array order.

## Consent and lifecycle

Project files are untrusted. Reading configuration, creating sessions and replaying
history never starts a server. On the first prompt, Bee asks before launching a
process or making any HTTP request, including initialization and tool discovery.
Connection approval does not approve tool calls: each direct MCP call separately
defaults to ask after argument-schema validation.

These actions use the existing [permission rules](tools.md#native-permission-rules):

| Action | Resource | Default |
| --- | --- | --- |
| `mcp.launch` | Source-qualified server label, e.g. `project/files` | Ask |
| `mcp.connect` | Source-qualified server label, e.g. `client/http/docs` | Ask |
| `mcp.call` | Source-qualified server label plus `/TOOL` | Ask |

Trusted global/`BEE_CONFIG` rules can allow or deny; launch-project rules can only
tighten policy. MCP definition files cannot grant themselves permission. Startup
consent uses the `_bee_mcp_connection` permission-subject extension, with server,
transport, safe target, session scope and an opaque revision. Clients must display
or handle this subject to approve; missing, invalid or rejected decisions fail
closed. Tool calls use correlated standard tool-call subjects, with `bee/mcp`
metadata binding server identity and revision. Always decisions remain client-owned.
Client-hosted ACP MCP retains its existing client/server-owned authorization policy.

Healthy approved connections are reused within the session, but tools are listed
again each prompt. Configuration is re-read at prompt boundaries and resume;
changed/removed definitions retire old connections and replacements need fresh
connection consent. Invalid reload fails closed. A resume that changes active
direct connections cancels and drains the old turn before replacement. An empty
client attachment list clears client attachments, not project definitions.

Cancellation/timeout closes uncertain direct connections before terminal idle;
untouched healthy connections can remain open. Close drains all session connections
before idle/response, and disconnect drains connection-owned runtimes. Stdio uses
process groups on Unix and jobs on Windows, with kill/reap cleanup. Connections
and approvals are never durable history and do not survive restart. Remote
cancellation and connection closure cannot undo side effects. Bee never
automatically retries tool POSTs, including after HTTP session expiration.

## Supported MCP subset

Direct transports use pinned rmcp 3.7.0 and propose MCP `2025-11-25` initialization,
accepting the SDK's historical `2024-11-05`, `2025-03-26` and `2025-06-18`
negotiation results. HTTP means Streamable HTTP, including JSON and SSE response
bodies; it does not mean the legacy SSE endpoint transport. ACP-hosted routing
continues to use its separate `2026-07-28` MCP-over-ACP metadata contract.

Only `tools/list` and completed `tools/call` are exposed. Sampling, roots,
resources, prompts, subscriptions, task/interactive continuation and OAuth are
not advertised. Results preserve complete result/error JSON envelopes, including
unknown result/error fields, explicit nulls and metadata. Non-text content stays
JSON rather than becoming provider-native multimodal input.

Direct ingress bounds JSON frames/bodies and SSE events to 1 MiB before typed
SDK decoding. Shared catalog limits are 64 pages/server, 256 tools, depth 32 and
1 MiB aggregate results. Initialization has a 30-second deadline, catalog discovery
30 seconds total, and each call 120 seconds. These are not a schema-compilation
sandbox. See [tool execution](tools.md#client-hosted-mcp-tools) for common bounds.

Embedded ACP runtimes opt in with `Runtime::with_project_mcp(environment)`.
Normal `bee` serving enables project discovery; deterministic scenario serving
does not read project configuration.
