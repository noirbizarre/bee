# ADR-024: Integrate GitHub Copilot device OAuth, direct tokens and native models

## Status

Accepted. Extends [ADR-008](008-provider-authentication.md), [ADR-009](009-provider-presets-and-login-flows.md),
[ADR-013](013-per-model-protocol-selection.md) and [ADR-018](018-optional-model-discovery.md) with a native
GitHub.com Copilot integration.

## Context

Copilot can expose Chat Completions, Responses and Messages under one account API target. A generic preset cannot
describe device polling, bearer authentication for Messages, per-request initiation classification or exact native
model endpoint declarations. Bee must keep its own agent loop and client-managed tool execution.

Sources inspected on 2026-10-26:

- [GitHub device flow][device] specifies code issuance, minimum polling interval, pending/slow-down responses and expiry.
- [GitHub SDK authentication][sdk-auth] describes GitHub user-token authentication. It does not establish a complete
  direct inference wire contract or guarantee that every GitHub PAT works with that contract.
- OpenCode's [provider integration][upstream-auth] and [native model adapter][upstream-models], at revision
  `d8607b6616a50d6b89dba2ed46fec99c9457cc97`, provide the current direct-bearer compatibility reference.

[device]: https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow
[sdk-auth]: https://docs.github.com/en/copilot/how-tos/copilot-sdk/auth/authenticate
[upstream-auth]: https://github.com/anomalyco/opencode/blob/d8607b6616a50d6b89dba2ed46fec99c9457cc97/packages/core/src/plugin/provider/github-copilot.ts
[upstream-models]: https://github.com/anomalyco/opencode/blob/d8607b6616a50d6b89dba2ed46fec99c9457cc97/packages/core/src/github-copilot/models.ts

## Decision

### Separate authentication from credential source and optional discovery

Ship `github` with `auth = "github"`, store-backed device login and native discovery. Authentication
mode remains active when the whole credential table is overridden with an explicit environment-variable reference,
or model discovery is disabled. No automatic environment, store, account or provider fallback is performed.

Use the existing public OAuth client ID `Ov23li8tweQw6odWQebz`, also used by OpenCode. The consent application's
identity is external, not a Bee registration; using it is a compatibility choice, not a claim of GitHub endorsement
or a general authorization grant for third-party clients. No client secret is embedded.

Request `read:user` at `https://github.com/login/device/code`; display only its validated GitHub device page and
user code. Poll `https://github.com/login/oauth/access_token` using the device grant. Wait before the first poll,
honor the interval, increase it by at least five seconds on slow-down, and bound the operation by device expiry.
Network requests remain HTTPS-only, bounded, redirect-free and cancellable. The library never prints.

Persist the upstream GitHub user token privately under the selected connection. The chosen public flow returns no
locally known expiration; Bee does not invent refresh semantics. A response requiring token renewal fails explicitly
instead of dropping refresh metadata. An environment token is never persisted. Missing or rejected environment
credentials cannot start store-backed browser login. Local status is not an entitlement check.
The existing private credential backend supports Unix; Windows uses explicit environment-token authentication.

Use provider revisions independent of ChatGPT host registration. Revision tombstones survive empty logout so a
pending initial login cannot resurrect authority. Compare prior credential and revision under the store lock, then
hold the ACP current-turn gate through synchronous persistence. Completion publication is separately gated.
Cancellation, close and disconnect abandon polling. Logout deletes local authority; remote revocation is user-managed
through GitHub application settings.

### Use the direct bearer contract and validated account targets

Send the selected GitHub token directly; do not implement the older Copilot token-exchange flow. Prepare the target
with `GET https://api.github.com/copilot_internal/user` using API version `2025-04-01`, independently of model listing.
Explicit authorization failure or `chat_enabled:false` stops the operation. An unavailable or malformed lookup may
use only the safe default `https://api.githubcopilot.com`; it does not establish entitlement. A supplied unsafe target
is rejected, not ignored.

Accept only HTTPS root targets with default port at these exact hosts:

- `api.githubcopilot.com`
- `api.individual.githubcopilot.com`
- `api.business.githubcopilot.com`
- `api.enterprise.githubcopilot.com`

Userinfo, query, fragment, non-root paths and arbitrary ports/hosts are rejected before bearer transmission. Custom
Enterprise authentication domains are outside this integration. A private per-turn session pins target, credential
and opaque interaction ID; store revision/credential checks before each model step prevent logout or replacement
from switching a running turn's authority. Embedded model callers require the same prepared session.

All three inference protocols use bearer authorization, honest Bee User-Agent, API version `2026-08-01`,
`Openai-Intent: conversation-edits` and `X-Interaction-Type: conversation-agent`. The opaque interaction ID stays stable
for an ACP session. `X-Initiator` classifies the semantic request as user-led or agent/tool continuation before protocol
serialization. Messages sends no `x-api-key`. Required integration headers are reserved against custom overrides.

Preserve existing wire semantics: stateless Responses with encrypted reasoning replay, native Messages blocks and
Chat function calls. ChatGPT namespace and output-cap exceptions are not Copilot behavior. There are no automatic
inference retries, protocol retries or legacy-auth fallbacks. Prompt-cache controls require their existing verified
route contracts; Copilot defaults add none.

### Freeze exact account-native model facts below explicit configuration

List `<validated target>/models` with the same selected token. Preserve server order, labels and supported capability
and limit metadata. Exclude explicitly disabled, non-picker, non-tool or non-streaming entries from automatic choices;
do not change account model policies. Only exact `/chat/completions`, `/responses` and `/v1/messages` declarations map
to implemented protocols. Present but unsupported-only declarations are not guessed routes.

Routing precedence becomes explicit model mapping → native endpoint declaration → opted-in public hint → preference.
Multiple native routes use configured preference order. Known native restrictions reject conflicting configuration
before inference. Context precedence is explicit model/default capacity → native total context → opted-in public
capacity. Prompt-only and output limits remain separate metadata, not invented total capacities or request settings.

Resolve the final snapshot after account observation, then freeze route/budget for the turn. Delay native-history
conversion until this account-bound route is known. Private model/target facts never enter the public cache.
Existing single-flight, bounds, epochs, stale same-account fallback and post-fetch identity checks continue to apply.
Replacing the upstream token conservatively changes identity, even if both tokens belong to the same GitHub user.

## Consequences and verification

Users can choose device OAuth or one explicit environment variable without adding a second agent runtime. Native
declarations reach models requiring different APIs without hard-coded model families. Native protocol/capacity facts
are independently useful when public Models.dev hints are disabled.

This integration depends on the cited direct-wire behavior and public OAuth application availability. Subscription,
seat, policy, token eligibility and quota remain server-controlled; listing a model does not verify inference access.
Live account checks must be reported separately from deterministic fixtures.

Tests cover polling/slow-down/expiry/denial, secret-safe failures, trusted target validation, source isolation,
revision races without ChatGPT initialization, guarded commits, model metadata/routing precedence, account visibility
invalidation, and production ACP streaming/tool cycles on all three protocols. ACP tests also cancel/close device
polling and verify no later credential write or completion notification. Existing CI covers stdout purity, store
permissions, durable history and continuation behavior.
