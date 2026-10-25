# bee

<p align="center">
  <img src="images/logo.svg" alt="Bee" style="max-width: 100%; height: auto;">
</p>

Bee is a lightweight, headless agent runtime written in Rust that implements ACPv2. It provides the core agentic loop
and LLM provider integration, allowing it to work with any compatible ACPv2 client.

## Status

Bee is at the **bootstrap stage**: the repository, tooling, CI and release automation exist, and the `bee` binary
serves ACPv2 over stdio, so a client can start it, complete the `initialize` handshake, create sessions and submit
prompts. Bee advertises the baseline session lifecycle (`session/new`, `list`, `resume`, `close`, `prompt`, `cancel`).

**Chat Completions, Responses and Anthropic Messages inference are implemented.** Selected profiles receive prompts and
stream answers over ACP. Cancellation and close interrupt pending inference. Production sessions retain successful
context across close/resume and process restart. Start resume replays the separate visible transcript before current
state. See [session history](sessions.md) for storage, retention and client restoration.
Cancelled, failed and refused turns do not enter model context.
Prompts accept text and resource links, represented as references without fetching their contents.

The bounded SDK-free loop supports injected function executors. Normal ACP sessions advertise and execute the native
`bash`, `read`, `glob`, `grep`, `list`, `write`, and `edit` tools across all three backends,
using each session's working directory.
Client-hosted MCP tools are discovered and invoked through ACPv2's `session.mcp.acp` transport alongside the native
catalog. The client supplies server attachments and enforces permissions. See [tools](tools.md) for the contracts,
execution context and supported MCP subset.
Native Bash calls request client permission. Form-capable clients also receive
`ask_user`; URL-capable clients can perform provider browser login. See
[ACP user interactions](tools.md#acp-user-interactions) for capability and outcome handling.

See [ACPv2 compatibility](compatibility.md) for the version baseline, automated workflow
coverage, official TCK results and outstanding protocol gaps.

Bee loads project and nested `AGENTS.md` instructions, additive `~/.agents/AGENTS.md`, and standard Agent Skills from
project/user `.agents/skills` directories. Additional skill roots use `[skills] extra_roots = ["/opt/team/skills"]` in the
normal configuration hierarchy. See [project instructions and skills](instructions.md) for scope, precedence and loading.

## Configuration

Bee reads TOML configuration from up to three files, merged in this order, lowest precedence first:

1. **Global:** `$XDG_CONFIG_HOME/bee/config.toml`, or `~/.config/bee/config.toml` when `XDG_CONFIG_HOME` is unset. The
   same on every platform, macOS included.
2. **`BEE_CONFIG`:** the file named by that environment variable, for settings chosen per client or workspace.
3. **Project:** `.config/bee.toml` in the directory Bee is launched from. Parent directories are not searched.

The global and project files are optional. A `BEE_CONFIG` file that does not exist is an error, and so is any file
that exists but cannot be read or parsed: Bee exits before speaking the protocol and says which file and, for syntax
errors, which line and column. Diagnostics never repeat the contents of a file, since they may hold API keys.

Tables merge key by key, and for a scalar the higher layer wins. An array in a higher layer **replaces** the lower one
instead of extending it. Giving one key different types in two layers (a string over an integer, say) is an error that
names the key and both files. See [ADR-006](adr/006-layered-xdg-configuration.md).

### Agent execution

```toml
[agent]
max_steps = 16        # model requests per turn, including the final response
max_tool_calls = 64   # individual tool calls across all batches in a turn
```

Both settings are positive integers and merge independently across configuration layers.
Tools execute sequentially; a batch exceeding a budget is rejected before execution.
See [turn execution limits](inference.md#turn-execution-limits) for boundary behavior.

### Providers and models

Optional [model discovery](models.md) enriches ACP selectors and `bee models` with Models.dev metadata and separately
enabled endpoint listings. Catalog refresh is automatic and nonblocking; configured model IDs always work independently.

A provider is a named connection profile, separate from the protocols it speaks and from the model:

```toml
provider = "work"                  # the default profile (optional when there is only one)

[providers.work]                   # any name; it is a label, never looked up
model = "corp/ft-2026"             # optional default model: any id, listed anywhere or not
models = ["corp/base"]             # optional suggestions for model selectors, not a limit
credential = { env = "GATEWAY_KEY" }           # optional: names where the secret lives, see Authentication
headers = { "X-Team" = "infra" }   # optional, sent only to this profile
timeout_ms = 120000               # optional whole-request deadline, including streaming
max_output_tokens = 4096          # optional output limit
context_window = 32768            # optional explicit context capacity; no default
model_context_windows = { "corp/base" = 16384 } # optional exact-model capacity overrides

# `protocols` is required: one table per protocol the provider speaks: "openai-chat", "openai-responses" or
# "anthropic-messages".
[providers.work.protocols.openai-chat]
base_url = "https://gateway.corp.example/v1"   # optional; defaults per protocol
options = { max_tokens_field = "max_completion_tokens" }   # settings only this protocol understands
```

A provider can speak several protocols, each at its own URL and with its own options. Several protocols need `prefer`,
which orders them, most preferred first; Bee does not guess among them. An optional `model_protocols` table maps exact
model IDs to configured protocols. Unmapped IDs use the first endpoint in preference order, unless optional
[catalog protocol hints](models.md#optional-protocol-hints-and-context-budgets) are explicitly enabled.

```toml
[providers.gateway]
prefer = ["anthropic-messages", "openai-chat"]
model_protocols = { "vendor/model.v2" = "openai-chat" }

[providers.gateway.protocols.openai-chat]
base_url = "https://gw.example/openai/v1"

[providers.gateway.protocols.anthropic-messages]
base_url = "https://gw.example/anthropic"
options = { max_tokens = 2048 }
```

Mapping keys are literal, case-sensitive model IDs: quote IDs containing dots, slashes or other TOML punctuation.
There is no prefix matching or vendor inference. Mapping targets must be supported protocols configured on the final
profile, including endpoints inherited from a preset; invalid targets stop Bee at launch. Mappings do not add model
suggestions or restrict which IDs can be selected. Unknown/private models remain usable without Models.dev or another
catalog. `prefer` is still required for multi-protocol profiles because unmapped models need a fallback.

`model_protocols` merges key by key across global, `BEE_CONFIG` and project files, and over presets: a higher-priority
entry replaces the same model ID while retaining other entries. An empty table (`model_protocols = {}`) does not clear
inherited mappings. See [ADR-013](adr/013-per-model-protocol-selection.md).

`context_window` and entries of `model_context_windows` are positive 32-bit token capacities. Exact model entries win
over the profile fallback; otherwise capacity is unknown unless catalog context windows are explicitly enabled.
The model map merges per key across layers and presets,
without clearing inherited entries for an empty table. Before each request, Bee estimates input, reserves the effective
output maximum and drops oldest complete turns when needed. The active turn and its tool pairs remain intact.
Configured sessions report labeled estimates and notices when older context is removed. See
[conversation context and token budgets](inference.md#conversation-context-and-token-budgets) for the estimate formula,
transactional retention and explicit overflow handling.

Several profiles can use the same protocol, so a gateway, a local server and a vendor API are just three profiles.
Layers merge as above: a project file can override one header or the model of a global profile. Unknown keys,
unsupported protocols and malformed URLs, headers or models stop Bee at launch with an error that names the key and
the file and never the value. A `credential` only *names* where the secret lives; do not put a key in the file (see
[Authentication](#authentication)).

When a provider is configured, Bee offers each ACPv2 session a `provider` and a `model` configuration option and
handles `session/set_config_option`. These are session settings and differ from the configuration above: the client
chooses among configured profiles and can name any model id, but cannot change a profile's endpoint, headers or
credential. The provider is the client's choice, else `provider`, else the only profile; the model is the client's
choice, else the profile's `model`, else the first of its `models`. Choosing another provider resets the model, and
settings cannot change while a turn is running. See [ADR-007](adr/007-provider-profiles-and-model-selection.md).

Protocol resolution uses that effective model ID for every prompt, whether selected by the client or supplied by a
default. Changing models resolves the endpoint again, retains portable history and drops native continuation.

Requests use the resolved protocol exactly; failures never trigger a retry on another protocol. `openai-chat`,
`openai-responses` and `anthropic-messages` can run inference, including presets preferring Responses or Messages.
See [Inference and smoke tests](inference.md) for the supported subset and verification instructions.

## Authentication

A profile's `credential` says where its secret comes from. It is separate from the `protocol` (which only decides how
the secret is presented: `Authorization: Bearer` for `openai-chat` and `openai-responses`, `x-api-key` for
`anthropic-messages`) and from the model. Exactly one source, or none:

```toml
credential = { env = "OPENROUTER_API_KEY" }            # a variable in the environment that launches Bee
credential = { store = true }                          # Bee's credential store; store a token with `bee auth login`
credential = { store = true, login = "openrouter" }    # the same, and `bee auth login` can sign in with a browser
# no credential: an unauthenticated endpoint, such as a local server
```

An empty variable counts as unset. A credential that is named but missing or expired is an error that says which
command fixes it; Bee never quietly sends a request without it. Credentials are resolved when a request needs them, not
at launch.

### Presets

Bee ships ready-made profiles, so a well-known provider is one line. A profile names a preset and overlays its own keys:

```toml
[providers.work]
preset = "openrouter"              # protocols and credential come from the preset
model = "anthropic/claude-sonnet-4"
```

Shipped presets, with the variable each reads its key from:

| Preset       | Protocols, preferred first                              | Credential                        |
| ------------ | ------------------------------------------------------- | --------------------------------- |
| `openai`     | `openai-responses`, `openai-chat`                       | `OPENAI_API_KEY`                  |
| `chatgpt`    | `openai-responses`                                      | stored ChatGPT OAuth tokens       |
| `anthropic`  | `anthropic-messages`                                    | `ANTHROPIC_API_KEY`               |
| `openrouter` | `openai-chat`, `openai-responses`                       | stored, with browser sign-in      |
| `opencode`   | `openai-chat`, `openai-responses`, `anthropic-messages` | `OPENCODE_API_KEY` (OpenCode Zen) |
| `deepseek`   | `openai-chat`, `openai-responses`, `anthropic-messages` | `DEEPSEEK_API_KEY`                |
| `google`     | `openai-chat` (Gemini's compatible endpoint)            | `GEMINI_API_KEY`                  |
| `groq`       | `openai-chat`, `openai-responses`                       | `GROQ_API_KEY`                    |
| `mistral`    | `openai-chat`                                           | `MISTRAL_API_KEY`                 |
| `xai`        | `openai-responses`, `openai-chat`                       | `XAI_API_KEY`                     |
| `ollama`     | `openai-chat`, local server                             | none                              |

Scalars and arrays in the profile replace the preset's. `model_protocols`, `model_context_windows`, `headers` and each
protocol's table under `protocols` merge key by key, `prefer` replaces, and `credential` is replaced whole.
Every URL comes from vendor documentation,
and a vendor is listed by the protocols Bee speaks: OpenCode Zen serves its GPT, Grok and Muse models over
`openai-responses`, its Claude models over `anthropic-messages` and the rest over `openai-chat`, and only its Gemini
models (Google's native protocol) cannot be reached. Set exact `model_protocols` entries for models needing a protocol
other than the preset's preference. OpenCode Go uses other endpoints
(`/inference/go`) and needs a profile of its own. The catalogue deliberately has no model lists: name your own
`model`.

Define your own preset, or override a shipped one by name, in a `presets` table of any configuration file:

```toml
[presets.corp]
credential = { env = "CORP_KEY" }

[presets.corp.protocols.anthropic-messages]
base_url = "https://llm.corp.example"

[presets.ollama.protocols.openai-chat]   # overrides one key of the shipped preset
base_url = "http://gpu-box:11434/v1"
```

A browser sign-in is data too. `login = "<name>"` refers to a `login_flows` entry, and you can add one for another
provider whose flow is an authorization code with PKCE that returns an API key (`kind = "pkce-key"`):

```toml
[login_flows.acme]
kind = "pkce-key"
authorize_url = "https://id.acme.example/authorize"
exchange_url = "https://id.acme.example/v2/token"
headless = true                    # the provider can show the code, so `--paste-code` works

[login_flows.acme.authorize]       # the provider's query parameter names
callback = "redirect"
challenge = "chal"
challenge_method = "meth"
state = "st"

[login_flows.acme.exchange]        # its JSON field names, and where the credential comes back
code = "authorization_code"
verifier = "proof"
challenge_method = "meth"
response_key = "api_key"
```

Login flows may only be defined in the global file or the file `BEE_CONFIG` names, never in the project's
`.config/bee.toml`: a flow chooses where the authorization code and key are sent, and a project file can come from a
repository you only cloned. ChatGPT uses a separate `kind = "chatgpt"` mechanism with fixed trusted endpoints;
its OAuth registration, identity verification and token renewal are not configurable PKCE-key exchanges.
Device-code sign-in is unsupported.
See [ADR-009](adr/009-provider-presets-and-login-flows.md).

### Use your ChatGPT plan

The `chatgpt` preset implements OpenAI's official
[open-source/local application flow](https://developers.openai.com/siwc/token-sharing-open-source).
It uses your eligible ChatGPT account/workspace plan or credits for supported Responses requests.
Eligibility depends on OpenAI's current account, workspace, region and policy requirements;
successful identity sign-in alone does not grant inference permission. This documented integration covers
open-source, locally hosted applications. Paid or remotely hosted offerings require OpenAI's
[separate approval path](https://openai.com/form/sign-in-with-chatgpt-interest/).
Use is subject to the applicable [OpenAI terms and policies](https://openai.com/policies).

Configure a named profile in your Bee configuration:

```toml
provider = "personal"

[providers.personal]
preset = "chatgpt"
# Select an available model after login, or configure its exact slug here.
```

```sh
bee auth login personal
bee auth status personal
bee models list personal --endpoint
```

Login displays **Continue with ChatGPT**, opens the browser, and receives authorization at a temporary
`http://127.0.0.1:<port>/auth/callback` listener. `--no-browser` prints the authorization URL for you to open locally.
The callback must reach the Bee host; on a remote machine arrange secure loopback forwarding.
`--with-token` and `--paste-code` are not supported for this profile.
Authorize ChatGPT-plan usage when prompted, then use the account-specific model listing or your ACP client's model
picker to select an available model slug. A declined plan permission is retained as a signed-in, inference-disabled
registration; explicitly repeat `bee auth login personal` to request permission again.

Named profiles are distinct account/workspace labels. Add another profile using `preset = "chatgpt"` for another
registration; email alone does not identify a workspace. Reauthorization of an existing profile must match its
verified account and issued client ID. Bee reuses registration and host identity across restarts and logout.
The Bee data directory defines one host installation; give copied installations or self-hosted VMs distinct host
identities by starting with a fresh Bee data directory and signing in there.

Tokens live in Bee's existing private credential store, with atomic writes and cross-process serialized renewal.
Bee renews near expiry before inference steps and model discovery, including after restart. If refresh is revoked,
expired, or interrupted after server-side rotation, sign in again. Temporary failures preserve stored credentials.
OAuth tokens, codes and PKCE verifiers are excluded from diagnostics and ACP events; authorization URLs omit ID-token
hints. Bee does not read Codex's credential files.

`bee auth logout personal` attempts remote refresh-token revocation and clears local tokens, retaining the account/client
mapping for later sign-in. If revocation cannot be confirmed, Bee says so; disconnect the app in ChatGPT settings.
[Manage usage and access](https://chatgpt.com/settings/usage), including account-wide and app-specific limits.

The existing `openai` preset still uses `OPENAI_API_KEY` and separate API billing. OpenRouter's browser flow still
returns an API key. Bee never silently changes from ChatGPT-plan usage to API-key billing.
See [ChatGPT inference limitations](inference.md#chatgpt-plan-requests) and
[ADR-020](adr/020-chatgpt-plan-authentication.md).

### ACP browser sign-in

During a prompt, a missing or expired stored credential starts browser sign-in when
the profile names a login flow and the client advertises `capabilities.elicitation.url`.
Bee sends session-scoped `elicitation/create` with a unique ID. Accepting the URL
means the browser interaction may proceed, not that authentication succeeded.
Bee waits for the loopback callback, validates state, exchanges the code over HTTPS,
and persists the credential before sending `elicitation/complete` with that same ID.
Credentials and callback/exchange secrets never enter model context.

ACP browser sign-in requires Bee's private credential store, currently supported on
Unix. On Windows, configure an environment credential instead; URL capability alone
does not enable the store.

The whole attempt has a five-minute deadline; credential exchange also has a
30-second transport deadline. Decline, cancel, unknown actions and errors end the
turn without inference. Session cancel/close drops pending requests, listener and
exchange before terminal idle; late responses cannot store a credential. A credential
already committed before cancellation remains stored. Final store commits are
serialized across processes using a stable sibling lock and preserve other providers.

The browser must be able to reach the Bee host's loopback callback. URL capability
does not imply that a remote client shares that host; use `bee auth login` on the
agent host when it does not. Missing environment variables, invalid/insecure stores,
missing login flows and unsupported URL mode remain configuration errors with CLI
guidance. A provider's inference 401/403 does not automatically retry login or inference.

### Commands

```bash
bee auth login [PROFILE]                 # browser sign-in for OpenRouter or ChatGPT profiles
bee auth login [PROFILE] --paste-code    # no callback: open the URL anywhere, paste the code it shows (SSH, containers)
bee auth login [PROFILE] --with-token    # read an API token from standard input
bee auth logout [PROFILE]                # forget Bee's stored copy
bee auth status [PROFILE]                # where each credential comes from, never the credential itself
```

`PROFILE` defaults to the default profile. `--no-browser` prints the sign-in URL without trying to open it. Prompts and
URLs go to stderr; only `status` writes to stdout. Bare `bee` still serves ACPv2.

### OAuth sign-in, API tokens, inference credentials and MCP authorization

These are different things and Bee keeps them apart:

- **Browser sign-in** is a provider's own flow, and what it returns differs by provider. OpenRouter's authorization-code
  flow with PKCE returns an **API key**, not an OAuth access token: Bee stores it and sends it as a bearer token. The
  key does not expire and cannot be refreshed, so a rejected key is fixed by signing in again. ChatGPT's separate flow
  returns renewable OAuth tokens tied to a verified account/workspace registration.
- **API tokens** are pasted (`--with-token`) or read from a variable. API-key profiles support them, and they are the
  only option for providers with no browser flow.
- **Cloudflare** (Workers AI, AI Gateway) uses a *scoped API token* from the environment or the store. Bee does not
  read or reuse Wrangler's browser-login token: it belongs to Wrangler and is not interchangeable with an API token's
  scopes. Put the account id in `base_url`, for example
  `https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1` under `protocols.openai-chat`; a provider that also
  serves the Anthropic protocol lists it beside it, with `prefer`.
- **Inference credentials** (everything above) authenticate Bee to an *LLM provider*. **MCP server authorization**
  (OAuth 2.1 for a remote tool server) is a separate audience: the credential store keeps the two in separate tables so
  one can never be offered for the other. The MCP authorization flow itself is not implemented; MCP configuration stays
  the client's concern.

### Where credentials are kept

`bee auth login` stores credentials in `$XDG_DATA_HOME/bee/credentials.json` (or `~/.local/share/bee/credentials.json`),
created with mode `0600` in a `0700` directory and replaced atomically. Bee refuses to read the file if other users can
access it, and tells you to `chmod 600` it. For API keys, `bee auth logout` removes Bee's copy only: the key stays valid
with the provider until you revoke it there. ChatGPT logout also attempts renewable-session revocation and retains its
registration/host metadata. Credentials, authorization codes, PKCE verifiers and response bodies never appear
in diagnostics or debug output. See [ADR-008](adr/008-provider-authentication.md).

OpenRouter example:

```toml
[providers.openrouter]
preset = "openrouter"
```

```bash
bee auth login openrouter
```

Stored credentials are resolved when an inference request needs them, using the selected profile's audience.

## Scope

Bee is the agent side of an ACPv2 connection. It runs the agentic loop and talks to LLM providers; presentation and
orchestration belong to the client. The planned boundaries are:

- ACPv2 protocol integration
- agent execution and the agentic loop
- LLM provider integration and model configuration
- session execution and cancellation
- configuration and credential handling
- error handling and observability

## Non-goals

- Bee is an agent runtime, not an orchestration platform.
- Bee does not provide its own TUI, web interface or session-management frontend.
- Bee does not assume it will only be used with Hive or any particular ACPv2 client.
- MCP configuration and orchestration are not Bee-specific responsibilities. Bee should use the capabilities provided
  through the ACPv2 client-agent contract, where the protocol supports them.
- WebAssembly-based extensions are a possible future direction, not an initial requirement.
- Bee does not assume ACPv2 provides a universal implementation of provider authentication. Each browser sign-in is a
  described in data and limited to one mechanism, not a generic OAuth client.

## Installation

Bee is not published to crates.io (the `bee` crate name is taken by an unrelated project). Download a binary for your
platform from the [latest release](https://github.com/noirbizarre/bee/releases/latest) once one is published.

## Usage

An ACPv2 client launches `bee` as a subprocess and speaks the protocol on its stdin and stdout. Stdout carries protocol
traffic only; diagnostics go to stderr. Closing stdin ends the session and Bee exits with status 0. `bee auth` is the
one command a person runs by hand.

```bash
bee --help
```
