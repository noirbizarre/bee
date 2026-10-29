# Model availability, discovery and metadata

Accounts own model availability. Profiles scope accounts, and compiled-in providers supply implementation capabilities.
The **public Models.dev catalog** enriches metadata; an **account endpoint listing** observes models at the configured
target with current credentials. These remain separate: public descriptions never grant account availability, and a
listing does not guarantee entitlement, remaining quota or successful inference.

## Account model availability

Account-local model IDs are opaque, case-sensitive raw wire IDs, including private, fine-tuned and local names.
Availability is enforced for explicit selection and inference, including configured defaults, recency and resumed sessions.

| Account configuration | Available models |
| --- | --- |
| `discovery = false`, nonempty `models` | Exactly the declared raw IDs. |
| `discovery = false`, omitted `models` or `models = []` | None. |
| `discovery = true`, omitted `models` | All selectable IDs from a successful account listing. |
| `discovery = true`, nonempty `models` | Successful selectable account listing ∩ declared IDs. |
| `discovery = true`, `models = []` | None. |

```toml
model = "work/vendor/model-a"

[accounts.work]
provider = "openai"
discovery = false
models = ["vendor/model-a", "vendor/model-b"]
catalog = "openai"
```

Arrays replace across configuration layers. Omitted `models` and `models = []` are intentionally distinct when discovery
is enabled. Default/selected IDs, `model_protocols`, context/metadata-map keys and public catalog entries cannot append
choices. Unknown/private IDs work offline when declared with discovery off. Configuration parsing does no network work;
statically impossible defaults fail early, while discovery-dependent choices are checked asynchronously before use.

Discovery reports declared, pending, ready, stale, failed or unauthorized status per account. Pending or first failed
discovery authorizes no models. A fresh successful empty listing is known empty. Transient failures can retain last-good
results only for the same account and target, with original fetch time and a stale flag. Logout, authorization rejection,
account replacement or target changes clear visibility; obsolete late responses cannot republish it. Identity-preserving
OAuth renewal does not clear visibility. Private snapshots from different account authorities are never combined.

Before each production prompt, Bee authenticates/renews the selected account, checks authority and profile membership,
awaits required availability and native route/context facts, then freezes the turn. Explicit selection follows the same
availability rule. `--endpoint` may observe metadata with automatic discovery off, but cannot override declared
availability or expose an alternative unfiltered selectable list. Native hidden-model routing/context facts do not grant
picker membership. No selected candidate means a choose-model error without authenticating an arbitrary account.

Cloudflare AI Gateway uses explicit raw `models` with discovery disabled. Third-party IDs retain their `author/model`
prefix and Workers AI models retain `@cf/author/model`. Native routing keys do not grant availability, and model support
depends on the selected protocol. See [Cloudflare setup](providers.md#cloudflare-ai-gateway) and
[request restrictions](inference.md#cloudflare-ai-gateway-requests).

OVHcloud AI Endpoints also requires explicit raw `models` and discovery disabled. Endpoint list/refresh operations
are unsupported for `ovh`. Its separate public catalog is metadata only and cannot grant account availability.
See [OVH setup](providers.md#ovhcloud-ai-endpoints) for independent project-bound credentials and model selection.

## ACP and CLI

The ACP picker combines available models across **all active accounts**. Machine values are qualified
`<account-id>/<raw-model-id>` references. Only the first slash separates the account: `work/vendor/model-a` sends
`vendor/model-a` on the wire. Account-local routing, context maps and metadata retain raw IDs.

Labels combine account display name (provider label, then account ID as fallback) and model display name (raw ID as
fallback). Qualified IDs remain visible to disambiguate collisions. Account/model selection changes atomically, with
session busy/finalization guards and durable save before response. Cross-account changes discard native continuation
even when the raw model ID is unchanged.

Unselected, pending or unavailable sessions retain a usable picker with a presentation-only “Choose a model” option.
Its empty value is never accepted as a model reference, persisted or sent to inference. The first real choice is never
visually or automatically selected. Per-account status and model provenance/freshness accompany choices. No sole-model,
first-declaration, native-order or public-catalog default exists. See [default and recency rules](profiles.md#configured-default-and-last-used-model).

```sh
bee models list --json
bee models list work --json
bee models list work --endpoint --json
bee models providers --json
bee models list --catalog anthropic --json
bee models refresh
bee models refresh work --json
```

`list [ACCOUNT]` aggregates active accounts when omitted, or restricts to the named active account. Discovery is bounded
and concurrent. Human/JSON output uses qualified IDs, consistent display labels and per-account status. Successful
results are printed even if another account fails. Ordinary list warns and succeeds with usable stale results; targeted
discovery with no usable result returns nonzero. Explicit refresh failure remains nonzero even with stale data.

`providers` and `--catalog ID` browse the public catalog independently of account membership, using public catalog IDs
and raw model IDs. `--catalog` conflicts with a positional account and `--endpoint`. `refresh ACCOUNT` explicitly
refreshes that account; untargeted `refresh` forces a public download even with public `refresh = false`, but respects
`enabled = false`. Human freshness diagnostics go to stderr; JSON status and facts go to stdout. ACP stdout remains
protocol-only. Compiled implementations are inspected separately with `bee providers list/show`.

## Automatic catalog refresh and cache

Supported native-name login can create a missing enabled account, without choosing a default model:

```sh
bee auth login openrouter
bee auth login chatgpt
bee auth login github
```

New accounts may join an explicitly restricted active profile subject to effective-layer/origin checks. Existing-account
login changes neither enabled state nor membership. Successful authentication triggers discovery; discovery failure
does not undo login. Credentials stay in the private store. ChatGPT requires its browser OAuth flow.

OpenRouter, ChatGPT, GitHub Copilot and OpenCode Go enable native discovery by default. OpenCode Zen remains opt-in;
set `discovery = true`. Google AI Studio enables generic compatibility listing by default. `discovery = false` disables
automatic account listing without disabling native authentication, headers, renewal or route restrictions.
Google Cloud requires `discovery = false` and declared models; unsupported true discovery is rejected.

Production ACP starts background discovery after initialization. Public catalog requests do not block sessions or
inference; discovery-enabled account availability may need to be awaited. Embedded runtimes stay offline unless a
discovery service is supplied. Public policy is independent:

```toml
[catalog]
enabled = true                  # false disables both network and cached public catalog use
refresh = true                  # false permits cache-only use
refresh_interval_secs = 86400    # positive 32-bit seconds; default one day
```

Bee downloads `https://models.dev/api.json`. Only validated public data is atomically cached at
`$XDG_CACHE_HOME/bee/models.dev.json`, falling back to `~/.cache/bee/models.dev.json`. Relative XDG cache roots are
ignored; no usable root means memory-only public caching. Write failure does not discard a successful download.
Fresh cache skips downloading. Expired data refreshes in the background with stale fallback and exponential retry
from one minute to one hour. Corrupt, future-dated and unsupported envelopes are ignored. Stale public metadata has
no maximum age, including opted-in hints/budgets, but cannot grant model availability.

Downloads have a 15-second whole-request deadline, no redirects and a 32 MiB body limit; endpoint listings use 4 MiB.
Catalogs are bounded to 4096 providers and 100000 models. Diagnostics never expose bodies, credentials or URLs.
Private endpoint IDs never enter the persistent public cache. CLI invocations rediscover private results rather than
sharing a persistent account catalog.

## Associate and override metadata

```toml
[accounts.work]
provider = "openai"
discovery = false
models = ["my-private-model"]
catalog = "openai"
catalog_protocol_hints = false
catalog_context_window = false
metadata = { tool_call = true }
model_metadata = { "my-private-model" = { display_name = "Private model", structured_output = false } }
context_window = 32768
model_context_windows = { "my-private-model" = 16384 }
```

`catalog` is an optional Models.dev association supplied by provider defaults or explicit configuration. Account names
are never implicitly matched to catalog providers; an unknown association simply provides no enrichment. Google's public
Models.dev ID remains `google`, separate from the compiled provider `google-aistudio` and raw `google/...` model paths.
Configured URLs always win; public API URLs, environment suggestions, headers and credentials are never applied.

`metadata` and exact, case-sensitive `model_metadata` entries accept:

- `display_name`: nonempty, control-free string.
- `input_modalities`, `output_modalities`: at most 32 nonempty, control-free names.
- `tool_call`, `structured_output`: optional booleans; missing means unknown.
- `input_limit`, `output_limit`: positive 32-bit informational limits, not request settings.

Fields resolve independently: exact model override → account default → native declaration → public catalog → unknown.
Explicit false overrides true declarations. Tables merge per key across layers/provider defaults; empty tables retain
inherited values. Metadata cannot add wire features or availability, nor automatically suppress tools. ACP/JSON facts
carry source, declared/inferred confidence, fetch time and stale status. Catalog modalities do not extend Bee's subset.
`inference_succeeded_at` records successful account inference observations without promising permanent access.

## Optional protocol hints and context budgets

With `catalog_protocol_hints = true`, resolution is exact configured `model_protocols` → Copilot exact native
`supported_endpoints` → recognized catalog npm hint for an already configured protocol → `prefer`.
Model `provider.npm` wins over provider `npm`; `family` is never a routing signal. `@ai-sdk/anthropic` hints Messages,
`@ai-sdk/openai-compatible` hints Chat and `@ai-sdk/openai` hints Responses. Unknown packages/unconfigured targets fall
back to preference. Hints are inferred, not verified. Failures never trigger another protocol attempt.

With `catalog_context_window = true`, capacity is exact `model_context_windows` → account `context_window` → Copilot
native total context → valid catalog capacity → unknown. Native Copilot routing/capacity applies independently of public
opt-ins; `max_prompt_tokens` remains informational. Public input/output limits never replace request output settings.
Approximate accounting and transactional whole-turn eviction still apply. The enforced fact's provenance is exposed in
`_meta["bee.dev/context"].window`.

Availability checks and required native facts precede the frozen model/tool loop, even with an explicit routing map.
Public enrichment remains optional. Later route changes strip native continuation while retaining portable messages;
only successful non-refused completion commits working history and route.

## Endpoint-specific visibility

Enabled listing refreshes every five minutes after success, retries failures exponentially from one minute to one hour
and shares in-flight refreshes. Listing never initiates browser login. ChatGPT OAuth renews before listing when needed.
Native adapters come from compiled provider capabilities, not string-valued configuration selectors.

- **OpenRouter:** authenticated `/models/user`, preserving user preferences, privacy/guardrails, labels, modalities,
  supported parameters and context limits. Pagination origins/paths are validated before credentials are attached;
  incomplete refresh never replaces a complete listing. Visibility is not a credit, quota or routing guarantee.
- **ChatGPT:** account-specific `models[]`, retaining `visibility = "list"`, exact `slug` IDs, labels and server order.
  Listing is separate from OpenAI API keys and does not establish plan entitlement.
- **GitHub Copilot:** direct authenticated `/models` with native ordering and limits. Disabled, non-picker, non-tool or
  non-streaming entries are not selectable. Exact `/chat/completions`, `/responses` and `/v1/messages` declarations
  choose supported routes in configured preference order. Unknown-only declarations are not guessed, and conflicting
  explicit mappings fail. Bee never enables model policies automatically. See [setup](index.md#use-github-copilot).
- **Generic compatibility:** `/models` appended to each distinct base URL, preserving gateway paths; unpaginated
  `data[].id`, sorted/deduplicated. Advertising more pages is unsupported rather than silently treated as complete.
- **Google AI Studio:** generic API-key bearer listing at
  `https://generativelanguage.googleapis.com/v1beta/openai/models`. Native Gemini pagination and capability/limit
  metadata are not implemented. No implicit model default or quota guarantee.
- **Google Cloud:** declared exact `google/...` IDs only. No verified project/region listing contract; `discovery = true`
  is rejected. AI Studio's public catalog is not Cloud availability evidence. See [setup](index.md#use-google-cloud).
- **OpenCode Zen / Go:** separate `/inference/v1/models` and `/inference/go/v1/models` offerings and credentials.
  Lists are unranked; select a text model explicitly. Neither listing proves subscription entitlement or Bee compatibility.

## Native adapter coverage

OpenRouter, ChatGPT, Copilot, OpenCode offerings and generic AI Studio listing are implemented as described above.
Native paginated Anthropic and Mistral APIs, Cloudflare AI Gateway routing discovery, native Gemini Models metadata,
and Amazon Bedrock foundation-model/inference-profile discovery remain follow-up work. Configure declared models with
discovery off where a suitable listing is unavailable. Cloudflare Workers AI enumeration is separate from AI Gateway;
Bedrock must distinguish published foundation models from account/region/IAM-authorized invocation targets.

See [ADR-028](adr/028-account-model-availability-and-selection.md) for current availability and selection decisions and
[ADR-018](adr/018-optional-model-discovery.md) for public metadata/cache rationale.
