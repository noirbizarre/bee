# bee

<p align="center">
  <img src="images/logo.svg" alt="Bee" style="max-width: 100%; height: auto;">
</p>

Bee is a lightweight, headless agent runtime written in Rust that implements ACPv2. It provides the core agentic loop
and LLM provider integration, allowing it to work with any compatible ACPv2 client.

## Status

Bee implements a standalone headless agent runtime for its initial **0.1.0 release**. The `bee` binary
serves draft ACP wire-v2 over stdio, so a compatible client can start it, complete the `initialize` handshake,
create sessions and submit prompts. Bee advertises the baseline session lifecycle
(`session/new`, `list`, `resume`, `close`, `prompt`, `cancel`).

**Chat Completions, Responses and Anthropic Messages inference are implemented.** Selected connections receive prompts and
stream answers over ACP. Cancellation and close interrupt pending inference. Production sessions retain successful
context across close/resume and process restart. Start resume replays the separate visible transcript before current
state. See [session history](sessions.md) for storage, retention and client restoration.
Cancelled, failed and refused turns do not enter model context.
Prompts accept text and resource links, represented as references without fetching their contents.

The bounded SDK-free loop supports injected function executors. Normal ACP sessions advertise and execute the native
`bash`, `read`, `glob`, `grep`, `list`, `write`, `edit`, `delete`, `move`, and `copy` tools across all three backends,
using each session's working directory.
Client-hosted MCP tools are discovered and invoked through ACPv2's `session.mcp.acp` transport alongside the native
catalog. The client supplies server attachments and enforces permissions. See [tools](tools.md) for the contracts,
execution context and supported MCP subset.
Native Bash and mutation calls request client permission by default; read-only tools
run directly. [Ordered permission rules](tools.md#native-permission-rules) can allow,
ask or deny native operations, with project-only tightening. Form-capable clients also receive
`ask_user`; URL-capable clients can perform provider browser login. See
[ACP user interactions](tools.md#acp-user-interactions) for capability and outcome handling.

See the [ACP / ACPv2 compatibility matrix](compatibility.md) for the complete versioned feature inventory,
supported subsets, planned and not-planned features, implementation evidence and pinned interoperability results.

Bee loads project and nested `AGENTS.md` instructions, additive `~/.agents/AGENTS.md`, and standard Agent Skills from
project/user `.agents/skills` directories. Additional skill roots use `[skills] extra_roots = ["/opt/team/skills"]` in the
normal configuration hierarchy. See [project instructions and skills](instructions.md) for scope, precedence and loading.

## Configuration

New users should begin with [installation](installation.md) and [getting started](getting-started.md).
The latter includes client launch settings and [known limitations](getting-started.md#known-limitations).

For client integration tests, launch `bee --scenario success` as a minimal ACPv2 reference implementation and
deterministic test agent. [Built-in scenarios](scenarios.md) cover completion, plans, permissions, form input,
cancellation and controlled errors with memory-only sessions and no LLM credentials or external services.
Normal inference configuration follows below.

Bee reads TOML configuration from up to three files, merged in this order, lowest precedence first:

1. **Global:** `$XDG_CONFIG_HOME/bee/config.toml`, or `~/.config/bee/config.toml` when `XDG_CONFIG_HOME` is unset,
   empty or relative. This is the same on every platform, macOS included.
2. **`BEE_CONFIG`:** the file named by that environment variable, for settings chosen per client or workspace.
3. **Project:** `.config/bee.toml` in the directory Bee is launched from. Parent directories are not searched.

The global and project files are optional. A `BEE_CONFIG` file that does not exist is an error, and so is any file
that exists but cannot be read or parsed: Bee exits before speaking the protocol and says which file and, for syntax
errors, which line and column. Diagnostics never repeat the contents of a file, since they may hold API keys.

Tables merge key by key, and for a scalar the higher layer wins. An array in a higher layer **replaces** the lower one
instead of extending it. Giving one key different types in two layers (a string over an integer, say) is an error that
names the key and both files. See [ADR-006](adr/006-layered-xdg-configuration.md).

Effective [permission policy](tools.md#native-permission-rules) is an exception: global and `BEE_CONFIG` rule arrays
concatenate in order, while project rules only tighten decisions. An empty project array cannot erase trusted rules.

An empty `BEE_CONFIG` is unset; a relative path resolves from Bee's process working directory. Configuration is loaded
at launch, not reloaded from each session's `cwd`. On Windows, an absent or empty `HOME` falls back to `USERPROFILE`.
All XDG roots must be absolute; empty or relative roots use the home-directory fallback:

| Purpose | XDG location | Home fallback |
| --- | --- | --- |
| Configuration | `$XDG_CONFIG_HOME/bee/config.toml` | `~/.config/bee/config.toml` |
| Credentials | `$XDG_DATA_HOME/bee/credentials.json` | `~/.local/share/bee/credentials.json` |
| Public model cache | `$XDG_CACHE_HOME/bee/models.dev.json` | `~/.cache/bee/models.dev.json` |
| Sessions and model preferences | `$XDG_STATE_HOME/bee/` | `~/.local/state/bee/` |

Without a usable configuration root, the global layer is skipped. Normal serving still needs a usable durable state
root; set an absolute `XDG_STATE_HOME` when no home directory is available. Scenario mode uses memory-only state.

### Named profiles

[Profiles](profiles.md) group connection references and profile-specific model preferences. Select with top-level
`profile = "work"` or `BEE_PROFILE=work`. The selector precedence is implicit `default` → global setting → `BEE_PROFILE`
→ `BEE_CONFIG` file → project setting. An unknown profile fails without fallback. Existing configurations retain their
behavior through an in-memory `default` profile; once explicit groups exist, an undeclared `default` is empty.
Connection selection, auth/model commands and durable session access are scoped to the active profile.

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

A provider connection is separate from the grouping profile, the protocols it speaks and the model:

```toml
provider = "work"                  # legacy default connection, within the implicit default profile

[providers.work]                   # any configured connection ID, independent of vendor
display_name = "Company gateway"  # optional presentation name; IDs remain stable
model = "corp/ft-2026"             # optional default model: any id, listed anywhere or not
models = ["corp/base"]             # optional suggestions for model selectors, not a limit
credential = { env = "GATEWAY_KEY" }           # optional: names where the secret lives, see Authentication
headers = { "X-Team" = "infra" }   # optional, sent only to this connection
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
connection, including endpoints inherited from a preset; invalid targets stop Bee at launch. Mappings do not add model
suggestions or restrict which IDs can be selected. Unknown/private models remain usable without Models.dev or another
catalog. `prefer` is still required for multi-protocol connections because unmapped models need a fallback.

`model_protocols` merges key by key across global, `BEE_CONFIG` and project files, and over presets: a higher-priority
entry replaces the same model ID while retaining other entries. An empty table (`model_protocols = {}`) does not clear
inherited mappings. See [ADR-013](adr/013-per-model-protocol-selection.md).

`context_window` and entries of `model_context_windows` are positive 32-bit token capacities. Exact model entries win
over the connection fallback; otherwise capacity is unknown unless catalog context windows are explicitly enabled.
The model map merges per key across layers and presets,
without clearing inherited entries for an empty table. Before each request, Bee estimates input, reserves the effective
output maximum and drops oldest complete turns when needed. The active turn and its tool pairs remain intact.
Configured sessions report labeled estimates and notices when older context is removed. See
[conversation context and token budgets](inference.md#conversation-context-and-token-budgets) for the estimate formula,
transactional retention and explicit overflow handling.

Several connections can use the same protocol, so a gateway, a local server and a vendor API are three connections.
Layers merge as above: a project file can override one header or the model of a global connection. Unknown keys,
unsupported protocols and malformed URLs, headers or models stop Bee at launch with an error that names the key and
the file and never the value. A `credential` only *names* where the secret lives; do not put a key in the file (see
[Authentication](#authentication)).

When a provider is configured, Bee offers each ACPv2 session a `provider` and a `model` configuration option and
handles `session/set_config_option`. These are session settings and differ from the configuration above: the client
chooses among connections in the active profile and can name any model id, but cannot change a connection's endpoint,
headers or credential. Explicit client selection takes precedence. New sessions otherwise prefer the active profile's
usable account-bound last-used connection/model, even over project-configured defaults. Without a remembered choice,
Bee uses the profile's default connection and its effective `model`, else its first effective `models` entry;
eligible native discovery can initialize an otherwise unselected model. Profile model fields override connection
defaults independently. An unavailable remembered choice can require explicit selection rather than switching accounts;
see the [profile resolution rules](profiles.md#configured-default-and-last-used-model).
Choosing another provider resets the model, and settings cannot change while a turn is running.
See [ADR-007](adr/007-provider-profiles-and-model-selection.md).

Protocol resolution uses that effective model ID for every prompt, whether selected by the client or supplied by a
default. Changing models resolves the endpoint again, retains portable history and drops native continuation.

Requests use the resolved protocol exactly; failures never trigger a retry on another protocol. `openai-chat`,
`openai-responses` and `anthropic-messages` can run inference, including presets preferring Responses or Messages.
See [Inference and smoke tests](inference.md) for the supported subset and verification instructions.

### Prompt-cache settings

Native cache settings belong to `providers.<connection>.protocols.<protocol>.options`, including when inherited from a
preset. OpenAI Chat and Responses accept `prompt_cache_key` (a table with `mode = "auto"`, `"disabled"` or `"explicit"`
and an explicit `value`), `prompt_cache_retention` (`"default"`, `"in_memory"`, `"24h"`), and `prompt_cache_options`
(`enabled`, `mode`, `ttl`, `breakpoints`). Anthropic Messages accepts `cache_control` (`policy`, `ttl`).

By default, Bee derives stable opaque keys on verified key-capable endpoints; it adds no retention, write-mode or
breakpoint policy. Anthropic writes require explicit opt-in. Endpoint, model and authentication compatibility are checked
before inference; wire compatibility alone does not enable a control. Unsupported explicit choices produce
`bee::llm::cache` errors instead of being stripped. See the
[capability checklist and configuration examples](inference.md#prompt-caching) before enabling a policy.

Nested options merge key by key across presets and configuration layers. Use `prompt_cache_key.mode = "disabled"`,
`prompt_cache_retention = "default"`, `prompt_cache_options.enabled = false`, or `cache_control.policy = "disabled"` to
override an inherited setting; an empty table does not reset it. Disabling a key only omits that routing/accounting field,
not the provider's automatic caching.

## Authentication

A connection's `credential` says where its secret comes from, independently of the protocol and model. Ordinary protocol
defaults present it as `Authorization: Bearer` for `openai-chat` and `openai-responses`, or `x-api-key` for
`anthropic-messages`. Native authentication can override that presentation: Copilot's `auth = "github"` uses
`Authorization: Bearer` for all three protocols, including Messages, without an `x-api-key` header.
Exactly one source, or none:

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

Bee ships ready-made connection presets. A connection names a preset and overlays its own keys:

```toml
[providers.work]
preset = "openrouter"              # protocols and credential come from the preset
model = "anthropic/claude-sonnet-4"
```

Shipped presets, with the variable each reads its key from:

| Preset | Display name | Protocols, preferred first | Credential |
| --- | --- | --- | --- |
| `openai` | OpenAI | `openai-responses`, `openai-chat` | `OPENAI_API_KEY` |
| `chatgpt` | ChatGPT | `openai-responses` | stored ChatGPT OAuth tokens |
| `github` | GitHub Copilot | `openai-responses`, `anthropic-messages`, `openai-chat` | stored GitHub device OAuth; explicit env override supported |
| `anthropic` | Anthropic | `anthropic-messages` | `ANTHROPIC_API_KEY` |
| `openrouter` | OpenRouter | `openai-chat`, `openai-responses` | stored, with browser sign-in |
| `opencode` | OpenCode Zen | `openai-chat`, `openai-responses`, `anthropic-messages` | `OPENCODE_API_KEY` |
| `opencode-go` | OpenCode Go | `openai-chat`, `openai-responses`, `anthropic-messages` | `OPENCODE_GO_API_KEY` |
| `deepseek` | DeepSeek | `openai-chat`, `openai-responses`, `anthropic-messages` | `DEEPSEEK_API_KEY` |
| `google-aistudio` | Google AI Studio | `openai-chat` (official Gemini compatibility endpoint) | `GEMINI_API_KEY` |
| `google-cloud` | Google Cloud | `openai-chat` (official Vertex AI compatibility endpoint) | renewable ADC |
| `google` | Google AI Studio (legacy preset) | `openai-chat` | `GEMINI_API_KEY` |
| `groq` | Groq | `openai-chat`, `openai-responses` | `GROQ_API_KEY` |
| `mistral` | Mistral | `openai-chat` | `MISTRAL_API_KEY` |
| `xai` | xAI | `openai-responses`, `openai-chat` | `XAI_API_KEY` |
| `ollama` | Ollama | `openai-chat`, local server | none |

`display_name` is optional on connections and presets. A connection override wins over its preset's name; without
either, the selector uses the connection ID. Names must be nonblank, contain no control characters and fit in 256 UTF-8
bytes. ACP shows `Display name (connection-id)` when the two differ, preserving distinct account choices. Selection,
CLI arguments, credential storage and durable sessions always use the connection ID.

Scalars and arrays in the connection replace the preset's. `model_protocols`, `model_context_windows`, `headers` and each
protocol's table under `protocols` merge key by key, `prefer` replaces, and `credential` is replaced whole.
URLs follow vendor documentation or the referenced native integration contract. OpenCode translates text models across
Chat, Responses and Messages, including Gemini text models. Exact `model_protocols` entries can override the preference.
OpenCode Go uses separate endpoints (`/inference/go`) and needs a connection of its own. Presets deliberately contain no
model lists: configure `model` or select an ID through ACP.

### Use OpenCode Zen or Go

[Zen](https://opencode.ai/docs/zen/) is Console pay-as-you-go access; sign in to the
[console](https://opencode.ai/console), add billing details and credits, and create an API key.
[Go or Go Plus](https://opencode.ai/docs/go/) requires a subscription in a single-member workspace, such as Personal.
Both document API keys for external coding agents. As verified on October 27, 2026, neither documents an external-client
OAuth/device flow, client registration, refresh mechanism or revocation API. OpenCode CLI authentication and website
sign-in are separate from Bee's platform API-key authentication; Bee does not import their private credentials.

For first-use stored-key setup on Unix, run the command for the offering you use and paste its key on stdin:

```bash
bee auth login opencode --with-token
bee auth login opencode-go --with-token
bee auth status
bee models list opencode-go --endpoint
```

Each login creates a separate store-backed connection in the active profile and enables model discovery. Select an
appropriate text model through ACP or set `model` in that connection. Public offering lists are unranked and do not
prove access, so Bee does not choose their first entry automatically. Zen's free-tier chat models require OpenCode;
use paid Zen text models or Go from Bee. Non-text services such as Jev are outside Bee's inference protocols.

Environment configuration remains supported, including existing `preset = "opencode"` configurations:

```toml
provider = "zen"

[providers.zen]
preset = "opencode"                # OPENCODE_API_KEY
model = "gpt-6.1-sol"              # choose a currently available paid text model

[providers.go]
preset = "opencode-go"             # OPENCODE_GO_API_KEY
model = "kimi-k3"                  # choose a model available to your subscription
```

Pass the appropriate variable to the Bee subprocess. No key or model ID is borrowed from the other offering. Existing
environment-backed connections are not converted by login. To use stored credentials for an existing or custom-named
connection, explicitly add `credential = { store = true }` beneath its `preset`, then run
`bee auth login CONNECTION --with-token`. Connection IDs are credential identities; use a new connection when changing
offerings/accounts rather than repurposing an existing stored credential.

Rotate or revoke keys in the console, then repeat token login to replace Bee's stored copy. `bee auth logout CONNECTION`
removes only that connection's local key; it does not revoke it remotely. Status is a local availability check, not a
subscription or entitlement check. No browser authorization is started for these API-key connections.

Bee sends its own versioned user agent and a stable `x-opencode-session` header across tool steps, prompts and durable
resume. Embedded callers constructing a model per turn should pass their stable conversation identity with
`Model::with_conversation_id`. These headers are reserved for the integration. Native discovery uses the official
offering target; a custom inference proxy remains usable but cannot silently redirect its credentials to official
discovery. Disable automatic discovery on such connections and configure model IDs explicitly.

Access denial can mean an invalid/revoked key, missing billing/subscription, workspace policy or unavailable model.
Check the selected offering in the console. Go allowances have 5-hour, weekly and monthly windows; limits and prices
can change. Console-side **Extra Usage**, if you enable it, can charge pay-as-you-go credits after the Go allowance.
Bee never enables it, switches offerings or retries inference against another credential. Consult the official
[Go usage limits and client requirements](https://opencode.ai/docs/go/) and
[Zen pricing and privacy terms](https://opencode.ai/docs/zen/) for current conditions.

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
GitHub Copilot uses a separate fixed `kind = "github"` device mechanism.
See [ADR-009](adr/009-provider-presets-and-login-flows.md).

### Use Google AI Studio

Create a Gemini API key in [Google AI Studio](https://aistudio.google.com/), following Google's
[API-key setup](https://ai.google.dev/gemini-api/docs/api-key). Set the variable in the environment that launches Bee:

```sh
export GEMINI_API_KEY="YOUR_AI_STUDIO_API_KEY"
```

Add this connection to your Bee configuration, replacing the model placeholder with an available Gemini model ID:

```toml
[providers.google-aistudio]
preset = "google-aistudio"
model = "YOUR_GEMINI_MODEL_ID"
```

If you have several connections, set the active profile's default connection explicitly. With explicit named profiles,
add `google-aistudio` to the chosen profile's `connections` array; see [profile membership](profiles.md).
Inspect authenticated model visibility with:

```sh
bee auth status google-aistudio
bee models list google-aistudio --endpoint --json
```

The preset uses the official Gemini Developer API OpenAI-compatible endpoint and enables authenticated `/models`
discovery. Select an exact text/function-capable model explicitly: these generic suggestions do not supply an initial
default or guarantee invocation access. A custom `credential = { env = "GOOGLE_API_KEY" }` can replace the default
variable. Bee does not automatically fall back between keys. The legacy `google` preset remains supported with its
existing opt-in discovery default.

Eligible free-tier access and paid-tier behavior depend on the model, project, region and Google's current
[pricing](https://ai.google.dev/gemini-api/docs/pricing) and
[rate limits](https://ai.google.dev/gemini-api/docs/rate-limits). API keys belong to projects; adding another key to the
same project does not create a fresh quota. A model listing does not report remaining quota. **Google AI Plus subscription
quotas are not Gemini API credits**; direct API usage has separate eligibility and billing. Bee reports rate-limit
failures without automatically retrying inference, changing accounts or upgrading billing.

This integration uses an AI Studio API key, with no Antigravity OAuth or subscription credential flow. The separate
[Google Cloud provider](#use-google-cloud) uses managed-platform endpoints and Cloud credentials. See
[inference behavior](inference.md#google-ai-studio-requests) for signed tool continuation and context budgeting.

### Use Google Cloud

The `google-cloud` preset displays **Google Cloud** and runs Gemini through the official
[Vertex AI OpenAI-compatible API](https://cloud.google.com/vertex-ai/generative-ai/docs/start/openai).
Choose a billing-enabled resource project, enable `aiplatform.googleapis.com`, and grant the invoking identity
`aiplatform.endpoints.predict` (commonly through `roles/aiplatform.user`, Vertex AI User).
See Google's [access-control guidance](https://cloud.google.com/vertex-ai/generative-ai/docs/access-control).
The identity also needs `serviceusage.services.use` on an ADC quota project when that attribution is used, commonly via
`roles/serviceusage.serviceUsageConsumer`. Resource access and quota-project permission are separate requirements.

For local user credentials, install the Google Cloud CLI and run:

```sh
gcloud services enable aiplatform.googleapis.com --project=YOUR_PROJECT_ID
gcloud auth application-default login
gcloud auth application-default set-quota-project YOUR_QUOTA_PROJECT_ID
```

Ordinary `gcloud auth login` alone does not create ADC. Add a connection, replacing the project and model placeholders:

```toml
[providers.google-cloud]
preset = "google-cloud"
model = "google/YOUR_SUPPORTED_GEMINI_MODEL_ID"
google_cloud = { project = "your-project-id", location = "global" }
# google_cloud.quota_project can explicitly override the quota project in ADC.
```

The resource project is required; location defaults to `global`. Use a supported region such as `us-central1` to choose
a regional endpoint. Bee keeps the exact `google/...` model ID. Check Google's current model lifecycle and
[locations](https://cloud.google.com/vertex-ai/generative-ai/docs/learn/locations) for compatibility-route availability.
Configure `models`, `context_window` or `model_context_windows` as needed. Cloud model discovery is not implemented:
`discover_models = true` is rejected instead of querying an assumed `/models` endpoint. With explicit named profiles,
add this connection to the chosen profile's `connections` array; see [profile membership](profiles.md).

Bee supports these [ADC sources](https://cloud.google.com/docs/authentication/application-default-credentials), in order:

1. `GOOGLE_APPLICATION_CREDENTIALS` pointing to an authorized-user or service-account JSON file. For a service-account
   key, pass `GOOGLE_APPLICATION_CREDENTIALS=/absolute/path/to/key.json` to the Bee subprocess.
2. Local user ADC from `gcloud auth application-default login`: `$CLOUDSDK_CONFIG/application_default_credentials.json`
   when configured, otherwise `~/.config/gcloud/application_default_credentials.json` on Unix or
   `%APPDATA%/gcloud/application_default_credentials.json` on Windows.
3. An attached service account from the Google Cloud metadata service. Grant its identity the resource and quota-project
   permissions above, and ensure its access scopes permit Cloud Platform requests.

Malformed or unreadable selected files fail without falling through to another account. Federation and impersonation
ADC files are currently unsupported and fail with setup guidance. Bee does not run credential-source executables.
Tokens are renewed in memory before model steps and are not copied into Bee's credential store. Custom URLs, protocols
and headers cannot redirect this integration's ADC authority. `bee auth status google-cloud` describes externally managed
ADC without acquiring a token; use gcloud to manage local ADC, including `gcloud auth application-default revoke` when
appropriate. Bee login/logout commands do not modify shared ADC.

The resource project selects the inference resource; `google_cloud.quota_project` overrides ADC's `quota_project_id`
for the `x-goog-user-project` header. A quota project is not a promise that all charges move away from the resource
project: Google Cloud's service-specific billing and quota rules apply. See
[Vertex AI pricing](https://cloud.google.com/vertex-ai/generative-ai/pricing).
Eligible Google Cloud credits depend on their terms and model eligibility. **Google Cloud billing/credits are distinct
from AI Studio's Gemini API free tier and Google AI Plus/Pro application or Antigravity quotas.** Model configuration
and token usage do not report remaining credits or quota. Failed or cancelled local turns may still incur provider
charges. Bee never retries inference or changes billing projects/regions automatically after quota or capacity errors.
See [Cloud inference behavior](inference.md#google-cloud-requests).

### Use GitHub Copilot

For a GitHub.com account with Copilot chat access:

```sh
bee auth login github
bee auth status github
bee models list github --endpoint --json
```

Login adds the connection to user configuration, displays a code to enter at `https://github.com/login/device`,
and waits for authorization. `--no-browser` prints instructions without opening a browser. This is a device flow:
enter the code on GitHub, rather than pasting it into Bee. Bee uses the existing public OAuth client
`Ov23li8tweQw6odWQebz`, also used by OpenCode; the consent application is not a Bee-owned registration.
Successful login discovers account-visible models and can initialize the first session's model selection.
Stored device login currently requires Bee's Unix private credential backend. On Windows, configure an environment
token as shown below; it does not require that backend.

To use a GitHub user token from an explicitly selected environment variable instead:

```toml
[providers.github]
preset = "github"
credential = { env = "COPILOT_GITHUB_TOKEN" }
```

Set that variable before starting Bee. This replaces the store-backed credential source and browser login;
Bee does not search other variables, copy the token into its store, or fall back to stored authorization.
Use a token accepted by the Copilot direct API for your account. A GitHub token that can access repositories
does not necessarily authorize Copilot. Classic PATs are not a supported setup; server rejection remains authoritative.

The preset's `auth = "github"` applies independently of optional discovery, presenting a bearer token on all
three protocols. Account metadata may select a validated GitHub Copilot API host. Explicit entitlement denial stops
the operation with guidance to activate Copilot or request an organization seat. Custom Enterprise login domains
are not implemented. See [Copilot inference](inference.md#github-copilot-requests) for routing and context limits.

With `discover_models = false`, configure a model and an exact `model_protocols` mapping when it differs from the
preset preference. Authentication and safe target preparation still run. With native discovery enabled, advertised
model endpoints determine the route below explicit configuration, without model-family guesses or protocol retries.
ACP clients supporting URL elicitation can complete the same cancellable device flow during the first prompt.

`bee auth logout github` removes Bee's stored token. Remote authorization remains valid until revoked in
[GitHub's application settings](https://github.com/settings/connections/applications/Ov23li8tweQw6odWQebz).
Status reports local presence, not verified entitlement. Failed or cancelled login preserves previous credentials.

### Use your ChatGPT plan

The `chatgpt` preset implements OpenAI's official
[open-source/local application flow](https://developers.openai.com/siwc/token-sharing-open-source).
It uses your eligible ChatGPT account/workspace plan or credits for supported Responses requests.
Eligibility depends on OpenAI's current account, workspace, region and policy requirements;
successful identity sign-in alone does not grant inference permission. This documented integration covers
open-source, locally hosted applications. Paid or remotely hosted offerings require OpenAI's
[separate approval path](https://openai.com/form/sign-in-with-chatgpt-interest/).
Use is subject to the applicable [OpenAI terms and policies](https://openai.com/policies).

Start with `bee auth login chatgpt`: Bee creates the built-in connection in user configuration, adds it to the active
profile and discovers its catalogue after authentication. No model IDs are required; native provider ordering initializes
an otherwise unselected model. Listing is not proof of account entitlement. See [model discovery](models.md).

For a custom account label, configure a named connection:

```toml
provider = "personal"

[providers.personal]
preset = "chatgpt"
# Optionally override the native initial choice with an exact model slug.
```

```sh
bee auth login personal
bee auth status personal
bee models list personal
```

Login displays **Continue with ChatGPT**, opens the browser, and receives authorization at a temporary
`http://127.0.0.1:<port>/auth/callback` listener. `--no-browser` prints the authorization URL for you to open locally.
The callback must reach the Bee host; on a remote machine arrange secure loopback forwarding.
`--with-token` and `--paste-code` are not supported for this connection.
Authorize ChatGPT-plan usage when prompted, then use the account-specific model listing or your ACP client's model
picker to select an available model slug. A declined plan permission is retained as a signed-in, inference-disabled
registration; explicitly repeat `bee auth login personal` to request permission again.

Named connections are distinct account/workspace labels. Add another connection using `preset = "chatgpt"` for another
registration; email alone does not identify a workspace. Reauthorization of an existing connection must match its
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
the connection names a login flow and the client advertises `capabilities.elicitation.url`.
Bee sends session-scoped `elicitation/create` with a unique ID. Accepting the URL
means the browser interaction may proceed, not that authentication succeeded.
For PKCE-key and ChatGPT callback flows, Bee waits for the loopback callback, validates state,
and exchanges the code over HTTPS. For GitHub device login, the browser interaction presents the code to enter
on GitHub while Bee polls for authorization; no loopback callback is needed.
Bee persists the credential before sending `elicitation/complete` with that same ID.
Credentials and callback/exchange secrets never enter model context.

ACP browser sign-in requires Bee's private credential store. Store operations, including token imports with
`--with-token`, are currently supported only on Unix. On native Windows, environment credentials are available for
compatible API-key connections and explicit Copilot-token setups; URL capability alone does not enable the store.
ChatGPT-plan login requires stored OAuth credentials and is currently unavailable on native Windows. An OpenAI API-key
connection is a separate authentication and billing route, not ChatGPT-plan access.

The whole attempt has a five-minute deadline; credential exchange also has a
30-second transport deadline. Decline, cancel, unknown actions and errors end the
turn without inference. Session cancel/close drops pending requests, listener and
exchange before terminal idle; late responses cannot store a credential. A credential
already committed before cancellation remains stored. Final store commits are
serialized across processes using a stable sibling lock and preserve other providers.

For callback flows, the browser must be able to reach the Bee host's loopback listener. URL capability
does not imply that a remote client shares that host; arrange secure loopback forwarding or use `bee auth login`
on the agent host when necessary. GitHub device login can use a browser on another host without callback forwarding.
Missing environment variables, invalid/insecure stores,
missing login flows and unsupported URL mode remain configuration errors with CLI
guidance. A provider's inference 401/403 does not automatically retry login or inference.

### Commands

```bash
bee auth login [CONNECTION]                 # browser/device sign-in for supported connections
bee auth login [CONNECTION] --paste-code    # no callback: open anywhere, paste the code (SSH, containers)
bee auth login [CONNECTION] --with-token    # read an API token from standard input
bee auth logout [CONNECTION]                # forget Bee's stored copy
bee auth status [CONNECTION]                # where each credential comes from, never the credential itself
```

`CONNECTION` defaults to the active profile's default connection for login/logout; status lists its member connections.
Explicit targets must be members, except explicit login can add a connection to the active profile. Missing built-in
`openrouter`, `chatgpt` and `github` connections are created on login.
`--no-browser` prints the sign-in URL without opening it.
Prompts and URLs go to stderr; only `status` writes to stdout. Bare `bee` still serves ACPv2.

### OAuth sign-in, API tokens, inference credentials and MCP authorization

These are different things and Bee keeps them apart:

- **Browser sign-in** is a provider's own flow, and what it returns differs by provider. OpenRouter's authorization-code
  flow with PKCE returns an **API key**, not an OAuth access token: Bee stores it and sends it as a bearer token. The
  exchange supplies no refresh token or key expiry metadata; a revoked or expired key requires signing in again.
  ChatGPT's separate flow
  returns renewable OAuth tokens tied to a verified account/workspace registration.
- **API tokens** are pasted (`--with-token`) or read from a variable. API-key connections support them, and they are the
  only option for providers with no browser flow.
- **Cloudflare Workers AI** uses a *scoped API token* from the environment or the store. Bee does not
  read or reuse Wrangler's browser-login token: it belongs to Wrangler and is not interchangeable with an API token's
  scopes. Put the account id in `base_url`, for example
  `https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1` under `protocols.openai-chat`; a provider that also
  serves the Anthropic protocol lists it beside it, with `prefer`.
- **Cloudflare AI Gateway** is a routing integration configured with its gateway URL, upstream provider/model identities
  and required authentication. The Workers AI URL above is not an AI Gateway catalogue endpoint. Configure models
  manually until gateway-native discovery is implemented; not every upstream model is usable through a given gateway.
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

### OpenRouter browser authentication

OpenRouter's officially supported [OAuth PKCE flow](https://openrouter.ai/docs/guides/overview/auth/oauth) creates a
user-controlled API key. Configure a connection in your Bee configuration:

```toml
[providers.openrouter]
preset = "openrouter"
```

```bash
bee auth login openrouter
bee auth status openrouter
```

Bee opens OpenRouter in your browser, validates the successful callback's state, exchanges the code using S256 PKCE,
and stores the key privately. OpenRouter supports localhost callbacks on any port; no application registration,
client secret, scopes or audience parameters are required by its documented flow. Denying access ends login without
storing anything; OpenRouter omits state on denial, so that response can only abort login, never authorize it.

Use `bee auth login openrouter --no-browser` to print the URL without opening it. The browser must reach the Bee host's
loopback interface. For SSH sessions or containers where it cannot, use `bee auth login openrouter --paste-code`:
open the URL on another machine and paste the authorization code OpenRouter displays. The code is single-use and
expires after ten minutes; Bee's browser callback wait is five minutes. An expired code requires a new login attempt.

You need an eligible OpenRouter account and permission to create a key in the selected workspace. The documented flow
does not require a special subscription or developer client registration. Account eligibility and organization/model
restrictions follow [OpenRouter's terms](https://openrouter.ai/terms); the provider's browser page handles account or
workspace ineligibility. Inference uses your account's credits and is subject to account, workspace, model and
[key limits](https://openrouter.ai/docs/api/reference/limits). Signing in does not grant free inference.

The resulting key has no refresh token. Bee records no expiry because the exchange does not return one; this is not a
promise that the key never expires. `auth status` checks the local credential, not its validity at OpenRouter. For a
revoked or expired key, run login again. For credit or budget failures, manage your account/key limits at OpenRouter.
Provider failures stop the request without silently switching credentials or providers, or replaying inference.

`bee auth logout openrouter` removes Bee's copy only. Revoke the key in
[OpenRouter's key settings](https://openrouter.ai/settings/keys) to invalidate it at the provider.

Browser authentication avoids manually copying an API key, but the inference credential is the same kind of bearer
key. To supply an existing key, use `bee auth login openrouter --with-token` and provide it on standard input.
Alternatively, replace the preset's credential source:

```toml
[providers.openrouter]
preset = "openrouter"
credential = { env = "OPENROUTER_API_KEY" }
```

The environment source replaces the store source entirely. See
[ADR-021](adr/021-openrouter-authentication.md) for the verified protocol, lifecycle and eligibility constraints.

Stored credentials are resolved when inference needs them, using the selected connection's audience. Referencing the
same connection from several profiles intentionally shares its credentials; separate accounts need separate connections.

## Scope

Bee is the agent side of an ACPv2 connection. It runs the agentic loop and talks to LLM providers; presentation and
orchestration belong to the client. Its responsibilities are:

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
- Bee does not assume ACPv2 provides a universal implementation of provider authentication. Each browser sign-in is
  described in data and limited to one mechanism, not a generic OAuth client.

## Installation

Bee is not published to crates.io (the `bee` crate name is taken by an unrelated project). Download a binary for your
platform from the [latest release](https://github.com/noirbizarre/bee/releases/latest) once one is published.
See [installation](installation.md) for supported assets, checksums, package managers and source builds.

## Usage

An ACPv2 client launches `bee` as a subprocess and speaks the protocol on its stdin and stdout. Stdout carries protocol
traffic only; diagnostics go to stderr. Closing stdin ends the connection and Bee exits with status 0.
People can use `bee auth` for provider credentials and `bee models` for model discovery.
Follow [getting started](getting-started.md) for a complete client launch, or consult the [CLI reference](cli.md).

```bash
bee --help
```
