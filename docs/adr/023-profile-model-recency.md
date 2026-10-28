# ADR-023: Persist per-profile model recency separately from configured defaults

## Status

Accepted. Extends ADR-007, ADR-018, ADR-019 and ADR-022.

Default resolution and advisory/explicit-selection exceptions are superseded by
[ADR-028](028-account-model-availability-and-selection.md). Current defaults are qualified root/profile preferences;
all selections and inference enforce account availability, with no native-order default. Successful account-bound recency,
private persistence and warning-only preference-write failure remain applicable. Conflicting rules below are historical.

## Context

Profiles already have layered configured model defaults and durable sessions retain their own selections. Neither
remembers a recent workflow when opening a new session. Rewriting configuration on selector changes would erase the
distinction between a stable preference and runtime state. Reusing a bare model ID or blindly falling back after account
loss risks sending a prompt through a different account. Optional discovery is not universally supported or authoritative
for explicit custom IDs, so demanding a live listing before every selection would regress offline/custom connections.

## Decision

Keep the current configuration schema and precedence. The effective configured default is the profile's default
connection plus its `model`, otherwise first `models` entry. Prefer an account-bound last-used connection/model when
creating a session, without allowing other sessions' later successes to change that session's choice.

Record last-used only after real inference successfully commits under the existing finalization/cancellation boundary.
`EndTurn` and `MaxTokens` count; failed, refused or cancelled work and selector/discovery-only changes do not. Store
private versioned preferences by profile under the XDG state root, with short-lived locking and atomic read/modify/write
replacement. Embedded runtimes use isolated memory unless the caller explicitly injects a persistent preference store.
Do not derive recency by scanning journals or merge runtime state into user configuration.

Bind remembered models to connection targets/headers and credential continuity through private domain-separated digests.
OAuth binds stable registration identity and scopes, not rotating tokens. API-key replacement conservatively requires
confirmation. Never emit digests or credential values through ACP or diagnostics.

Use the resolution table in [profiles](../profiles.md#configured-default-and-last-used-model): validate membership,
credentials/identity and current complete account visibility. Unknown/stale/unsupported discovery preserves the exact
choice provisionally, with execution-time identity checks. A missing remembered model permits a configured fallback only
on the same connection/account, with a warning. Removed connections, changed accounts or unavailable authorization require
explicit selection; never search other accounts. With no remembered choice, retain the existing configured/native-order
default rules. Explicit/custom and legacy pinned selections keep their existing advisory-discovery semantics.

Persist automatic selection provenance and explicit-selection-required guards in session state. Journal version 4 makes
these fields explicit so older binaries reject rather than ignore a safety boundary; v1–v3 remain readable. Model/provider
settings clear guards transactionally, including explicit confirmation of the displayed provider. Background discovery
and native initialization cannot clear a guard. Profile ownership is still validated before recovery/replay.

Preference persistence follows successful history commit as a best-effort second transaction. Failures warn before idle
without undoing the answer, stop reason, or in-memory recency. Invalid preference state fails startup rather than becoming
an empty preference that could select another account.

## Consequences

- Stable defaults retain ordinary configuration precedence; recent successful workflows take precedence for new sessions.
- Account-private endpoint catalogs remain disposable and are not copied into persistent preferences.
- Concurrent processes preserve unrelated profiles; the latest serialized preference update wins.
- A crash between history and preference commits can retain older recency. Making them atomic would couple independent
  sessions through a shared transaction log and is not warranted for a convenience preference.
- API-key rotation can require reselection even for the same real account; safe continuity is preferable to guessing.
- Opening a session does not wait for discovery. Later evidence may invalidate an automatic choice, with a visible notice.
- Session guards add a versioned journal contract but prevent restart or native discovery from bypassing confirmation.
