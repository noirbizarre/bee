# ADR-018: Enrich model selection with optional discovery

## Status

Accepted. Extends ADR-007, ADR-013 and ADR-014 with optional catalog hints and budgets.

## Context

Opaque model IDs let private, new and local models run without a registry. They do not help users discover IDs or
understand capabilities. A global catalog and an endpoint's authenticated model list answer different questions:
published descriptions are not account-specific visibility, and visibility is not inference-feature verification.

## Decision

An SDK-free discovery service publishes snapshots of Models.dev's dynamically keyed public catalog and separate
in-memory endpoint listings. Production refresh runs in connection-owned background tasks; inference takes only an
already-published snapshot. Cache/network failures cannot invalidate configured models or delay their prompts.
Embedded callers inject discovery explicitly.

Cache validated public data atomically under XDG cache, retaining stale fallback on refresh failure. Never persist
account-specific endpoint results there. Profile `catalog_provider` associates catalog records explicitly; shipped
presets may supply this field as data. Never infer association from profile names or use catalog endpoints/credentials.
Keep configured suggestions/defaults separate from discovered IDs. Global provider discovery does not create profiles.

Represent metadata per field with source, declared/inferred confidence, freshness and observation time. Unknown is not
false. Exact model configuration overrides profile defaults, which override catalog facts. Endpoint listing proves
visibility at observation time only. Metadata does not add inference capabilities to Bee's implemented subset.

Routing and context influence are separate per-profile opt-ins. Explicit `model_protocols` wins over a recognized
catalog npm hint to an already-configured protocol; every other case uses ADR-013's `prefer` fallback. Use model
`provider.npm`, then provider `npm`, never `family`. Map Anthropic to Messages, OpenAI-compatible to Chat, and OpenAI
to Responses (the AI SDK's default since v5); mark this inferred, not guaranteed.

Explicit exact-model/profile context capacities override opted-in catalog context limits. Published input/output limits
remain informational. Stale validated data can continue informing opted-in routing and budgets, with visible provenance.
Freeze both decisions for a whole turn. If the next turn changes protocol, make its working history portable before
inference. Commit the route and history transactionally only on successful non-refused completion.

Automatic generic endpoint listing is opt-in and never starts browser login. Supported native presets enable their own
adapter automatically, independently of public catalogue policy. Native OpenRouter user-filtered discovery and the official
ChatGPT OAuth catalogue preserve provider-specific semantics. See the [coverage matrix](../models.md#native-adapter-coverage).

Amendment for issue #64: ordinary refresh retains same-account last-good visibility on transient failure, reporting stale
data, the last successful fetch, latest attempt and sanitized error separately. Credential/account replacement, logout and
authorization rejection invalidate private visibility; token renewal alone does not. Publication generations reject late
obsolete responses. Private results are never persisted in the public cache. External credential changes are observed on
refresh. ACP options and CLI queries share the projection without implying that listed models can be invoked.

Explicit built-in login may create a missing user-owned connection and add active-profile membership. Native ordering can
initialize a missing model selection, below session/configured defaults; this selection is pinned and durably saved before
publication. Public catalogues still cannot create connections or choose defaults. Existing selections survive refresh.

## Consequences

Selection is informative without making a complete provider registry a prerequisite. Users can override inaccurate
catalog facts and explicitly select protocol/context behavior. Offline behavior remains usable and its staleness visible.
Catalog context limits remain declarations and approximate accounting remains approximate.

We reject catalog-required validation, automatic endpoint replacement, capability inference from listing membership,
and model-family routing heuristics. Background work adds cancellation/cache lifecycle obligations; immutable decisions
prevent refresh from changing an in-progress tool loop. Public catalog browsing includes providers whose native
protocols Bee does not implement, without promising it can infer against them.
