# Model discovery and metadata

Bee keeps these concepts separate:

- A **profile** groups connection references and model preferences; see [named profiles](profiles.md).
- A **provider connection** owns configured URLs, credentials, headers and base defaults.
- A **protocol backend** is implemented code: Chat Completions, Responses or Anthropic Messages.
- An **endpoint listing** is the model IDs visible through a particular configured endpoint with current credentials.
- The **Models.dev catalog** describes publicly known providers and models. It suggests and enriches; it is not an
  allowlist, an endpoint configuration source, or verification of your account's capabilities.

Unknown, custom, fine-tuned and local model IDs remain valid without network access. Configured `base_url` values
always win: Bee never applies catalog API URLs, environment-variable suggestions, headers or credentials.

## Automatic catalog refresh and cache

Production ACP starts optional background discovery after initialization. Session creation and inference do not wait
for a catalog request. Embedded runtimes remain offline unless the caller supplies a discovery service.

```toml
[catalog]
enabled = true                  # false disables both network and cached catalog use
refresh = true                  # false permits cache-only use
refresh_interval_secs = 86400    # positive 32-bit seconds; default one day
```

Bee downloads `https://models.dev/api.json`. Only validated public catalog data is atomically cached at
`$XDG_CACHE_HOME/bee/models.dev.json`, or `~/.cache/bee/models.dev.json`. Relative XDG cache roots are ignored.
No cache root means memory-only discovery. Cache write failure does not discard a successful download.

Fresh cache skips downloading. Expired data refreshes in the background; failures retain the last valid snapshot,
mark it stale and retry with exponential backoff from one minute up to one hour. Corrupt, future-dated and unsupported
cache envelopes are ignored. Stale data remains usable without a maximum age, including for explicitly enabled
protocol hints and context budgets. Explicit configuration remains authoritative.

Downloads have a 15-second whole-request deadline, no redirects, and a 32 MiB body limit. Endpoint listings use a
4 MiB body limit. Catalogs are bounded to 4096 providers and 100000 model entries. Diagnostics do not expose upstream
bodies, credential values or URLs. Discovery failure never prevents manually configured models from running.

## Associate and override metadata

```toml
[providers.work]
preset = "openai"
model = "my-private-model"
catalog_provider = "openai"       # explicit Models.dev ID; inherited from known shipped presets
catalog_protocol_hints = false   # opt-in only
catalog_context_window = false   # opt-in only
discover_models = false          # opt-in automatic account-specific GET /models

metadata = { tool_call = true }
model_metadata = { "my-private-model" = { display_name = "Private model", structured_output = false } }
context_window = 32768
model_context_windows = { "my-private-model" = 16384 }
```

Arbitrary connection names are never matched to a catalog provider implicitly. Missing/unknown `catalog_provider` entries
are valid and simply have no enrichment. Presets carry associations as data; they do not hard-code model lists.

`metadata` and exact, case-sensitive `model_metadata` entries accept:

- `display_name`: nonempty, control-free string.
- `input_modalities`, `output_modalities`: lists of nonempty, control-free modality names (at most 32 entries).
- `tool_call`, `structured_output`: optional booleans. Missing means unknown, not false.
- `input_limit`, `output_limit`: positive 32-bit token limits, informational rather than request settings.

Metadata fields resolve independently: model override → connection default → catalog declaration → unknown.
Explicit false values
override true catalog declarations. Tables merge per key across configuration layers and presets; empty tables retain
inherited values. Existing `model_protocols`, `context_window` and `model_context_windows` remain the canonical explicit
routing/context settings. Catalog context/input/output limits stay distinct; output limits never silently replace
`max_output_tokens` or protocol-specific request options.

ACP and JSON output attach per-field source, confidence, fetch time and stale status. Configuration/catalog facts are
**declared**; npm protocol hints are **inferred**. Unverified capabilities stay absent/unknown rather than becoming
guarantees. Catalog modalities describe the model, not additional inference features implemented by Bee. Capability
overrides describe facts; they do not add wire features or automatically suppress tools.

## Optional protocol hints and context budgets

With `catalog_protocol_hints = true`, resolution is:

1. Exact configured `model_protocols` mapping.
2. Recognized catalog npm hint, only if that protocol is already configured on the connection.
3. Existing `prefer` fallback.

Bee reads model `provider.npm`, falling back to provider-level `npm` only when absent. `@ai-sdk/anthropic` hints Messages;
`@ai-sdk/openai-compatible` hints Chat; `@ai-sdk/openai` hints Responses, matching the AI SDK's default since v5.
Unsupported/unmapped packages and unconfigured targets fall back to `prefer`. `family` is never a routing signal.
Hints are approximate: one SDK can expose several APIs. Use an explicit mapping when your endpoint differs.
Failures never trigger another protocol attempt.

With `catalog_context_window = true`, capacity resolves as exact `model_context_windows` entry → connection
`context_window` → valid catalog context capacity → unknown. The existing approximate accounting and transactional
oldest-whole-turn eviction apply. Input/output catalog limits remain informational. Context reports identify the
enforced window's provenance in `_meta["bee.dev/context"].window`.

Both routing and budget are frozen at prompt acceptance, with no discovery wait, for the entire model/tool loop.
A later protocol change strips native continuation from the next turn's working history while preserving portable
messages. Only successful non-refused completion commits that history and route; cancellation/failure preserves
previously committed context.

## Endpoint-specific visibility

`discover_models = true` enables background enumeration once per minute. Bee appends `/models` to each distinct
configured OpenAI-compatible base URL, retaining gateway paths. Existing credentials and connection headers are used;
listing never starts browser login. ChatGPT OAuth credentials are renewed when necessary before listing;
other missing/expired credentials or unsupported listings do not affect inference.

Ordinary OpenAI-compatible endpoints use the unpaginated `data[].id` contract. The `chatgpt` preset uses OpenAI's
account-specific `models[]` contract, keeps `visibility = "list"` entries, and presents `display_name` labels with exact
`slug` IDs in server order. Its automatic endpoint discovery is enabled by the preset. Account names and IDs remain
separate from the public Models.dev cache.
A response advertising more pages is reported as unsupported rather than presented as complete.
Ordinary results are sorted/deduplicated; ChatGPT results are deduplicated without reordering. Both are bounded to
100000 IDs and kept in
memory per connection within the runtime's active profile. They are cleared before each refresh and after Bee's browser
login, so a failed refresh with
replacement credentials cannot retain another account's model list. External credential changes are observed on the
next listing refresh; visibility is an observation at the reported fetch time, not an authorization guarantee.

Endpoint IDs never enter the persistent public catalog cache. Listing confirms visibility only, not tool calls,
structured output, modalities or support for every inference protocol.

## ACP and CLI

ACP model options retain configured/default IDs first, then any selected custom ID, endpoint IDs and catalog IDs,
without duplicates. Display names are labels; the exact ID is sent to inference. Descriptions and
`_meta["bee.dev/models"]` expose provenance/freshness. Refresh sends complete `config_option_update` options to open
sessions without changing selections. Discovery never creates connections or chooses a default. A connection with no
selected/configured model keeps the existing absent-selector behavior; clients can still set any well-formed ID.

```sh
bee models providers --json
bee models list work --json
bee models list --catalog-provider anthropic --json
bee models list work --endpoint --json
bee models refresh
```

`providers` and `--catalog-provider` browse the dynamic global catalog without requiring configured connections.
`list [CONNECTION]` uses the active profile's default connection when omitted. Explicit targets must be members of the
active profile; its model preferences override connection defaults. `--endpoint` explicitly queries endpoint visibility
even if automatic listing is disabled, and does not require a catalog download. Ordinary queries refresh expired or
missing catalog data when enabled, falling back visibly to stale cache. Configured IDs remain listable offline.

`refresh` forces a download even with `refresh = false`, but respects `enabled = false`. A forced refresh failure has a
failure exit status, even if a stale catalog remains available. Human-readable output labels freshness on stderr;
`--json` includes status and facts on stdout. Only these binary command paths print: ACP stdout stays protocol-only.

See [ADR-018](adr/018-optional-model-discovery.md) for architecture and tradeoffs.
