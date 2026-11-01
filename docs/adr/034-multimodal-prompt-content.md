# ADR-034: Gate portable multimodal prompts by model declarations and native routes

## Status

Accepted. Supersedes ADR-005's text-only/unknown-content restriction and the display-only interpretation of input
modalities in ADR-018. Extends ADR-019's journal contract with version 7.

## Context

Issue [#22](https://github.com/noirbizarre/bee/issues/22) requires incoming images, audio and embedded resources to reach
inference. ACP initializes once before session/model selection and asynchronous discovery. Models and provider routes
within one connection have different input contracts. A wire adapter's ability to serialize media does not authorize
every model to consume it. Model context, visible replay, caching and turn eviction must retain their separate lifecycles.

## Decision

Keep existing `Message::User(String)` serialization and add an ordered SDK-free `UserParts` variant. A multipart user
message is one turn boundary. Original ACP blocks remain the visible transcript; only normalized understood input enters
successful context. Unknown/custom content remains opaque and never enters inference as instructions.

Advertise the union of implemented transports across active configured routes at `capabilities.session.prompt`.
Embedded text resources are supported independently of binary modalities. Injected deterministic turn bodies do not
advertise media. Advertisements do not select a model or imply universal model support.

Effective `input_modalities` follows exact-model > account > native > public > unknown precedence. Lists replace rather
than union; explicit empty lists revoke inherited binary support. Require `image`, `audio` or `pdf` respectively and
intersect with provider/route support. Unknown is text-only. Freeze this decision alongside routing and context budgets
after availability and authentication. Input metadata cannot expand account availability or override transport restrictions.

Chat, Responses and Messages serialize inline images and PDFs in their respective native shapes. Chat additionally
implements WAV/MP3 audio inputs. ChatGPT-plan and Copilot support images only, with a runtime-owned Copilot vision header
derived from the full outgoing history. Cloudflare REST, Vertex media and Mistral PDF shapes remain unimplemented in
these adapters.
Embedded binary resources need supported MIME types; supplied text becomes contextual user text. Never fetch resource or
image URIs, upload files implicitly, or derive provider filenames from local paths.

Validate format, encoded/decoded sizes, advertised transport and definitive configured policy before insertion. Where
authority/discovery may change the decision, revalidate after acceptance and use ordinary failed-turn cleanup. Check
retained successful attachments too: incompatible model changes fail explicitly without dropping content.

Binary limits are 32 parts, 8 MiB decoded each, 16 MiB decoded per prompt and a 24 MiB serialized media/resource prompt
envelope. Anthropic images additionally require at most 5 MiB encoded. Validate base64 and MIME envelopes; leave full file
decoding to providers. Count serialized envelope bytes without allocating a second oversized payload. Errors and Debug
never include content. Journal v7 persists inline multipart context; versions 1–6 remain readable, and multipart context
cannot appear under an older record version. Existing 64 MiB per-record and cumulative replay bounds still apply.

Retain approximate serialized-byte context accounting and transactional oldest-whole-turn eviction. Mark documented cache
boundaries on trailing text parts; media-only OpenAI-style turns skip unsupported conversation markers while retaining
instruction caching. Preserve native continuation and tool-result correlation.

## Consequences

- Valid inputs survive tool steps, later prompts and restarts without flattening or reordering media.
- Clients may encounter an incompatible selected model even when the connection advertises a capability. Errors describe
  selecting a compatible model, declaring accurate input metadata or starting a fresh session for incompatible history.
- Base64 byte accounting may reject valid attachments earlier than provider token accounting would. It is approximate,
  not a guaranteed media-token upper bound. Improving modality-aware estimates is independent work.
- Inline payloads occur in both visible history and successful context. Conservative prompt envelopes leave room for
  outputs, while exact journal encoding still rejects oversized terminal records transactionally.
- Older binaries reject new journals. Rich MCP/tool results and generated media remain separate features.

Union advertisement avoids suppressing vision for an entire connection because one selectable model is text-only.
Metadata gating avoids optimistic requests against unknown models. Retaining text serialization avoids rewriting existing
journals and text-only embedding call sites; embedders with exhaustive message matches must handle the new variant.
