# Inference and smoke tests

Bee implements text and function-calling Chat Completions, Responses and Anthropic Messages.
A profile's exact `model_protocols` mapping wins, otherwise its preferred protocol is used; failures never trigger
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
- Text, streamed function calls, optional usage, and ordinary JSON answers are supported. Chat supports one choice,
  modern function tools, and `max_tokens` or `max_completion_tokens`. Legacy functions, audio and other meaningful
  unsupported output fail explicitly. Compatible servers need only implement this practical subset.
- Custom headers cannot override generated authentication, content type, accept, host, framing or protocol headers.
  With no credential source, a custom authentication header is permitted. Headers are compared case-insensitively.
- Redirects and automatic retries are disabled. A local configured HTTP endpoint is supported; login exchanges remain
  HTTPS-only. Diagnostics classify failures without repeating response bodies, URLs or header values.

Streams must have a valid terminal lifecycle. Truncated streams are errors, not successful partial answers. Each wire
event/tool argument is limited to 1 MiB and each response to 16 MiB. Tools execute sequentially with no automatic retry
after side effects.

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

Successful conversation context is retained within the ACP connection, including close/resume. Cancelled, failed and
refused turns may already have visible text but are excluded from future context. Changing provider/model preserves
portable text and drops protocol-specific continuation. This does not implement ACP update replay or disk persistence.

The SDK-free `agent::run` accepts tool definitions and an injected `Executor`, with correlated call/result history.
The [native tool contract](tools.md) provides typed schema validation, registration, bounded results, and
`agent::NativeExecutor` for this seam.
Normal ACP sessions advertise and execute the shipped `bash`, `read`, `glob`, `list`, `write`, and `edit` tools
through this
same interface.
They resolve relative paths against the session cwd, while configuration is loaded from the launch directory.
Cancelling or closing a session drops pending inference and tool futures and signals native-tool cancellation tokens.
Executors must cooperate with cancellation, and external side effects cannot be rolled back.
Library callers can use `agent::run_with_limits` for explicit policy or `acp::Runtime` for a custom catalog and policy.

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
