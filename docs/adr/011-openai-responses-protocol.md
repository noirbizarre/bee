# ADR-011: Add `openai-responses` as a protocol of its own

## Status

Accepted. Extends the "Names" section of [ADR-010](010-several-protocols-per-provider.md).

## Context

ADR-010 anticipated that OpenAI's second API would get its own name, `openai-responses`,
and that `openai-chat` would keep meaning Chat Completions. The Responses API is now what
OpenAI recommends for new work, and several vendors serve it: OpenAI, xAI, DeepSeek, Groq,
OpenRouter and OpenCode Zen all document it, and OpenCode Zen serves its GPT, Grok and Muse
models *only* over it. With two protocols, those models were unreachable by configuration.

Models.dev expresses the same split as the AI SDK package a model uses:
`@ai-sdk/openai` is the Responses API and `@ai-sdk/openai-compatible` is Chat Completions.

## Decision

`openai-responses` is a third protocol (`Protocol::OpenAiResponses`).

- **A protocol, not an option of `openai-chat`.** It is a different endpoint (`/responses`)
  with different request and event shapes, and it can be served by a vendor that does not
  serve Chat Completions, or the other way round. An option could not say that, and a profile
  lists protocols precisely so each can have its own URL and settings (ADR-010).
- **Default base URL** is OpenAI's, `https://api.openai.com/v1`, the same as `openai-chat`: the
  URL includes the version and the backend appends `/responses`.
- **No options yet.** The output limit is always `max_output_tokens`, so there is nothing to
  switch. An `options` table under it is accepted when empty and refused with
  `bee::provider::unknown_key` otherwise, the diagnostic saying the table accepts no keys.
- **Presented like Chat.** `Authorization: Bearer`.
- **Ordering.** It is inserted between the two existing protocols in `Protocol::ALL`, which
  `prefer` uses to order the protocols it leaves out. `openai-chat` still comes before
  `anthropic-messages` there, so a configuration that did not name the new protocol resolves as
  before; the only profiles whose result can change are ones that list it.

Like the other two, it is configuration only: no backend exists for any protocol yet. Chat is
[#8](https://github.com/noirbizarre/bee/issues/8) and Messages is
[#23](https://github.com/noirbizarre/bee/issues/23); the Responses backend has no issue yet.

### Presets

A shipped preset lists `openai-responses` only where the vendor's own documentation shows it,
at the URL it shows. `openai` and `xai` prefer it, because their documentation is written
against it. `openrouter`, `groq`, `deepseek` and `opencode` list it but keep Chat preferred:
OpenRouter's Responses API is a beta, and on OpenCode Zen Chat reaches the most models
(including the free ones) until a protocol can be chosen per model.

### Choosing per model is not this decision

A gateway such as OpenCode Zen serves some models over Responses, some over Messages and the
rest over Chat, and today `prefer` picks one protocol for the whole profile. Choosing per model
is recorded in [#24](https://github.com/noirbizarre/bee/issues/24), with the finding that the
signal in Models.dev is a model's `provider.npm`, **not** its `family`: one family appears
under several protocols inside one provider (`grok`, `minimax` and `qwen` on OpenCode Zen).

## Consequences

- Any model OpenCode Zen offers over Responses, Messages or Chat is now reachable by
  configuration; only its Gemini models, which use Google's native protocol, are not.
- A profile that lists all three protocols needs `prefer`, as any profile with several does.
- A backend for Responses is a separate piece of work, with its own streaming and tool-call
  shapes, and is what makes the protocol do anything.

## Alternatives rejected

- **`openai-chat` with a `responses = true` option.** One URL and one set of settings for two
  endpoints, and no way to list a vendor that serves only one.
- **Waiting for the backend.** The name and its place in the catalogue cost nothing to add and
  let presets describe what vendors actually serve; the backend can then be added without a
  configuration change.
