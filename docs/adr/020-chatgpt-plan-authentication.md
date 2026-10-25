# ADR-020: Official ChatGPT-plan OAuth for local applications

## Status

Accepted. Extends ADR-008/009/017 and qualifies ADR-014's wire output-limit behavior for ChatGPT-plan inference.
Implements [#53](https://github.com/noirbizarre/bee/issues/53).

## Context

OpenRouter's PKCE-key flow returns an API key. ChatGPT-plan access requires a dedicated public-client OAuth
registration, verified OIDC identity, rotating renewable credentials and account-specific authorization.
Conflating those flows would lose registration identity or silently change the request's billing audience.

On 2026-10-25, OpenAI's official documentation explicitly supported direct public Responses inference for
[open-source and locally hosted applications](https://developers.openai.com/siwc/token-sharing-open-source).
The documentation separately directs paid/remotely hosted applications to an approval path. Eligibility remains
subject to account/workspace/region/policy checks and applicable [OpenAI terms](https://openai.com/policies).

## Decision

Add a distinct `chatgpt` preset and fixed-endpoint OAuth mechanism. Follow official
[registration and sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in):
authorization-code PKCE S256, random state/nonce, literal IPv4 loopback callback, dynamic initial registration,
issued client ID on exchange and subsequent authorization, and verified ID-token signature/issuer/audience/expiry/nonce.
Request `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct` with resource
`https://api.openai.com/v1`. Access tokens remain opaque.

Named profiles label distinct account/workspace registrations. Persist the verified subject and issued client ID;
identity email alone cannot distinguish workspaces. Persist an opaque UUID host ID before the first authorization.
The Bee data directory represents one installation; restarts and logout do not create another host.

Extend the private credential store with typed OAuth metadata while continuing to read existing API-key stores.
All writers share a stable sibling OS lock across atomic replacement. Serialize refresh and commit replacement
access/refresh tokens, scope and expiry together. Pending login compares per-profile revision and existing credentials before
activation. Keep browser interactions cancellable and turn-owned, and omit optional ID-token URL hints so ACP clients
never receive OAuth tokens. Preserve validated sign-in with disabled plan usage if direct permission is absent.

Use the documented [account lifecycle](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions)
for renewal and logout/revocation. Resolve renewable credentials at every model-request boundary and before model
discovery. Temporary failures preserve credentials; terminal refresh errors clear unusable tokens and require sign-in.
Cancellation/transport loss after remote rotation can make the previous token unusable; reauthorization is recovery,
not an automatic retry of a potentially rotated grant. Logout retains registration/host and reports unconfirmed remote
revocation after local token removal.

Use only the documented [public inference route](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)
and its [preview constraints](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).
Retain Bee's Responses/tool loop with namespaces, stateless history and completion-only success. Omit forbidden
`max_output_tokens` while retaining local context headroom. Display ordered account-visible model slugs/names separately
from the public catalog. Classify allowlisted provider error codes without exposing response bodies.

## Consequences

- API-key/environment authentication and OpenRouter remain distinct, with no implicit billing fallback.
- Direct inference preserves Bee's own tool executor, permission policy, context and history lifecycle.
- Codex App Server delegation is unnecessary for this documented path and would introduce a competing agent/session
  lifecycle. Private Codex credentials and undocumented ChatGPT backend endpoints are excluded.
- Store updates require cross-process coordination; refresh holds the store lock through a bounded exchange.
- Unix private-file support remains the credential-store platform boundary.
- ChatGPT's output cap is a local reservation, not a server-enforced limit; execution/response bounds remain independent.
- Live eligibility cannot be established by mocks; deterministic tests verify protocol, lifecycle, concurrency and
  secrecy without requiring developer credentials.
