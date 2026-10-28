# ADR-022: Named profiles scope connections, model preferences and durable sessions

## Status

Accepted. Extends ADR-006, ADR-007 and ADR-019. The historical Rust `provider::Profile` type remains a connection.

Configuration and default-membership rules are superseded by [ADR-028](028-account-model-availability-and-selection.md).
Current profiles independently inherit enabled accounts or replace membership with `accounts`, and have one optional
qualified model. The schema below is historical; selector precedence and durable profile ownership remain applicable.

## Context

Connection names already identify credentials, OAuth registrations and model-discovery results. Calling these objects
profiles confuses an account connection with a work/personal context containing several accounts and model preferences.
Changing credential identities would disconnect existing installations. Merely changing the default provider would leave
other accounts selectable and let resumed sessions carry model choices or context across profiles.

## Decision

Keep `[providers.<connection>]` and credential storage unchanged. Add `[profiles.<name>]` with complete `connections`
membership, an optional default `provider`, and per-connection `models` tables accepting `model` and `models`.
Both model fields independently inherit omitted connection defaults. A scoped owned connection collection carries its
immutable profile identity; consumers use it for ACP selection, auth and account-specific discovery.

Use the existing configuration merge engine. Selector precedence is implicit `default` → global `profile` →
`BEE_PROFILE` → `BEE_CONFIG` file → project `profile`. Empty environment values are unset. Explicit configuration files
can override the environment; incompatible types still fail. Unknown profiles and invalid references fail without
fallback. Validate inactive declarations too, preserving startup validation rather than postponing account errors.

With no named profile declarations, project legacy connections/defaults into `default` in memory. With explicit groups,
an undeclared `default` is empty. This guarantees its existence without granting implicit access to configured accounts.
A sole member is the default; several require an explicit default; zero members is valid.

Persist immutable ownership in the creation snapshot and scope listing to that header. Validate owner and restored
connection membership before recovery/replay, rejecting cross-profile resume even for shared connections. Legacy
journals belong to `default`. Journal version 3 requires explicit ownership; v1/v2 remain readable and can accept new
records. Older binaries must reject v3 instead of silently ignoring ownership.

Selection is runtime-fixed. Existing auth/model positional arguments continue to name connections, and public catalog
browsing remains independent of membership. No live ACP grouping switch or mode-specific configuration is introduced.

## Consequences

- Existing configuration, credential names and login registrations require no rewrite or new authentication.
- Groups can share connections deliberately; shared logout affects all references. Separate accounts need distinct
  connections. Profile model overrides are isolated owned values.
- Explicit groups require explicit membership; adding the first group does not silently expose all legacy accounts.
- Durable session ownership prevents accidental cross-context restoration; renamed profiles cannot resume old sessions
  until their original name is restored. Names are configuration identities, not an OS security boundary.
- Omitted model fields preserve connection defaults; overriding only suggestions does not replace an explicit default.
- Scope travels with the provider collection rather than an independently configurable runtime label, avoiding mismatched
  connection access and history ownership. Public embedding constructors remain compatible.
- A live profile switch would require transactional session migration and continuation handling; restart selection keeps
  that complexity outside the initial feature.
