# ADR-027: Provider registry and instance-owned authentication

## Status

Accepted. Implements #108. Supersedes the preset/configurable-flow choices of
[ADR-009](009-provider-presets-and-login-flows.md) and corresponding terminology/source restrictions in
[ADR-007](007-provider-profiles-and-model-selection.md) and [ADR-008](008-provider-authentication.md).
Native security contracts in ADRs 020–026 remain applicable.

## Context

Presets were a second user-facing provider concept. Connections already owned credentials and profiles grouped them,
but native behavior depended on login/discovery identities and separate model session slots. Extending integrations
required unrelated central wiring.

## Decision

Use a compiled-in, string-keyed registry and named instances referencing `type`. Definitions own protocol defaults,
authentication methods and private field schemas. Keep the implemented protocol enum; replace the native discovery
identity enum with capabilities. Custom endpoints are instances of the same model.

Keep grouping profiles separate. Credentials/discovery use instance names, not provider types or user profiles.
Preserve session identities and native endpoints/defaults, including independent OpenCode offerings.

API-key/OAuth methods are generic. Acquisition, sources, native registration and account settings remain distinct.
Fields declare accepted shapes, required/optional status, ownership and secrecy. Typed credentials use private storage:
read v1/v2, write v3, preserve host registration/revisions and MCP audience separation. External ADC stays external.

Prepared runtimes own request-boundary renewal, authority checks, trusted routes and native headers. The model loop
holds one runtime interface. Protocol adapters retain native continuation; ACP owns interactions, cancellation and
guarded commits. Discovery enablement cannot change inference identity.

Login resolves instances before native type names. Missing native names create matching instances. Explicit environment
login persists complete credential generations and switches the instance to stored authority, validated against all
effective layers. Never fall back between accounts or credential sources after authentication failure.

Reject preset and configurable-flow syntax with migration guidance, checking each layer before merging. Authentication
contracts remain compiled-in, preventing configuration from redirecting credential exchanges.

## Consequences

- Ordinary integrations need a registry definition, not a provider enum variant or user-maintained preset catalogue.
- Custom endpoints use concise protocol/URL configuration and supported generic credentials.
- Native refresh, revocation, target validation and cancellation remain covered by existing regression tests.
- Type/target/account fields participate in continuity; old automatic references can require explicit reselection.
- Credential/config files are individually atomic, not crash-atomic together. Publish credentials before configuration;
  report partial publication with recovery guidance, without unsafe concurrent rollback.
- `bee providers list/show` inspect types; `bee models providers` remains public Models.dev browsing.
- New Cloudflare, Scaleway and OVH integrations remain follow-up work using these extension points.

See the [provider guide](../providers.md) for configuration, lifecycle and migration.
