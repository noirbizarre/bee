# Inference and smoke tests

Bee implements text and function-calling Chat Completions. Responses and Anthropic are configuration-only until their
backends land. A profile's preferred protocol is used exactly; failures never trigger protocol fallback. Any opaque
model ID is accepted without a catalog lookup, but the endpoint must support that model and the selected protocol.

## Request settings and supported subset

- `timeout_ms`: positive 32-bit integer, default 120000, for each HTTP exchange including streamed body reads.
- `max_output_tokens`: positive 32-bit integer, default 4096. Chat maps this through `options.max_tokens_field`.
- Configured base paths are preserved. Chat appends `/chat/completions` to its base URL.
- Text, streamed function calls, optional usage, and ordinary JSON answers are supported. Chat supports one choice,
  modern function tools, and `max_tokens` or `max_completion_tokens`. Legacy functions, audio and other meaningful
  unsupported output fail explicitly. Compatible servers need only implement this practical subset.
- Custom headers cannot override generated authentication, content type, accept, host, framing or protocol headers.
  With no credential source, a custom authentication header is permitted. Headers are compared case-insensitively.
- Redirects and automatic retries are disabled. A local configured HTTP endpoint is supported; login exchanges remain
  HTTPS-only. Diagnostics classify failures without repeating response bodies, URLs or header values.

Streams must have a valid terminal lifecycle. Truncated streams are errors, not successful partial answers. Each wire
event/tool argument is limited to 1 MiB and each response to 16 MiB. The loop permits sixteen model requests and
sixty-four tool calls per turn, with sequential execution and no automatic retry after side effects.

Successful conversation context is retained within the ACP connection, including close/resume. Cancelled, failed and
refused turns may already have visible text but are excluded from future context. Changing provider/model preserves
portable text and drops protocol-specific continuation. This does not implement ACP update replay or disk persistence.

The SDK-free `agent::run` accepts tool definitions and an injected `Executor`, with correlated call/result history.
This is exercised against local HTTP servers. **Production ACP does not discover or execute tools yet**: no tool
definitions are sent, and unsolicited calls fail clearly. Cancelling an injected executor drops its future; an executor
must cooperate with cancellation, and external side effects cannot be rolled back.

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

For a third-party/local compatible server, repeat using its documented base URL and model. For example:

```toml
[providers.local]
model = "YOUR_LOCAL_MODEL_ID"
[providers.local.protocols.openai-chat]
base_url = "http://127.0.0.1:11434/v1"
```

Run official and compatible endpoint checks separately; passing local fixtures does not claim compatibility with
every server or model. Live tests require user-supplied credentials and are not part of automated CI.
