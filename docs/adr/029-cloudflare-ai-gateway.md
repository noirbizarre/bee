# ADR-029: Cloudflare account routing and interactive settings acquisition

## Status

Accepted.

## Context

Issue #111 requires named Cloudflare credentials with Account ID and Gateway ID. Both routing identifiers belong
to the account, while the token belongs in credential storage (ADRs 027–028). Required settings currently make
an incomplete account invalid before CLI login can collect them.

Cloudflare's [REST API](https://developers.cloudflare.com/ai-gateway/usage/rest-api/), checked October 28, 2026,
supports Chat Completions, Responses and Messages using the same scoped API-token bearer authentication.
The older gateway `/compat/chat/completions` endpoint is deprecated for new single-model calls. Workers AI
[Responses compatibility](https://developers.cloudflare.com/workers-ai/configuration/open-ai-compatibility/) is
limited to two GPT-OSS models and non-streaming requests; Workers AI cannot use Messages.

## Decision

Register `cloudflare` with the existing static credential material and required private `account_id`/`gateway_id`
settings. Derive official account URLs and the gateway header from validated settings; reject native target and
authentication-header overrides. Use bearer authentication on every protocol. Require Workers AI Read permission
for inference, explicit model declarations and no discovery. Native model/protocol restrictions apply before HTTP.

Represent incomplete login configuration as a non-executable, target-only draft. Shared profile membership rules
resolve an omitted target only when exactly one account is active. CLI login asks for missing declared ordinary
settings before strict preflight, imports the environment token or asks for a manual token, then publishes settings
and stored credential authority through existing guarded transactions. Other invalid accounts remain errors.
Noninteractive stdin remains a single token line; missing routing settings must be configured first.

Snapshot original declarations and provenance separately from proposed accounts so concurrent target edits cannot
be mistaken for user-supplied additions. Preserve unrelated comments/edits and existing-account membership.

## Consequences

No generic Cloudflare authentication enum or new credential-store format is needed. Two accounts may share a
Cloudflare account while selecting different gateways and independent local tokens. API-token logout removes
only Bee's stored copy. Credentials and configuration remain separate atomic files, with explicit partial-publication
errors rather than unsafe rollback over concurrent credential changes.

We reject weakening normal account validation or inventing placeholder routing values for onboarding. Drafts add
a small persistence API but cannot reach inference. Model restrictions need updating when Cloudflare expands its
documented Workers AI protocol support. Supporting legacy gateway BYOK passthrough or native discovery would
require separately verified authentication and availability contracts.
