# ADR-021: Complete OpenRouter authentication using its documented PKCE-key flow

## Status

Accepted. Clarifies OpenRouter lifecycle assumptions in [ADR-008](008-provider-authentication.md) and completes
the data-driven mechanism in [ADR-009](009-provider-presets-and-login-flows.md). Implements
[#54](https://github.com/noirbizarre/bee/issues/54).

## Context

Bee already ships OpenRouter browser and pasted-code sign-in, a store-backed preset, and prompt-time browser login
through [ACP URL elicitation](017-acp-permission-and-elicitation.md). Issue #54 requires verifying official support
for third-party clients and documenting the actual credential lifecycle rather than reusing website sessions.

Official sources checked on 2026-10-26:

- [OAuth PKCE](https://openrouter.ai/docs/guides/overview/auth/oauth) explicitly describes user-authorized API-key
  provisioning, localhost callbacks on any port, headless authorization, state handling and exchange errors.
- [API authentication](https://openrouter.ai/docs/api/reference/authentication) describes bearer keys, credit limits,
  expiration for onboarding keys and revocation through the user's key settings.
- [Limits](https://openrouter.ai/docs/api/reference/limits) describes account/key budgets and rate limits.
- [Terms of service](https://openrouter.ai/terms), especially sections 2, 3, 5 and 10, cover eligibility, individual and
  organizational accounts, API credentials, model restrictions and third-party integrations. Eligibility includes
  being at least 18, not previously suspended/removed, and lawful use; organizational users also need authority and
  administrator-granted access. Applicable model terms and regional restrictions still apply.

The supported flow is intended for applications using their users' accounts and credits. The documented protocol
requires no client registration, client secret, scopes or audience parameters and states no special subscription
requirement. Authorization still depends on the user's eligible account and workspace access. This does not promise
access to every model or override account, organization or provider restrictions.

## Decision

### Keep the existing dedicated OpenRouter flow definition

Use `login_flows.openrouter` and the `openrouter` preset with the shared `pkce-key` mechanism. Browser authorization
uses `https://openrouter.ai/auth` with `callback_url`, `code_challenge`, `code_challenge_method=S256`, a random `state`
and `key_label=Bee`. The callback is an ephemeral loopback listener. OpenRouter's localhost attribution uses the host
and port rather than a marketplace application identity.

Headless authorization omits `callback_url` and state, retaining S256 PKCE. The user pastes the displayed code into
Bee; codes are single-use and expire after ten minutes. Bee waits five minutes for a browser callback and bounds
credential-exchange requests to 30 seconds. A browser must reach the agent host's loopback interface; remote clients
can instead initiate pasted-code login on that host.

Exchange the code using HTTPS JSON POST to `https://openrouter.ai/api/v1/auth/keys`, with `code`, `code_verifier` and
`code_challenge_method=S256`. Accept a nonblank string `key` from a successful response. Credential requests do not
follow redirects and response reads are bounded to one MiB in both CLI and ACP transports.

### State authenticates successful callbacks; an uncorrelated denial only aborts

OpenRouter returns state unchanged on successful authorization but explicitly omits it on denial. Every callback
that supplies a successful code must carry the expected state. An error-only callback with no state may terminate
the attempt as denied; it cannot exchange a code or write a credential. A supplied wrong state remains a mismatch,
including when accompanied by an error. A stateless callback containing both code and error is rejected.

An uncorrelated denial cannot be proven to originate at OpenRouter. Accepting it as an abort exposes no credential
and grants no authority; a local caller could already abort an attempt by sending a mismatched callback. Browser
pages and diagnostics use fixed text instead of reflecting callback/provider error values.

### Treat the result as an API key with provider-controlled lifetime

The documented exchange returns a key, not an access/refresh-token pair or expiry metadata. Store it through the
existing private `CredentialStore`, keyed by provider profile, and send it as the selected protocol's bearer key.
Do not invent a refresh flow or expiration time. The separately documented 180-day onboarding-key lifetime does
not establish the lifetime of keys created by this flow. This supersedes ADR-008's assertion that the key never
expires: Bee has no locally known expiry, which is different from a provider guarantee.

`auth status` reports local presence/known expiry only. Reauthorize a revoked, expired or otherwise invalid key;
manage credit/budget failures with OpenRouter. Logout removes Bee's copy, and the user revokes the remote key in
[OpenRouter's key settings](https://openrouter.ai/settings/keys). No documented PKCE refresh or self-revocation
endpoint is needed by this implementation. Failed login leaves any previous stored credential intact.

Keep existing manual stdin-key and environment-key paths. Authorization failure never silently chooses another
credential source or provider. Inference 401/403 is not an instruction to replay a request or automatically log in.
ACP's existing missing/locally-expired credential login remains turn-owned and cancellable.

## Consequences

- Users can authorize Bee without copying an API key or registering a client application.
- The flow follows documented public endpoints and never inspects browser sessions or passwords.
- No new Rust provider adapter or flow kind duplicates the existing data-driven mechanism.
- Bee cannot predict remote revocation or unspecified key expiry from its local store. A present key can still fail
  inference, and a successful login does not ensure credits, workspace permissions or model eligibility.
- Keys, authorization codes, verifiers and response bodies stay out of diagnostics, ACP messages and model context.
  Store permissions, atomic replacement and audience separation continue to use the existing authentication tests.
- Fake HTTP/browser and local-server tests cover state/denial handling, malformed responses, redirects, response
  bounds, persistence and failed replacement. Existing ACP cancellation/close/deadline tests protect cleanup and
  prevent late credential writes or completion notifications.
