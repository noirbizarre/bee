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

**Chat Completions, Responses and Anthropic Messages inference are implemented.** Selected accounts receive prompts and
stream answers and [supported displayable reasoning](inference.md#displayable-reasoning-and-acp-thoughts) over ACP.
Cancellation and close interrupt pending inference. Production sessions retain successful
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
Bee also supports [project-local MCP definitions](mcp.md) in `.agents/mcp.toml`,
plus direct stdio/Streamable HTTP
attachments. Direct connections and tool calls require separate consent by default.
Native Bash and mutation calls request client permission by default; read-only tools
run directly. [Ordered permission rules](tools.md#native-permission-rules) can allow,
ask or deny native operations, with project-only tightening. Form-capable clients also receive
`ask_user`; URL-capable clients can perform provider browser login. See
[ACP user interactions](tools.md#acp-user-interactions) for capability and outcome handling.

See the [ACP / ACPv2 compatibility matrix](compatibility.md) for the complete versioned feature inventory,
supported subsets, planned and not-planned features, implementation evidence and pinned interoperability results.

The Rust library also provides an [outbound ACPv2 client](acp-client.md) for explicitly selected named local agents,
with scoped stdio process ownership and structured host permission/elicitation handlers.

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
Project MCP resources are separate: they follow the session project root and reload at prompt boundaries and resume.
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

[Profiles](profiles.md) scope accounts and an optional qualified default model. Select with top-level
`profile = "work"` or `BEE_PROFILE=work`. The selector precedence is implicit `default` → global setting → `BEE_PROFILE`
→ `BEE_CONFIG` file → project setting. An unknown profile fails without fallback. Accounts with `enabled` omitted/true
form the baseline inherited independently by every profile. An explicit `accounts = [...]` replaces it, may include
disabled accounts, and `[]` means none. `[profiles.default]` affects only that profile. Model selection, account model
listing and durable session access are scoped to the active profile; explicit authentication can target any configured account.

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

Accounts reference compiled-in providers and own credentials, routes and [model availability](models.md).
Public Models.dev metadata only enriches available choices; it never expands availability or changes endpoints.

A configured account is separate from its provider implementation, grouping profile, protocols and model:

```toml
model = "work/corp/ft-2026"         # qualified root default; split only on the first slash

[accounts.work]                    # configured identity, independent of vendor
provider = "custom"               # compiled-in implementation
enabled = true                    # default; baseline profile membership
display_name = "Company gateway"  # optional presentation name; IDs remain stable
discovery = false                 # native listing capability is separate from this boolean
models = ["corp/ft-2026", "corp/base"] # exactly available raw IDs with discovery off
credential = { env = "GATEWAY_KEY" }           # optional: names where the secret lives, see Authentication
headers = { "X-Team" = "infra" }   # optional, sent only to this account
timeout_ms = 120000               # optional whole-request deadline, including streaming
max_output_tokens = 4096          # optional output limit
context_window = 32768            # optional explicit context capacity; no default
model_context_windows = { "corp/base" = 16384 } # optional exact-model capacity overrides

# Custom endpoints specify one table per protocol: "openai-chat", "openai-responses" or
# "anthropic-messages".
[accounts.work.protocols.openai-chat]
base_url = "https://gateway.corp.example/v1"   # explicit for custom endpoints
options = { max_tokens_field = "max_completion_tokens" }   # settings only this protocol understands
```

An account can configure several protocols, each at its own URL and with its own options. Several protocols need `prefer`,
which orders them, most preferred first; Bee does not guess among them. An optional `model_protocols` table maps exact
model IDs to configured protocols. Unmapped IDs use the first endpoint in preference order, unless optional
[catalog protocol hints](models.md#optional-protocol-hints-and-context-budgets) are explicitly enabled.

```toml
[accounts.gateway]
provider = "custom"
discovery = false
models = ["vendor/model.v2"]
prefer = ["anthropic-messages", "openai-chat"]
model_protocols = { "vendor/model.v2" = "openai-chat" }

[accounts.gateway.protocols.openai-chat]
base_url = "https://gw.example/openai/v1"

[accounts.gateway.protocols.anthropic-messages]
base_url = "https://gw.example/anthropic"
options = { max_tokens = 2048 }
```

Mapping keys are literal, case-sensitive model IDs: quote IDs containing dots, slashes or other TOML punctuation.
There is no prefix matching or vendor inference. Mapping targets must be supported protocols configured on the final
account, including endpoints supplied by its provider; invalid targets stop Bee at launch. Mappings do not grant model
availability. Unknown/private models remain usable offline when declared with discovery off. `prefer` is still required
for multi-protocol accounts because unmapped models need a fallback.

`model_protocols` merges key by key across global, `BEE_CONFIG` and project files, and provider defaults: a higher-priority
entry replaces the same raw model ID while retaining other entries. An empty table (`model_protocols = {}`) does not clear
inherited mappings. See [ADR-013](adr/013-per-model-protocol-selection.md).

`context_window` and entries of `model_context_windows` are positive 32-bit token capacities. Exact model entries win
over the account fallback; otherwise capacity is unknown unless catalog context windows are explicitly enabled.
The model map merges per key across layers and provider defaults,
without clearing inherited entries for an empty table. Before each request, Bee estimates input, reserves the effective
output maximum and drops oldest complete turns when needed. The active turn and its tool pairs remain intact.
Configured sessions report labeled estimates and notices when older context is removed. See
[conversation context and token budgets](inference.md#conversation-context-and-token-budgets) for the estimate formula,
transactional retention and explicit overflow handling.

Several accounts can use the same provider/protocol with independent credentials and endpoints.
Layers merge as above: a project file can override one header or replace an account's `models` array. Unknown keys,
unsupported protocols and malformed URLs, headers or models stop Bee at launch with an error that names the key and
the file and never the value. A `credential` only *names* where the secret lives; do not put a key in the file (see
[Authentication](#authentication)).

With `discovery = false`, available IDs are exactly `models`; omitted/empty means none. With discovery on, successful
selectable account results are intersected with optional `models`: omission applies no filter, `[]` permits none.
Pending/first failed discovery authorizes no models. Same-account last-good results may remain usable stale on transient
failure; authorization loss clears visibility. Defaults, selected IDs, routing/metadata keys and public catalogs
add no choices.

Bee offers one ACP model selector across active accounts. Values are qualified `account-id/raw-model` IDs; labels combine
account display name (provider label, then ID fallback) and model display name (raw ID fallback). Qualified IDs remain
visible when labels collide. Selection changes account/model atomically and cannot change during a turn.
An unselected session shows “Choose a model”, never the first real choice. No first-account/model or native-order
default exists.

Current/durable explicit selection takes precedence, followed by usable account-bound recent success, then an applicable
root/profile `model`, then unselected. A root default is dormant when its account is excluded; a profile's own out-of-scope
default is an error. Defaults contain qualified IDs, while model lists/maps keep raw IDs, preserving vendor prefixes.
Every selection and prompt enforces availability and account continuity; unavailable recency cannot silently cross accounts.
See [profile resolution](profiles.md#configured-default-and-last-used-model) and
[ADR-028](adr/028-account-model-availability-and-selection.md).

Protocol resolution uses that effective model ID for every prompt, whether selected by the client or supplied by a
default. Changing models resolves the endpoint again, retains portable history and drops native continuation.

Requests use the resolved protocol exactly; failures never trigger a retry on another protocol. `openai-chat`,
`openai-responses` and `anthropic-messages` can run inference, including providers preferring Responses or Messages.
See [Inference and smoke tests](inference.md) for the supported subset and verification instructions.

### Prompt-cache settings

Native cache settings belong to `accounts.<account>.protocols.<protocol>.options`, including when inherited from a
provider. OpenAI Chat and Responses accept `prompt_cache_key` (a table with `mode = "auto"`, `"disabled"` or `"explicit"`
and an explicit `value`), `prompt_cache_retention` (`"default"`, `"in_memory"`, `"24h"`), and `prompt_cache_options`
(`enabled`, `mode`, `ttl`, `breakpoints`). Anthropic Messages accepts `cache_control` (`policy`, `ttl`).

By default, Bee derives stable opaque keys on verified key-capable endpoints; it adds no retention, write-mode or
breakpoint policy. Anthropic writes require explicit opt-in. Endpoint, model and authentication compatibility are checked
before inference; wire compatibility alone does not enable a control. Unsupported explicit choices produce
`bee::llm::cache` errors instead of being stripped. See the
[capability checklist and configuration examples](inference.md#prompt-caching) before enabling a policy.

Nested options merge key by key across provider defaults and configuration layers. Use `prompt_cache_key.mode = "disabled"`,
`prompt_cache_retention = "default"`, `prompt_cache_options.enabled = false`, or `cache_control.policy = "disabled"` to
override an inherited setting; an empty table does not reset it. Disabling a key only omits that routing/accounting field,
not the provider's automatic caching.

## Authentication

An account's `credential` says where its secret comes from, independently of the protocol and model. Ordinary protocol
defaults present it as `Authorization: Bearer` for `openai-chat` and `openai-responses`, or `x-api-key` for
`anthropic-messages`. Native authentication can override that presentation: the `github` provider uses
`Authorization: Bearer` for all three protocols, including Messages, without an `x-api-key` header.
Exactly one source, or none:

```toml
credential = { env = "OPENROUTER_API_KEY" }            # a variable in the environment that launches Bee
credential = { store = true }                          # Bee's credential store; store a token with `bee auth login`
# no credential: an unauthenticated endpoint, such as a local server
```

An empty variable counts as unset. A credential that is named but missing or expired is an error that says which
command fixes it; Bee never quietly sends a request without it. Credentials are resolved when a request needs them, not
at launch.

### Providers and accounts

Bee ships a compiled-in provider registry. An account references a provider and overrides its allowed defaults:

```toml
[accounts.work]
provider = "openrouter"            # protocol/authentication defaults and native discovery
models = ["anthropic/claude-sonnet-4"] # filter successful discovery to this raw ID
```

`bee providers list` lists implementations; `bee providers show NAME` describes authentication and fields.
See [provider and account configuration](providers.md) for custom endpoints and multiple accounts.
Shipped providers, with the variable each reads its key from:

| Provider | Display name | Protocols, preferred first | Credential |
| --- | --- | --- | --- |
| `cloudflare` | Cloudflare AI Gateway | `openai-chat`, `openai-responses`, `anthropic-messages` | `CLOUDFLARE_API_TOKEN` |
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
| `groq` | Groq | `openai-chat`, `openai-responses` | `GROQ_API_KEY` |
| `mistral` | Mistral | `openai-chat` | `MISTRAL_API_KEY` |
| `scaleway` | Scaleway | `openai-chat` (serverless Generative APIs) | `SCW_SECRET_KEY` |
| `ovh` | OVHcloud AI Endpoints | `openai-chat`, `openai-responses` | `OVH_AI_ENDPOINTS_ACCESS_TOKEN` |
| `xai` | xAI | `openai-responses`, `openai-chat` | `XAI_API_KEY` |
| `ollama` | Ollama | `openai-chat`, local server | none |

Generic `custom` uses explicit compatible endpoint URLs and credentials. Groq is distinct from xAI: Grok models use
`xai`, not a separate provider identity.

`display_name` is optional on accounts and overrides the provider label, falling back to the account ID. Names must
be nonblank, control-free and fit in 256 UTF-8 bytes. Labels are presentation only; credential and session identities use
account IDs. The unified picker combines account/model labels and retains qualified IDs for collisions.

Scalars and arrays in the account replace provider defaults. `model_protocols`, `model_context_windows`, `headers` and each
protocol's table under `protocols` merge key by key, `prefer` replaces, and `credential` is replaced whole.
URLs follow vendor documentation or the referenced native integration contract. OpenCode translates text models across
Chat, Responses and Messages, including Gemini text models. Exact `model_protocols` entries can override the preference.
OpenCode Go uses separate endpoints (`/inference/go`) and needs an account of its own. Definitions contain no model
lists: declare raw `models` with discovery off, or select from successful filtered discovery. Optional defaults belong
only at root/profile `model` and are qualified.

### Use Scaleway Generative APIs

Run `bee auth login scaleway` to import `SCW_SECRET_KEY` or paste a key interactively. Use a key with
**GenerativeApisModelAccess** for the intended project. The native Chat endpoint is `https://api.scaleway.ai/v1`;
optional `settings.project_id` scopes its URL to a project UUID. Optional `settings.region` accepts only Paris (`fr-par`)
and uses the same global host. Declare Chat models and their context budgets; discovery is opt-in.
See [Scaleway setup, named accounts and permissions](providers.md#scaleway-generative-apis).

### Use OVHcloud AI Endpoints

Create a project-bound AI Endpoints access key in the OVHcloud Control Panel, then configure `provider = "ovh"` and
explicit raw `models`. Bee reads `OVH_AI_ENDPOINTS_ACCESS_TOKEN`; `bee auth login ACCOUNT` imports that binding into
private storage, or use `bee auth login ACCOUNT --with-token` to read a key from stdin. The shared inference endpoint
needs no project/region fields. Chat is preferred; Responses is explicitly selectable and model-dependent. Discovery
must remain disabled. See [setup and multiple accounts](providers.md#ovhcloud-ai-endpoints) and
[request restrictions](inference.md#ovhcloud-ai-endpoints-requests).

### Use Cloudflare AI Gateway

Run `bee auth login cloudflare` in a terminal to collect missing Account ID, Gateway ID and an API token, or configure
the identifiers in account `settings` and use an environment token or `--with-token`. The native provider derives
account-specific REST endpoints for all three protocols and sends the selected gateway header. It requires a token
with **Account → Workers AI → Read** and explicit model declarations with discovery disabled. See
[complete setup, multiple accounts and permissions](providers.md#cloudflare-ai-gateway) and
[protocol restrictions](inference.md#cloudflare-ai-gateway-requests).

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

First-use login can create separate enabled store-backed accounts and add newly created ones to a restricted active
profile. Existing-account login does not alter membership. Go enables automatic model discovery; Zen retains opt-in.
Select an available text model through ACP or set a qualified root/profile `model`. Public offering lists are unranked
and do not prove access, so Bee does not choose their first entry automatically. Zen's free-tier chat models require OpenCode;
use paid Zen text models or Go from Bee. Non-text services such as Jev are outside Bee's inference protocols.

Environment configuration remains supported:

```toml
model = "zen/gpt-6.1-sol"

[accounts.zen]
provider = "opencode"              # OPENCODE_API_KEY
discovery = false
models = ["gpt-6.1-sol"]           # choose a currently available paid text model

[accounts.go]
provider = "opencode-go"           # OPENCODE_GO_API_KEY; discovery defaults true
models = ["kimi-k3"]               # filter discovery to a model available to your subscription
```

Pass the appropriate variable to the Bee subprocess. No key or model ID is borrowed from the other offering. Existing
environment-backed accounts switch to stored credentials on explicit login. Plain `bee auth login ACCOUNT` imports
the configured environment key; `--with-token` imports stdin instead. Higher-priority source overrides must be removed
before login can switch authority. Account IDs are credential identities; use a new account when changing
offerings/accounts rather than repurposing an existing stored credential.

Rotate or revoke keys in the console, then repeat token login to replace Bee's stored copy. `bee auth logout ACCOUNT`
removes only that account's local key; it does not revoke it remotely. Status is a local availability check, not a
subscription or entitlement check. No browser authorization is started for these API-key accounts.

Bee sends its own versioned user agent and a stable `x-opencode-session` header across tool steps, prompts and durable
resume. Embedded callers constructing a model per turn should pass their stable conversation identity with
`Model::with_conversation_id`. These headers are reserved for the integration. Native discovery uses the official
offering target; a custom inference proxy remains usable but cannot silently redirect its credentials to official
discovery. Set `discovery = false` on such accounts and declare raw `models` explicitly.

Access denial can mean an invalid/revoked key, missing billing/subscription, workspace policy or unavailable model.
Check the selected offering in the console. Go allowances have 5-hour, weekly and monthly windows; limits and prices
can change. Console-side **Extra Usage**, if you enable it, can charge pay-as-you-go credits after the Go allowance.
Bee never enables it, switches offerings or retries inference against another credential. Consult the official
[Go usage limits and client requirements](https://opencode.ai/docs/go/) and
[Zen pricing and privacy terms](https://opencode.ai/docs/zen/) for current conditions.

Compatible ad hoc endpoints use an account of the `custom` provider:

```toml
[accounts.corp]
provider = "custom"
protocol = "anthropic-messages"
base_url = "https://llm.corp.example"
credential = { env = "CORP_KEY" }
discovery = false
models = ["private-model"]
```

Provider authentication contracts are compiled-in. Configuration cannot author new OAuth issuers or redirect credential
exchanges. See [accounts and credentials](providers.md) and
[ADR-027](adr/027-provider-registry-and-instance-authentication.md).

### Use Google AI Studio

Create a Gemini API key in [Google AI Studio](https://aistudio.google.com/), following Google's
[API-key setup](https://ai.google.dev/gemini-api/docs/api-key). Set the variable in the environment that launches Bee:

```sh
export GEMINI_API_KEY="YOUR_AI_STUDIO_API_KEY"
```

Add this account to your Bee configuration, replacing the model placeholder with an available Gemini model ID:

```toml
[accounts.google-aistudio]
provider = "google-aistudio"
models = ["YOUR_GEMINI_MODEL_ID"]   # filter authenticated discovery; raw wire ID
```

Choose `google-aistudio/YOUR_GEMINI_MODEL_ID` through ACP or set that qualified root/profile `model`.
If the chosen profile has an explicit `accounts` list, include `google-aistudio`; see [profile membership](profiles.md).
Inspect authenticated model visibility with:

```sh
bee auth status google-aistudio
bee models list google-aistudio --endpoint --json
```

The provider uses the official Gemini Developer API OpenAI-compatible endpoint and enables authenticated `/models`
discovery. Select an exact text/function-capable available model explicitly: listings do not supply a default or guarantee
invocation access. A custom `credential = { env = "GOOGLE_API_KEY" }` can replace the default variable. Bee does not
automatically fall back between keys. Models.dev's public association remains `google`; it is not a compiled provider ID.

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

The `google-cloud` provider displays **Google Cloud** and runs Gemini through the official
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

Ordinary `gcloud auth login` alone does not create ADC. Add an account, replacing the project and model placeholders:

```toml
model = "google-cloud/google/YOUR_SUPPORTED_GEMINI_MODEL_ID"

[accounts.google-cloud]
provider = "google-cloud"
discovery = false
models = ["google/YOUR_SUPPORTED_GEMINI_MODEL_ID"]
google_cloud = { project = "your-project-id", location = "global" }
# google_cloud.quota_project can explicitly override the quota project in ADC.
```

The resource project is required; location defaults to `global`. Use a supported region such as `us-central1` to choose
a regional endpoint. Bee keeps the exact `google/...` model ID. Check Google's current model lifecycle and
[locations](https://cloud.google.com/vertex-ai/generative-ai/docs/learn/locations) for compatibility-route availability.
Declare each available raw ID in `models`, and configure `context_window` or `model_context_windows` as needed.
Cloud model discovery is not implemented: `discovery = true` is rejected instead of querying an assumed `/models`
endpoint. Include the account in any explicit profile `accounts` list; see [profile membership](profiles.md).

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

First-use login can add the account to user configuration, displays a code to enter at `https://github.com/login/device`,
and waits for authorization. `--no-browser` prints instructions without opening a browser. This is a device flow:
enter the code on GitHub, rather than pasting it into Bee. Bee uses the existing public OAuth client
`Ov23li8tweQw6odWQebz`, also used by OpenCode; the consent application is not a Bee-owned registration.
Successful login discovers account-visible models; select one explicitly or set a qualified root/profile default.
Stored device login currently requires Bee's Unix private credential backend. On Windows, configure an environment
token as shown below; it does not require that backend.

To use a GitHub user token from an explicitly selected environment variable instead:

```toml
[accounts.github]
provider = "github"
credential = { env = "COPILOT_GITHUB_TOKEN" }
```

Set that variable before starting Bee. This replaces the store-backed credential source and browser login;
Bee does not search other variables, copy the token into its store, or fall back to stored authorization.
Use a token accepted by the Copilot direct API for your account. A GitHub token that can access repositories
does not necessarily authorize Copilot. Classic PATs are not a supported setup; server rejection remains authoritative.

The provider's native authentication applies independently of optional discovery, presenting a bearer token on all
three protocols. Account metadata may select a validated GitHub Copilot API host. Explicit entitlement denial stops
the operation with guidance to activate Copilot or request an organization seat. Custom Enterprise login domains
are not implemented. See [Copilot inference](inference.md#github-copilot-requests) for routing and context limits.

With `discovery = false`, declare raw `models` and an exact `model_protocols` mapping when it differs from the
provider preference. Authentication and safe target preparation still run. With native discovery enabled, advertised
model endpoints determine the route below explicit configuration, without model-family guesses or protocol retries.
ACP clients supporting URL elicitation can complete the same cancellable device flow during the first prompt.

`bee auth logout github` removes Bee's stored token. Remote authorization remains valid until revoked in
[GitHub's application settings](https://github.com/settings/connections/applications/Ov23li8tweQw6odWQebz).
Status reports local presence, not verified entitlement. Failed or cancelled login preserves previous credentials.

### Use your ChatGPT plan

The `chatgpt` provider implements OpenAI's official
[open-source/local application flow](https://developers.openai.com/siwc/token-sharing-open-source).
It uses your eligible ChatGPT account/workspace plan or credits for supported Responses requests.
Eligibility depends on OpenAI's current account, workspace, region and policy requirements;
successful identity sign-in alone does not grant inference permission. This documented integration covers
open-source, locally hosted applications. Paid or remotely hosted offerings require OpenAI's
[separate approval path](https://openai.com/form/sign-in-with-chatgpt-interest/).
Use is subject to the applicable [OpenAI terms and policies](https://openai.com/policies).

Start with `bee auth login chatgpt`: Bee can create an enabled account in user configuration, add the newly created
account to a restricted active profile, and discover its catalog after authentication. Select a qualified model explicitly;
native ordering never creates a default. Listing is not proof of account entitlement. See [model discovery](models.md).

For a custom account label, configure a named account:

```toml
[accounts.personal]
provider = "chatgpt"
display_name = "Personal ChatGPT"
# Discovery supplies available raw slugs; choose personal/<slug> through ACP.
```

```sh
bee auth login personal
bee auth status personal
bee models list personal
```

Login displays **Continue with ChatGPT**, opens the browser, and receives authorization at a temporary
`http://127.0.0.1:<port>/auth/callback` listener. `--no-browser` prints the authorization URL for you to open locally.
The callback must reach the Bee host; on a remote machine arrange secure loopback forwarding.
`--with-token` and `--paste-code` are not supported for this account.
Authorize ChatGPT-plan usage when prompted, then use the account-specific model listing or your ACP client's model
picker to select an available model slug. A declined plan permission is retained as a signed-in, inference-disabled
registration; explicitly repeat `bee auth login personal` to request permission again.

Named accounts are distinct account/workspace labels. Add another account using `provider = "chatgpt"` for another
registration; email alone does not identify a workspace. Reauthorization of an existing account must match its
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

The `openai` provider still uses `OPENAI_API_KEY` and separate API billing. OpenRouter's browser flow still
returns an API key. Bee never silently changes from ChatGPT-plan usage to API-key billing.
See [ChatGPT inference limitations](inference.md#chatgpt-plan-requests) and
[ADR-020](adr/020-chatgpt-plan-authentication.md).

### ACP browser sign-in

During a prompt, a missing or expired stored credential starts browser sign-in when
the selected account's provider supports login and the client advertises `capabilities.elicitation.url`.
Bee sends session-scoped `elicitation/create` with a unique ID. Accepting the URL
means the browser interaction may proceed, not that authentication succeeded.
For PKCE-key and ChatGPT callback flows, Bee waits for the loopback callback, validates state,
and exchanges the code over HTTPS. For GitHub device login, the browser interaction presents the code to enter
on GitHub while Bee polls for authorization; no loopback callback is needed.
Bee persists the credential before sending `elicitation/complete` with that same ID.
Credentials and callback/exchange secrets never enter model context.

ACP browser sign-in requires Bee's private credential store. Store operations, including token imports with
`--with-token`, are currently supported only on Unix. On native Windows, environment credentials are available for
compatible API-key accounts and explicit Copilot-token setups; URL capability alone does not enable the store.
ChatGPT-plan login requires stored OAuth credentials and is currently unavailable on native Windows. An OpenAI API-key
account is a separate authentication and billing route, not ChatGPT-plan access.

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
providers without browser login and unsupported URL mode remain configuration errors with CLI
guidance. A provider's inference 401/403 does not automatically retry login or inference.

### Commands

```bash
bee auth login [ACCOUNT]                 # browser/device sign-in for supported accounts
bee auth login [ACCOUNT] --paste-code    # no callback: open anywhere, paste the code (SSH, containers)
bee auth login [ACCOUNT] --with-token    # read an API token from standard input
bee auth logout [ACCOUNT]                # forget Bee's stored copy
bee auth status [ACCOUNT]                # source, never the credential itself
```

Omitted login/logout targets require exactly one active account; defaults and recency do not choose a target.
Status aggregates active accounts. Explicit authentication targets may name configured disabled accounts; authenticating
an existing account changes neither `enabled` nor membership. Native-name first-use login may create an enabled
same-name account and join an explicitly restricted active profile, subject to effective-layer/origin checks.
`--no-browser` prints the sign-in URL without opening it.
Prompts and URLs go to stderr; only `status` writes to stdout. Bare `bee` still serves ACPv2.

### OAuth sign-in, API tokens, inference credentials and MCP authorization

These are different things and Bee keeps them apart:

- **Browser sign-in** is a provider's own flow, and what it returns differs by provider. OpenRouter's authorization-code
  flow with PKCE returns an **API key**, not an OAuth access token: Bee stores it and sends it as a bearer token. The
  exchange supplies no refresh token or key expiry metadata; a revoked or expired key requires signing in again.
  ChatGPT's separate flow
  returns renewable OAuth tokens tied to a verified account/workspace registration.
- **API tokens** are pasted (`--with-token`) or read from a variable. API-key accounts support them, and they are the
  only option for providers with no browser flow.
- **Cloudflare AI Gateway and Workers AI** use the native `cloudflare` provider with private Account ID and Gateway ID
  settings and a scoped API token. Bee does not import Wrangler browser credentials. Configure models explicitly;
  [REST protocol restrictions](inference.md#cloudflare-ai-gateway-requests) and account/model billing requirements apply.
- **Inference credentials** (everything above) authenticate Bee to an *LLM provider*. **MCP server authorization**
  (OAuth 2.1 for a remote tool server) is a separate audience: the credential store keeps the two in separate tables so
  one can never be offered for the other. MCP OAuth is not implemented; direct servers accept explicit
  [environment/header credentials](mcp.md), independently of LLM accounts.

### Where credentials are kept

`bee auth login` stores credentials in `$XDG_DATA_HOME/bee/credentials.json` (or `~/.local/share/bee/credentials.json`),
created with mode `0600` in a `0700` directory and replaced atomically. Bee refuses to read the file if other users can
access it, and tells you to `chmod 600` it. For API keys, `bee auth logout` removes Bee's copy only: the key stays valid
with the provider until you revoke it there. ChatGPT logout also attempts renewable-session revocation and retains its
registration/host metadata. Credentials, authorization codes, PKCE verifiers and response bodies never appear
in diagnostics or debug output. See [ADR-008](adr/008-provider-authentication.md).

### OpenRouter browser authentication

OpenRouter's officially supported [OAuth PKCE flow](https://openrouter.ai/docs/guides/overview/auth/oauth) creates a
user-controlled API key. Configure an account in your Bee configuration:

```toml
[accounts.openrouter]
provider = "openrouter"
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
Alternatively, replace the provider's credential source:

```toml
[accounts.openrouter]
provider = "openrouter"
credential = { env = "OPENROUTER_API_KEY" }
```

The environment source replaces the store source entirely. See
[ADR-021](adr/021-openrouter-authentication.md) for the verified protocol, lifecycle and eligibility constraints.

Stored credentials are resolved when inference needs them, using the selected account's audience. Referencing the
same account from several profiles intentionally shares its credentials; separate identities need separate accounts.

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
- Bee does not provide an MCP registry service or configuration UI. Project-local definitions and ACP attachments use
  standard MCP transports and tools with client-mediated consent.
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
