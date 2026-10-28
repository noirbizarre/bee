# ADR-028: Account-scoped model availability and qualified selection

## Status

Accepted. Supersedes the advisory model membership and native-order defaults in
[ADR-018](018-optional-model-discovery.md), the configuration/default-membership choices in
[ADR-022](022-named-profiles.md), the default resolution and explicit-selection exceptions in
[ADR-023](023-profile-model-recency.md), and the instance schema/terminology and configuration compatibility guidance in
[ADR-027](027-provider-registry-and-instance-authentication.md). Earlier provider-selection configuration in
[ADR-007](007-provider-profiles-and-model-selection.md) is historical. Public metadata/cache isolation, durable profile
ownership, successful account-bound recency and native authentication/security contracts remain applicable.

## Context

Compiled implementations, configured credential identities and workflow profiles have different responsibilities.
Calling both implementations and configured identities providers obscures which object owns credentials and availability.
Separate account/model pickers and provider-local defaults make cross-account selection ambiguous. A union of public
catalog entries, defaults and endpoint listings can advertise models outside the configured account's intended scope.
Selecting the first model from a list turns incidental ordering into authority to incur inference through an account.

The initial MVP needs one supported configuration schema, explicit availability and a single cross-account model choice,
while retaining vendor-specific routing, credentials and continuation semantics.

## Decision

### Provider, account and profile boundaries

A provider is a compiled-in implementation. Definitions, protocols, trusted acquisition and prepared runtimes stay in
`src/provider.rs` and `src/provider/*`; native runtime preparation/renewal remains in `src/provider/runtime.rs`.
`src/account.rs` owns configured `Account`/`Accounts`, parsing and one SDK-free `ModelRef`; account-specific cache
configuration lives in `src/account/cache.rs`. `src/profile.rs` resolves membership and one optional model preference.

Use `[accounts.ID]` with `provider = "NAME"`, not `type`. Accounts own credentials, private fields, routes and raw model
declarations. Supported implementations are `openai`, `anthropic`, `openrouter`, `chatgpt`, `github`, `ollama`, `opencode`,
`opencode-go`, `deepseek`, `google-aistudio`, `google-cloud`, `groq`, `mistral`, `xai` and `custom`. Retain vendor endpoints,
protocol preferences and credential bindings. Groq is distinct from Grok models under xAI. There is no registered `google`
alias; public Models.dev `google` and raw Google model paths remain unchanged.

`enabled` defaults to true. Enabled accounts form the baseline. Every profile independently inherits that baseline when
`accounts` is omitted; an explicit list replaces it, can include disabled accounts, and `[]` means none.
`[profiles.default]` affects only itself. Validate every declared profile, including inactive ones.

Root/profile optional `model` contains `<account-id>/<raw-model-id>`, split only on the first slash. Reject slashes and
outer whitespace in account IDs; preserve case and further slashes in the raw suffix. Account-local lists, routing,
context/metadata keys and HTTP requests retain raw IDs. A profile's own model must refer to an active account. Without
one, inherit root `model` only if its account is active; an excluded root preference is dormant. References still require
configured accounts and valid raw IDs. Arrays replace across layers. There is no account-level singular `model`, scalar
root/profile default `provider`, profile `connections` or nested model-preference map.

### Account availability

`discovery` is boolean and separate from native capabilities; `catalog` associates public enrichment independently.
With discovery off, availability is exactly declared raw `models`, with omitted/empty permitting none. With discovery
on, use a successful selectable account listing intersected with optional `models`: omitted means no filter, `[]` none.
Selection and inference share this check. Public metadata, defaults, selected IDs, route/context/metadata-map keys and
explicit endpoint observations cannot expand the set. Hidden native route facts do not grant picker membership.

Pending/first failed discovery authorizes no models; successful empty is authoritative empty. Same-account last-good
results can survive transient failure under the stale policy. Authorization loss, account/target replacement and logout
clear private visibility; generations reject late obsolete publication. OAuth identity-preserving renewal retains identity.
Private results never enter the public cache. Configuration parsing performs no network work. Reject statically impossible
defaults early, and await discovery-dependent availability before use. Google Cloud accepts false discovery and rejects
unsupported true discovery; native auth, headers and target checks apply even with listing off.

### Qualified selection and execution

Use one picker across all active accounts, with qualified machine IDs. Labels combine account display name (provider
label, then account ID fallback) and model display name (raw ID fallback). Retain qualified IDs for collisions. Expose
per-account discovery status independently. When unselected/pending/unavailable, present an empty “Choose a model” value
only for the pinned SDK's string setting; never execute, accept or persist it, or visually select the first real choice.

Precedence is current/durable explicit selection → usable pinned account-bound recent success → applicable configured
default → unselected. No sole account/model, first declaration, native ordering or public catalog creates a default.
Authoritative exclusion of recency permits only an available configured fallback on the same account with matching
authority; never silently cross accounts. Pending evidence does not permanently invalidate recency. Only committed
successful inference records recency; preference-write failures warn without undoing answers.

Account/model selection is atomic under session busy/finalization gates with durable save before response and rollback.
Missing evidence is awaited cancellably outside the session mutex, then authority, gate ownership and availability are
rechecked. Every production prompt authenticates/renews its selected account, checks continuity/membership/availability,
resolves required native facts and freezes the turn. Explicit, default, recent and resumed choices cannot bypass this gate.
Changing accounts drops native continuation even if raw model IDs match. No candidate means choose a model without
authenticating an arbitrary account.

### CLI and authentication

Keep `bee providers list/show` for implementations and `bee models list [ACCOUNT]` for available account models.
Omitted list aggregates active accounts with bounded concurrent discovery, qualified IDs, labels and per-account status.
Print successful results despite other failures. Ordinary list warns/succeeds with usable stale results; targeted discovery
with no usable result is nonzero. Explicit refresh failure stays nonzero even with stale data. Public browsing uses
`bee models providers` and `bee models list --catalog ID` independently of membership; `--endpoint` cannot bypass availability.

Omitted login/logout requires exactly one active account, never root model or recency. Explicit authentication can target
configured disabled accounts without enabling them or changing membership. Native-name login can create a same-name
account with `provider = "NAME"`, enabled by default, and append only a newly created account to the restricted active
profile subject to effective-layer/origin checks. Write no default model. Preserve environment-to-store publication,
complete credential generations, stale-login guards, comments, no-op preflight and concurrent-edit safety.

Support one configuration schema with concise value-free diagnostics for unsupported fields. Credential-format readers
remain independent: read v1/v2, write v3, preserving private authority, native registrations and MCP audience separation.

## Consequences

- Public enrichment can fail without changing declared offline availability; discovery-enabled accounts must establish
  account visibility before execution, so asynchronous selection/prompt checks are necessary.
- Private/local IDs remain usable through explicit declarations without requiring a public registry entry.
- Defaults are explicit preferences, never account authorization. A first-use login can leave the picker unselected.
- Atomic qualified selection removes intermediate account/model mismatch and preserves raw vendor-prefixed wire IDs.
- Shared projections must guard authority epochs, stale results and concurrent selection/refresh/cancellation.
- Separate account and profile identities retain credential sharing and durable ownership without cloning account defaults.
- Configuration and credential publication remain separate atomic transactions; report partial success without unsafe rollback.

See [accounts](../providers.md), [profiles](../profiles.md) and [model availability](../models.md) for the current
user contract.
