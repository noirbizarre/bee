# Getting started

Bee is the agent subprocess; your client supplies the interface. You need a client implementing the documented
**draft ACP wire-v2** lifecycle over stdio. Wire-v1 interoperability is not implemented. Consult the
[compatibility matrix](compatibility.md) for pinned test evidence and capability requirements; it does not certify
every ACP client or a particular Hive release.

## 1. Install Bee

Follow [installation](installation.md), then check `bee --version` and `bee --help` on the machine running the agent.
The client must be able to find that executable; use its absolute path if the client's PATH differs from your shell's.

## 2. Configure an account and model

Create `.config/bee.toml` inside the directory from which the client will launch Bee:

```toml
model = "openai/YOUR_RESPONSES_MODEL_ID"

[accounts.openai]
provider = "openai"
discovery = false
models = ["YOUR_RESPONSES_MODEL_ID"]
credential = { env = "OPENAI_API_KEY" }
```

Replace both model placeholders with the same exact Responses-capable model ID available to your API account. The root
default is account-qualified; the account's availability list uses raw wire IDs. With discovery off, only declared models
are available. Without a default or usable recency, choose a model through ACP, even for a sole account/model. The OpenAI
provider prefers Responses. This route uses OpenAI API billing, not a ChatGPT subscription. For other providers,
protocols or local endpoints, see [configuration](index.md#providers-and-models) and [inference](inference.md).

Alternatively, save the file globally at `~/.config/bee/config.toml`, or set `BEE_CONFIG` to its absolute path.
The project file overrides both; see [configuration precedence](index.md#configuration). A fresh profile uses these
defaults; existing profiles may prefer an account-bound [last-used model](profiles.md#configured-default-and-last-used-model).
Explicit client model selection takes precedence.

Supply `OPENAI_API_KEY` through your client's agent environment or launch the client from an environment that already
contains it. The value must reach the **Bee subprocess**, not just your terminal. Do not put the key in TOML, command
arguments or committed client settings. `bee auth status openai`, run with the same environment/configuration, checks
local credential availability without printing the key; it does not validate account entitlement or model access.

For supported Unix browser-login alternatives, see [OpenRouter](index.md#openrouter-browser-authentication),
[ChatGPT](index.md#use-your-chatgpt-plan) and [Copilot](index.md#use-github-copilot).

## 3. Configure the client launch

These are launch fields to map to your client's own settings, not a client-specific configuration schema:

| Field | Value |
| --- | --- |
| Command | `bee`, or its absolute executable path |
| Arguments | Empty for normal inference |
| Working directory | The workspace containing `.config/bee.toml` |
| Environment | `OPENAI_API_KEY` supplied securely; optionally absolute `BEE_CONFIG` and a configured `BEE_PROFILE` |
| Transport | stdin/stdout, with stderr captured separately for diagnostics |

The client initializes the connection, creates a session with the desired session `cwd`, and sends a text prompt.
Try “Reply with a short greeting.” Expect streamed text followed by terminal idle/end-turn. Then ask a follow-up to
check retained context. Bee's launch directory controls project configuration; session `cwd` controls native tool work.

The client must handle permission requests for Bash and mutations under the default policy. Form and URL elicitation
are optional negotiated capabilities. MCP servers are configured and hosted by the client, then attached through ACP.

Running bare `bee` in a terminal waits for protocol input; there is no interactive `bee prompt` command. Closing stdin
ends the connection successfully. For a credential-free client integration check, launch `bee --scenario success`;
this exercises a deterministic protocol scenario, not provider inference. See [scenarios](scenarios.md).

## Known limitations

- Bee supports the documented draft wire-v2 subset, not wire-v1 or every proposed ACP feature. See [compatibility](compatibility.md).
- Inference supports text and function tools. Resource links remain references, not fetched content; see
  [unsupported inference features](inference.md).
- Native tools run on the host with Bee's privileges; there is no sandbox. Bash must be installed for the Bash tool.
  Review [tool execution and permissions](tools.md) for defaults and bounds.
- Bee does not launch MCP servers itself or implement remote MCP OAuth. Use [client-hosted ACP MCP attachments](tools.md).
- The private credential store is currently Unix-only. Environment-backed API credentials and explicit Copilot tokens
  work independently of it; ChatGPT-plan OAuth requires the store.
- Durable history is private but unencrypted, with no automatic disk expiry. See [session storage and retention](sessions.md).
- Bee has no bundled UI or orchestration service. Cancellation ends execution but does not undo committed tool effects.

If launch or inference fails, consult [CLI troubleshooting](cli.md#troubleshooting) and the client's captured stderr.
