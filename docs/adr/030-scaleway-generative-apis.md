# ADR-030: Scaleway serverless Generative APIs

## Status

Accepted

## Context

Issue [#109](https://github.com/noirbizarre/bee/issues/109) requires native Scaleway authentication, account context and
isolated routing. Under ADR-027 and ADR-028, compiled-in providers own implementations and configured accounts own
credentials/private settings; profiles group accounts rather than storing credentials themselves.

The official [Generative APIs reference](https://www.scaleway.com/en/developers/api/generative-apis/),
[schema](https://www.scaleway.com/en/developers/api/generative-apis/v1/schema.yml) and
[usage guide](https://www.scaleway.com/en/docs/generative-apis/api-cli/using-generative-apis/), checked October 29, 2026,
document bearer authentication with the API secret key. Neither access key nor organization ID is required for inference.
The guide documents project-scoped URLs for IAM and consumption/billing isolation, and the reference identifies Paris
as the only available region. The documented global endpoint works without a regional path.

Scaleway supports function tools and streaming Chat Completions. Its Responses API does not support Bee's native
continuation contract, including `previous_response_id` and replaying arbitrary response output as subsequent input.
The vendor recommends Chat for tool workflows. Chat also rejects explicit prompt-cache key controls.

## Decision

Register `scaleway` with generic API-key credentials, default `SCW_SECRET_KEY`, and Chat Completions only.
Reuse the generic credential store, CLI import transactions, bearer headers and Chat adapter.

Optional private `settings.project_id` accepts a hyphenated UUID and derives
`https://api.scaleway.ai/{project_id}/v1`; omission uses `https://api.scaleway.ai/v1` and the remote default project.
Optional private `settings.region` accepts only `fr-par` and records the currently supported location constraint;
it does not invent a regional URL. These settings remain outside model metadata, ACP presentation and model context.
Validate every merged endpoint against the selected project's exact official Chat route and reject authentication
header overrides or foreign login flows. Custom accounts remain the explicit route for dedicated deployments.

Login can import the inherited environment key or ask interactively with echo disabled. Explicit environment bindings
remain authoritative when missing; `--with-token` explicitly selects stdin. Successful login publishes stored authority
for that account without choosing a model or changing existing account membership. Logout removes only the local key.

Default discovery off and require explicit models unless the user opts in. Generic `/models` listing uses the same
project scope and selected bearer key, with existing account-bound availability and authorization-loss behavior.
No public catalog mapping or model/context defaults are assumed.

## Consequences

Multiple local accounts can isolate project routing, keys and model visibility without provider-specific auth variants
or a new store format. They still depend on remote IAM permissions; local accounts do not create remote permissions.

The integration supports the agent's streaming/tool workflow using the established Chat implementation. Responses and
dedicated deployment management require separate concrete work. Additional regions need verified vendor routing before
being accepted. Optional project settings are configured explicitly rather than inferred from credentials or environment.

Credential-free tests enforce routing, bearer authority, CLI source provenance, project/account separation, sanitized
failures, discovery isolation and ACP presentation. Official API changes can be addressed in a small provider policy module.
