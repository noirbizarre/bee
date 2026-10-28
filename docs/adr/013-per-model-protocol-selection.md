# ADR-013: Select inference protocols by exact model ID

## Status

Accepted. Extends ADR-010 and replaces the preferred-only routing rule in ADR-012.
Extended by [ADR-018](018-optional-model-discovery.md) for explicitly enabled catalog hints.

## Context

The configuration examples and default/advisory selection rules in this record are historical; see
[ADR-028](028-account-model-availability-and-selection.md) for accounts, qualified defaults and enforced availability.
Exact raw-ID protocol routing remains applicable and does not grant availability.

A gateway can expose Chat Completions, Responses and Anthropic Messages while requiring different protocols for
different models. A provider-wide preference cannot describe this. Model catalogs can be unavailable or omit private
and newly released models, so inference must not depend on catalog metadata. Provider identity, opaque model IDs,
protocol implementations, endpoint settings and authentication remain separate concerns.

## Decision

Provider profiles and presets accept an optional `model_protocols` TOML table:

```toml
[providers.gateway]
prefer = ["openai-chat", "openai-responses"]
model_protocols = { "vendor/model.v2" = "openai-responses" }
protocols = { openai-chat = {}, openai-responses = {} }
```

Keys are literal model IDs, validated using the existing opaque-ID rules. Matching is exact and case-sensitive;
there are no prefixes, vendor heuristics or catalog lookups. Values name supported protocols configured on the final
profile after preset overlay. Unknown or unconfigured targets fail startup with actionable typed diagnostics.
Mapping IDs and values do not appear in those diagnostics or Debug output, including configuration merge conflicts.

Resolution is a pure operation on a validated profile and the effective model ID: use the explicit mapping, else the
first endpoint in preference order. The model is the session selection, else `model`, else the first of `models`.
Mapping entries do not become model suggestions or an allowlist. Multiple protocols still require `prefer` so unknown
models have an explicit fallback. Failures never retry another protocol.

Existing recursive table merging applies. Global, `BEE_CONFIG` and project layers override entries by exact model ID
while retaining unrelated entries. User preset overrides merge the same way; profile entries overlay preset entries.
An empty table retains inherited mappings. There is no deletion syntax. Configuration-layer type changes remain
conflicts, while preset/profile overlays follow the existing replacement and final-validation rules.

`Profile::endpoint_for_model` returns the resolved endpoint without HTTP execution. Inference uses that endpoint for
dispatch, URL, protocol options and credential presentation. Credential sources, shared headers, deadlines and common
output limits stay on the provider profile. Sessions store no redundant protocol selection: changing model/provider
already invalidates native continuation and preserves portable history.

## Consequences

One provider can serve mixed-protocol models with shared credentials, even offline from all catalog services.
Unknown models retain preference-based routing, and existing configurations keep their behavior. Exact mappings need
maintenance when model IDs change; incorrect server support still fails at request time.

We reject a global model registry, catalog-required routing and model-family heuristics because opaque/private IDs
must work independently. We retain key-by-key merging rather than adding reset or deletion semantics to configuration.
Future optional catalog hints require a separate decision and cannot displace explicit configuration.
