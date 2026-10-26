# Inference and smoke tests

Bee implements text and function-calling Chat Completions, Responses and Anthropic Messages.
A profile's exact `model_protocols` mapping wins, followed by explicitly enabled catalog hints, then its preferred
protocol; failures never trigger
protocol fallback. Any opaque
model ID is accepted without a catalog lookup, but the endpoint must support that model and the selected protocol.

For example, a profile using `preset = "openai"` can set
`model_protocols = { "YOUR_CHAT_MODEL_ID" = "openai-chat" }` while keeping Responses as its preference.
The effective session-selected or default model ID controls dispatch, endpoint URL/options and authentication headers
together. The provider's credential, custom headers and common limits are shared. See
[provider configuration](index.md#providers-and-models) for validation and layer/preset merge rules.

## Request settings and supported subset

- `timeout_ms`: positive 32-bit integer, default 120000, for each HTTP exchange including streamed body reads.
- `max_output_tokens`: positive 32-bit integer, default 4096. Chat maps this through `options.max_tokens_field`.
- Configured base paths are preserved. Chat appends `/chat/completions` to its base URL.
- Responses appends `/responses` and uses `max_output_tokens`. It uses stateless, client-managed continuation
  (`store = false`) with native output items and encrypted reasoning state. Function outputs correlate by call ID,
  not output-item ID. Built-in server tools and non-text message content are unsupported. Streams require item completion
  followed by a completed/incomplete response; usage and final objects never duplicate streamed text.
- Anthropic appends `/v1/messages`, presents credentials with `x-api-key`, and sends `anthropic-version` from
  `options.version` or the default `2023-06-01`. Its `max_tokens` comes from `options.max_tokens` if present, else the
  common `max_output_tokens`. Native text/tool-use blocks and grouped user tool-result blocks retain their order and
  error flags. Thinking, multimodal blocks, server tools and pause-turn continuation are explicitly unsupported.
- Text, streamed function calls, optional usage, and ordinary JSON answers are supported. Chat requests streamed usage
  with `stream_options.include_usage`; compatible servers may omit the counters. Chat supports one choice,
  modern function tools, and `max_tokens` or `max_completion_tokens`. Legacy functions, audio and other meaningful
  unsupported output fail explicitly. Compatible servers need only implement this practical subset.
- Custom headers cannot override generated authentication, content type, accept, host, framing or protocol headers.
  With no credential source, a custom authentication header is permitted. Headers are compared case-insensitively.
- Redirects and automatic retries are disabled. A local configured HTTP endpoint is supported; login exchanges remain
  HTTPS-only. Diagnostics classify failures without repeating response bodies, URLs or header values.

Streams must have a valid terminal lifecycle. Truncated streams are errors, not successful partial answers. Each wire
event/tool argument is limited to 1 MiB and each response to 16 MiB. Tools execute sequentially with no automatic retry
after side effects.

## ChatGPT plan requests

The `chatgpt` preset uses the public `POST https://api.openai.com/v1/responses` endpoint with a renewable OAuth bearer
token. It sends full client-managed history, `store: false`, `stream: true`, encrypted reasoning continuation, and
function tools grouped in the `bee` namespace. Client-hosted MCP tools remain client-executed function tools.
Only `response.completed` establishes success; late usage-limit failures and incomplete/interrupted streams fail the
turn even when partial text was visible.

OpenAI's current preview forbids `max_output_tokens` on this route. Bee omits it but retains the configured value as a
local context reservation; **it is not a server-enforced generation cap for ChatGPT-plan requests**.
Bee's step/tool/response-size limits still apply. Other unsupported preview fields, hosted tools and persistent
`previous_response_id` continuation are not sent. Custom endpoints, additional protocols and custom headers cannot be
combined with this OAuth mechanism. See the current
[provider limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).

Eligibility, permission and usage errors stop the request without protocol or billing fallback.
Usage-limit recovery links to [ChatGPT usage settings](https://chatgpt.com/settings/usage).
Tokens are refreshed before each model step; protocol, model and context selection remain frozen for the turn.
See [setup and logout](index.md#use-your-chatgpt-plan).

## Prompt caching

Bee supports provider-native prompt caching, not a local response cache. Eligibility, minimum prompt length, lifetime,
billing and usage reporting depend on the endpoint and model. A configured key or breakpoint **never guarantees a cache
hit**. Caching reuses input computation; it does not reuse an earlier answer or change generation semantics.

Ordinary turns append messages and tool results. Unchanged instructions, tool definitions and their production ordering
remain deterministic. Scoped instruction/skill changes, changed tools, model/protocol selection and oldest-whole-turn
eviction can legitimately invalidate a prefix. Bee does not suppress fresh guidance to retain a cache hit.

### Capability checklist

Audited against the linked official contracts on 2026-10-26. This is Bee's intentionally supported subset, not a promise
that every endpoint implementing a wire protocol accepts every option. URL overrides are checked as the actual endpoint,
regardless of preset or connection name. Optional catalog prices and metadata are not capability evidence.

| Endpoint / adapter | Keys and native request controls in Bee | Usage preserved / limitations |
| --- | --- | --- |
| [OpenAI Responses][openai-cache] | Automatic stable key; opt-in older-model retention or newer-model mode, TTL and text breakpoints | Cached reads, writes and returned 30m/12h breakdown; model checks below |
| [OpenAI Chat][openai-chat-cache] | Automatic stable key; opt-in older-model retention or newer-model mode, 30m TTL and text breakpoints | OpenAI-shaped details; 12h TTL is restricted to Responses on direct OpenAI |
| [ChatGPT OAuth][chatgpt-cache] / Responses | Automatic stable key and explicit key/opt-out | Retention is forbidden by the preview; new write policies are rejected until route support is verified |
| [Anthropic Messages][anthropic-cache] | Opt-in automatic, stable-prefix or combined ephemeral caching; 5m/1h | Separate read/creation counts and returned 5m/1h breakdown; no markers by default |
| [OpenRouter][router-cache] / Chat and Responses | Key is opt-in because it changes sticky routing; new OpenAI controls only for verified `openai/` models | Native OpenAI-shaped read/write details; no legacy retention or cross-provider TTL translation |
| [OpenRouter][router-cache] / Messages | Anthropic policies on verified `anthropic/` models, using base `https://openrouter.ai/api` | Native Anthropic details; use this protocol for explicit Anthropic TTLs |
| [xAI][xai-cache] / Responses | Automatic stable key; no OpenAI retention or write-policy controls | OpenAI-shaped cached reads when reported |
| [xAI][xai-cache] / Chat | No added body controls; existing custom headers can set `x-grok-conv-id` explicitly | Header-based routing differs from Responses; cached reads when reported |
| [Mistral][mistral-cache] / Chat | Automatic stable key or explicit override/opt-out; no retention or breakpoint controls | `prompt_tokens_details.cached_tokens`; minimum cache block is model/provider dependent |
| [DeepSeek][deepseek-cache] / supported wire adapters | Provider-managed automatic caching; no added controls | Native hit/miss counters plus OpenAI/Anthropic-shaped details when returned |
| [Groq][groq-cache] / Chat and Responses | Provider-managed automatic caching on its supported models; no added controls | Cached reads when reported; provider does not expose manual management |
| [Google OpenAI compatibility][google-cache] / Chat | No added controls: Bee has no verified cache-control contract for this compatibility route | Preserve returned OpenAI-shaped details; Google's native cache-resource API is a different protocol |
| [OpenCode][opencode-cache] / all three adapters | No added controls: listed cache pricing does not establish forwarding guarantees | Preserve returned native details without assuming upstream control support |
| [Ollama][ollama-cache] / Chat, or custom endpoints | No added controls without a verified native contract | Compatibility field lists do not promise these controls; preserve recognized returned usage |

[openai-cache]: https://developers.openai.com/api/docs/guides/prompt-caching
[openai-chat-cache]: https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create
[chatgpt-cache]: https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations
[anthropic-cache]: https://platform.claude.com/docs/en/build-with-claude/prompt-caching
[router-cache]: https://openrouter.ai/docs/guides/best-practices/prompt-caching
[xai-cache]: https://docs.x.ai/developers/advanced-api-usage/prompt-caching/maximizing-cache-hits
[mistral-cache]: https://docs.mistral.ai/studio/conversations/advanced/prompt-caching
[deepseek-cache]: https://api-docs.deepseek.com/guides/kv_cache
[groq-cache]: https://console.groq.com/docs/prompt-caching
[google-cache]: https://ai.google.dev/gemini-api/docs/openai
[opencode-cache]: https://opencode.ai/docs/zen/
[ollama-cache]: https://docs.ollama.com/api/openai-compatibility

Default key derivation is intentionally omitted on unverified endpoints and on OpenRouter. Explicit `mode = "auto"` or
`mode = "explicit"` requires key support and fails otherwise. Any explicit unsupported retention, write policy or TTL
fails before HTTP with `bee::llm::cache`; Bee never retries after stripping it or changes protocol/billing route.

### Stable keys and OpenAI controls

The automatic `bee-v1-...` key hashes the grouping profile, connection name, effective endpoint, protocol, authentication
route class and model. It contains no credentials, prompt content, paths, timestamps or session/request IDs. The same
scope survives model reconstruction, credential refresh, resume and process restart; changing scope/connection/model
changes the key. Keys group routing/accounting, not authentication or account isolation. Hashing does not make a key an
anonymity guarantee. Embedded `Model` callers can set a stable grouping scope with `with_cache_scope` (default `default`).

```toml
[providers.openai]
preset = "openai"
model = "gpt-6.1-sol"

[providers.openai.protocols.openai-responses.options]
# Omit this table for the default automatic key; explicit values are 1–64 bytes.
prompt_cache_key = { mode = "explicit", value = "my-application-v1" }
# Paid writes and TTL selection are opt-in. Provider defaults remain active without this table.
prompt_cache_options = { enabled = true, mode = "explicit", ttl = "30m", breakpoints = "instructions-and-conversation" }
```

`prompt_cache_options.mode` is `implicit` (default within an enabled table) or `explicit`. TTL may be omitted or set to
`default` to preserve the native short lifetime. Responses supports `30m` and `12h`; direct OpenAI Chat supports `30m`,
while OpenRouter documents both. Longer minimum lifetime costs more and is not a deletion deadline. Supported newer
aliases are `gpt-5.6`, `gpt-5.6-pro`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-6`, `gpt-6-astra`,
`gpt-6-sol`, `gpt-6-luna` and `gpt-6.1-sol`, plus their date snapshots. Unknown models remain usable with default requests;
explicit model-specific controls require a verified alias. OpenRouter uses these with the `openai/` prefix.

`breakpoints` is a Bee placement setting, not a forwarded API field: `none`, `instructions`, `conversation`, or
`instructions-and-conversation`. Instruction caching in Responses sends the payload once in a leading developer text
block instead of top-level `instructions`. Conversation caching retains a bounded window of recent nonempty user-text
and completed tool-result-group boundaries, including the newest one. Earlier write endpoints remain available for
explicit-mode lookups as the conversation grows. Markers are request-local, never persisted in native continuation.
The window uses the remaining four-write budget after reserving slots for an enabled implicit boundary and selected
instruction boundary. Missing requested boundaries fail clearly. `mode = "explicit"` with
`breakpoints = "none"` disables caching on supporting models; `prompt_cache_key.mode = "disabled"` only omits the key.

Newer OpenAI models charge for implicit writes even without additional controls. Bee's default key does not opt into a
new write/retention policy, but does not disable that provider default. Current newer-model cache minimum is 1,024 visible
tokens; pricing and lookup windows vary and should be checked in the official guide. Prefix retention and latency gains
are not guaranteed.

For older OpenAI API-key models, set `prompt_cache_retention = "in_memory"` or `"24h"` instead of enabling new options.
`"default"` omits the field and resets an inherited choice. Bee accepts extended retention for the officially listed
`gpt-5.5`, `gpt-5.5-pro`, `gpt-5.4`, `gpt-5.2`, `gpt-5.1-codex-max`, `gpt-5.1`, `gpt-5.1-codex`,
`gpt-5.1-codex-mini`, `gpt-5.1-chat-latest`, `gpt-5`, `gpt-5-codex` and `gpt-4.1`, including date snapshots.
These also accept in-memory retention except `gpt-5.5`/`gpt-5.5-pro`, which require 24h. In-memory-only aliases additionally
include `gpt-4o`, `gpt-4o-mini`, `gpt-4.1-mini`, `gpt-4.1-nano`, `gpt-5-mini`, `gpt-5-nano`, `o1`, `o3`, `o3-mini`,
and `o4-mini`. Retention support is independent of `store = false`; organization data-retention policies still apply.

### Anthropic policies

```toml
[providers.claude]
preset = "anthropic"
model = "claude-sonnet-4-6"

[providers.claude.protocols.anthropic-messages.options]
cache_control = { policy = "stable-prefix-and-automatic", ttl = "5m" }
```

Policies are `disabled` (default), `automatic` (top-level native `cache_control`), `stable-prefix` (last system text block,
or last tool definition if no instructions exist), and `stable-prefix-and-automatic`. Enabled policies default to `5m`;
select `1h` explicitly for extended, higher-priced writes. One TTL applies to both boundaries, avoiding mixed-TTL ordering
ambiguities. The combined policy uses two of the four available slots. A requested stable prefix needs nonempty
instructions or at least one tool.

Verified aliases cover Sonnet 4/4.5/4.6/5/5.5, Opus 4/4.1/4.5/4.6/4.7/4.8/5/5.5, Haiku 4.5/5.5, Fable 5/5.1 and
Mythos 5/5.1 with the native hyphenated spelling and optional YYYYMMDD suffix. Retired models may still be rejected by the
provider. Minimum cacheable lengths vary by model. Prefixes are ordered tools → system → messages. The provider's
20-position lookback can miss an older boundary after a large addition; this baseline policy does not promise optimal
reuse for every long tool batch. Empty text cannot be marked. Provider-native continuation and tool-result grouping are
preserved. OpenRouter uses its documented dotted model versions, such as `anthropic/claude-sonnet-4.6`, rather than the
native `claude-sonnet-4-6` spelling; Bee normalizes capability lookup without rewriting the wire model ID.

### Usage visibility

`llm::Usage.cache` preserves only returned counters in native `CacheUsage` families: OpenAI-shaped read/write/TTL
details, Anthropic read/creation/TTL details, and DeepSeek hit/miss details. Each `CacheCounter` keeps optional tokens and
the number of model steps reporting it, so absent and zero remain distinct and partial turn totals stay identifiable.
Streaming snapshots replace cumulative counters rather than adding each event. Completed steps aggregate with
saturating arithmetic. This metadata is available to embedded callers and later instrumentation; ACP cost dashboards are
separate work.

OpenAI input totals already include cache tokens. Anthropic input totals are normalized as uncached + creation + reads.
Write breakdowns are components of the write total, not additional input. Bee never infers a hit from a request policy,
elapsed time or a catalog price, and no live cache hit is required by the credential-free regression tests.

## Turn execution limits

The layered configuration accepts positive integer budgets, with these defaults:

```toml
[agent]
max_steps = 16
max_tool_calls = 64
```

Each step is one model request, including the final answer. Tool calls count individually across all batches, and both
budgets reset per turn. A final answer on the last permitted step succeeds. A tool batch on that step fails before
execution because no model request remains to consume its results. Over-budget batches likewise execute no calls.
Unknown settings, invalid types and nonpositive budgets fail at startup; layers override each setting independently.
These turn budgets are separate from per-request timeouts, token limits and response-size bounds.

Successful conversation context is durably retained across close/resume and production process restarts. Cancelled,
failed and refused turns may already have visible text but are excluded from future context. Changing provider/model
preserves portable text and drops protocol-specific continuation. Explicit or opted-in catalog context windows bound
history as described below. A separate visible transcript retains accepted user messages and assistant partials,
even from unsuccessful turns, independently of context eviction. See [session history and replay](sessions.md).

The SDK-free `agent::run` accepts tool definitions and an injected `Executor`, with correlated call/result history.
The [native tool contract](tools.md) provides typed schema validation, registration, bounded results, and
`agent::NativeExecutor` for this seam.
Normal ACP sessions advertise and execute the shipped `bash`, `read`, `glob`, `grep`, `list`, `write`, and `edit` tools
through this
same interface.
The boundary also supplies [`update_plan`](tools.md#execution-plans) to publish
structured turn plans across all three provider protocols. Its definition and
correlated call/results count toward model context and normal turn limits;
`acp::plan_tool()` exposes its definition for host-side budget estimates.
The ACP boundary requests client permission before Bash execution and adds `ask_user`
when form elicitation is negotiated. Missing/expired stored credentials can initiate
URL-based browser login before the first model request. See
[ACP user interactions](tools.md#acp-user-interactions) for outcomes and cancellation.
Client-hosted [MCP tools](tools.md#client-hosted-mcp-tools) join this catalog through the ACP boundary's composite
executor. Catalog discovery runs on each prompt; permission denials and invocation failures are correlated tool
results. Complete MCP result/error envelopes are JSON strings in each provider's native tool-result representation.
They resolve relative paths against the session cwd, while configuration is loaded from the launch directory.
Cancelling or closing a session drops pending inference and tool futures and signals native-tool cancellation tokens.
Executors must cooperate with cancellation, and external side effects cannot be rolled back.
Library callers can use `agent::run_with_limits` for explicit policy or `acp::Runtime` for a custom catalog and policy.

## Conversation context and token budgets

Each accepted ACP prompt becomes one user message: text blocks and resource-link descriptions are joined in order
with newlines. Links are represented as `[Resource link, contents not fetched: URI]`; Bee does not fetch their contents.
Unsupported block types are rejected before the turn starts. Successful history includes assistant text, complete
function calls, their correlated results and any protocol-native continuation. Output-limited answers are retained;
failed, cancelled, refused and context-exhausted turns are not.

Context capacity is explicit configuration by default, never inferred from a model name:

```toml
[providers.work]
context_window = 32768
model_context_windows = { "small-model" = 8192 }
```

An explicitly enabled [catalog context window](models.md#optional-protocol-hints-and-context-budgets) can supply a
fallback below both explicit settings. Catalog budgets and protocol hints freeze at prompt acceptance; stale validated
cache remains usable with visible provenance. Neither a download nor an endpoint listing is required before inference.

Both settings accept positive 32-bit integers, including in presets. An exact, case-sensitive model entry overrides
the profile fallback. Quote model IDs containing TOML punctuation. The map merges per model across configuration
layers and presets; an empty map does not clear inherited entries. Without an applicable limit, capacity is unknown:
Bee estimates occupancy but does not truncate or invent a context window. A profile fallback applies to every unmapped
model selected in that profile, so configure it appropriately for those models.

Before **every model request**, including requests following tool execution, Bee requires:

```text
estimated input tokens + reserved output tokens <= effective context window
```

Input is estimated from the UTF-8 byte lengths of compact JSON for the selected protocol's conversation and tool
definitions: sum those lengths, divide by three rounding up, then add 256 tokens for framing. This includes tool
schemas, descriptions, call IDs, arguments and results. Native items are counted instead of their portable replacement,
not in addition to it. The estimate is approximate, especially for multilingual text, JSON and encrypted reasoning;
it is not a tokenizer guarantee. Output reservation uses the actual request limit, including Anthropic's
`options.max_tokens` override. Bee does not silently lower output limits to make a request fit.

When necessary, Bee drops oldest **complete ACP turns** until the request fits. Each dropped user message takes all
its assistant steps, tool calls/results and native continuation with it. Recent retained turns remain unchanged.
The active prompt and its entire in-progress tool loop are protected: individual messages, results and opaque native
items are never shortened or split. If the protected turn plus tool definitions and output reservation cannot fit,
the turn fails with `bee::llm::context_limit` before another HTTP request. A tool may already have performed side
effects before its result makes the next request too large; context management cannot roll those effects back.

Truncation changes the working copy only. Successful non-refused completion commits that reduced model history;
failure, cancellation or refusal leaves previously committed context intact. This bounds model history when configured,
without summarizing removed interaction or rewriting the separate client-visible transcript. Production sessions retain
visible history and successful model context across close/resume and process restarts; whole-turn context eviction does
not delete visible messages. Embedded runtimes default to memory storage unless history is explicitly enabled.
See [transcript versus model context](sessions.md#transcript-versus-model-context) for retention and replay boundaries.

### Usage and overflow reporting

After a successful commit with an effective capacity, ACP receives `usage_update` before terminal idle. `used` is
the estimate for retained context including tool definitions and the final answer, excluding future output reservation;
`size` is the effective window. `_meta["bee.dev/context"]` labels `estimated = true`, the method
`utf8-json-bytes/3+256`, `reservedOutput`, and the enforced `window` fact's source/confidence/freshness.
A final answer can change the estimate beyond the preflight budget;
the next request is checked again. If turns were dropped, an informational notice reports their count. These are
live context reports, not replayed messages, billing totals or generated summaries. Unsuccessful attempts emit no
committed-context report or truncation notice.

Library callers can inspect `Model::context` and `agent::Outcome::context` even when capacity is unknown.
`Model::step` checks a configured budget without modifying its input; the agent loop owns eviction. Library inputs
use user-message boundaries to identify turns; without any user boundary the entire input is protected. Malformed
historical call/result batches cannot be silently discarded to fit a budget.

`Outcome::usage` sums known provider input/output counters with saturating arithmetic. Missing counters are distinct
from zero; `context.steps`, `context.input_steps` and `context.output_steps` indicate whether each total covers every
completed request. These repeated-request totals are separate from current context occupancy. Failed/cancelled attempts
do not return an outcome, and no billing ledger or monetary cost is inferred from estimates.

Provider context overflow is explicit even without configured capacity. Bee recognizes structured
`context_length_exceeded` errors, Anthropic `invalid_request_error` messages beginning with `prompt is too long`, and
Anthropic's `model_context_window_exceeded` stop reason. HTTP, stream and terminal error paths retain the same safe
classification without echoing upstream text. Generic HTTP 400/413 or unrelated invalid requests are not assumed to
be context overflow. A normal output-token stop stays `max_tokens`; a context-exhausted turn fails even if partial
text was already streamed. Shorten the prompt, start a new session, or correct the configured window/output limit;
Bee does not automatically retry a rejected request.

See [ADR-014](adr/014-conversation-context-and-token-limits.md) for the design decision.

## Chat Completions manual smoke test

Use an ACPv2 client to launch `bee` from a directory with this configuration, supplying your own key and a model that
supports Chat Completions. The example deliberately selects Chat even if a preset's default is Responses:

```toml
[providers.smoke]
preset = "openai"
prefer = ["openai-chat"]
model = "YOUR_CHAT_MODEL_ID"
timeout_ms = 120000
```

1. Supply `OPENAI_API_KEY` in the client's launch environment, or configure a stored credential with `bee auth login`.
2. Initialize, create a session and prompt for a short answer. Expect running, streamed text, then idle/end-turn.
3. Ask a follow-up referring to the answer. Check that context is retained.
4. Submit a longer prompt and cancel after output begins. Expect exactly one cancelled idle and no later chunks.
5. Ask the model to discover files, list a directory and read a text file in the session directory. Expect a final
   answer using the tool results; a follow-up can refer to those results.
6. Ask it to create a new text file, edit a unique fragment and read the result. Verify that the file in the session
   directory contains the requested changes.

For a third-party/local compatible server, repeat using its documented base URL and model. For example:

```toml
[providers.local]
model = "YOUR_LOCAL_MODEL_ID"
[providers.local.protocols.openai-chat]
base_url = "http://127.0.0.1:11434/v1"
```

Run official and compatible endpoint checks separately; passing local fixtures does not claim compatibility with
every server or model. Live tests require user-supplied credentials and are not part of automated CI.

## Responses manual smoke test

Repeat the text, follow-up and cancellation checks above with an OpenAI model supporting Responses:

```toml
[providers.smoke]
preset = "openai"
model = "YOUR_RESPONSES_MODEL_ID"
```

The OpenAI preset prefers Responses, so no preference override is needed. For a compatible gateway, use its documented
Responses base URL and credentials explicitly:

```toml
[providers.compatible]
model = "YOUR_RESPONSES_MODEL_ID"
credential = { env = "COMPATIBLE_API_KEY" }
[providers.compatible.protocols.openai-responses]
base_url = "https://YOUR_COMPATIBLE_HOST/v1"
```

Supply the key only in the launch environment. Verify both endpoints independently, including a multi-turn follow-up.
Some compatible endpoints do not support stateless encrypted reasoning; those fail explicitly rather than losing state.

## Anthropic Messages manual smoke test

Supply your own `ANTHROPIC_API_KEY` in the ACP client's launch environment and choose a Messages-compatible model:

```toml
[providers.smoke]
preset = "anthropic"
model = "YOUR_ANTHROPIC_MODEL_ID"
max_output_tokens = 4096
```

Repeat the short answer, follow-up and cancellation checks against the official endpoint. The preset uses
`https://api.anthropic.com`; Bee appends `/v1/messages` and applies the required protocol headers. Do not enable thinking
or server tools for this subset. A Messages-compatible gateway may override this protocol's base URL without changing
its provider name or authentication source. Validate such gateways separately against their own documented contract.
