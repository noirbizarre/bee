# ADR-026: OpenCode offering-scoped API keys and discovery

## Status

Accepted.

## Context

Issue #55 asks for dedicated authentication for both OpenCode Zen and Go without depending on private CLI credentials.
The official [Zen](https://opencode.ai/docs/zen/) and [Go](https://opencode.ai/docs/go/) documentation, checked on
October 27, 2026, describes console-issued API keys for external coding agents. Neither offering documents external
OAuth/device authorization, client registration, scopes/audience, token refresh or an API for revocation. Website
sign-in is not a public authorization flow for Bee. Zen is pay-as-you-go; Go requires a single-member subscription.

## Decision

Keep the existing `opencode` preset and its environment credential compatible. Add `opencode-go` with its own
environment variable, endpoints and public catalogue association. Explicit CLI onboarding creates independent
store-backed connections through `--with-token`; existing configured credential sources remain authoritative.
No OAuth, PKCE or device-flow workaround is implemented. Key lifetime and revocation remain console-managed.

Use two typed integration identities and shared code for the documented offering-level model lists:
`https://opencode.ai/inference/v1/models` and `https://opencode.ai/inference/go/v1/models`. These public ID lists are
neither account entitlement nor ranked defaults. They supply no protocol/context facts and include restricted or
non-text models, so they cannot initialize automatic selection. Explicit configuration remains authoritative.
Console translates text models across Bee's three inference protocols; model-family guesses are unnecessary.

Identify Bee with its own user agent and supply a stable conversation header through every inference adapter.
Production ACP uses the durable session identity; embedded models generate an opaque identity unless supplied one.
Keep account listings separate from public Models.dev cache. Reject optional native discovery for overridden targets
rather than silently forwarding proxy credentials and custom headers to an official service.

## Consequences

Existing credential storage, cancellation, secret redaction and protocol adapters are reused. No new credential schema
or journal format is needed. Local logout removes only Bee's copy; users rotate/revoke API keys through the console.
Users explicitly select supported text models and check workspace access, billing and privacy policy in the console.
Zen free-tier chat access is restricted to OpenCode itself. Go has rolling allowances and user-controlled console-side
Extra Usage; Bee never falls back to another offering, account or credential after a failure.

A documented external authorization flow could be added later without treating the application's internal login state
as an integration contract. Native discovery currently supports official targets only; proxy users configure IDs.
