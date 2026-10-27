# Named profiles

A **profile** groups references to provider **connections** and its own model preferences. A connection, defined under
`[providers.<name>]`, owns endpoints, headers and a credential source. Profiles do not authenticate independently.

Connections may inherit a human-readable `display_name` from their preset or override it directly. ACP displays
`Display name (connection-id)` when the name differs from the ID, so two accounts using the same preset stay distinct.
Names are presentation only: profile membership, CLI arguments, account continuity and durable sessions still reference
the configured connection ID. Relabeling a connection does not select another account.

## Configure and select

```toml
profile = "work"

[providers.work-api]
preset = "openai"
credential = { env = "WORK_OPENAI_API_KEY" }
model = "connection-default"

[providers.personal-chatgpt]
preset = "chatgpt"

[profiles.work]
connections = ["work-api"]

[profiles.work.models.work-api]
model = "work-model"
models = ["work-alternative"]

[profiles.personal]
connections = ["personal-chatgpt"]
```

Choose a profile with the top-level `profile` setting or the environment that launches Bee:

```sh
BEE_PROFILE=personal bee
BEE_PROFILE=personal bee auth login personal-chatgpt
BEE_PROFILE=work bee models list
```

The selector resolves in this order, lowest precedence first:

1. Implicit `default`.
2. Global configuration `profile` setting.
3. `BEE_PROFILE` (empty counts as unset).
4. `profile` in the file named by `BEE_CONFIG`.
5. `profile` in the launch project's `.config/bee.toml`.

Explicit override files and project settings can therefore override the environment selector. Definitions use the
ordinary configuration merge rules: tables merge recursively, scalars replace same-typed values and arrays replace
whole. Changing a setting's type across layers is an error, including at the environment layer.

An unknown selected profile fails at startup with `bee::profile::setting`; Bee never chooses another profile or account.
Correct the selector in the reported source or define that profile. All declared profiles and connections are validated,
including inactive ones.

## Connection membership and model preferences

`connections` is the complete membership list, defaulting to empty. A single member is automatically the default.
For multiple members, set `provider = "connection-name"` inside the profile. An empty profile is valid but cannot run
inference until configured. Connection references must exist, be unique and contain the default and every model-preference
target. Unknown fields or invalid types are errors.

Only member connections appear in ACP selectors, auth status and account-specific model discovery. An explicit connection
argument to `bee auth status`, `bee auth logout` or `bee models list` must also be a member. Explicit `bee auth login`
is an onboarding exception: it can add an existing connection to the active profile and create a missing built-in
`openrouter`, `chatgpt` or `github` connection in user configuration. If a higher-priority configuration file overrides
the profile's membership array, add the connection to that array manually before login.
Positional arguments still name connections;
login/logout and model listing use the active profile's default connection when omitted. Auth status without an argument
lists all connections in the active profile. Public Models.dev browsing remains independent of membership.

`[profiles.<profile>.models.<connection>]` accepts `model` and `models`. Each field independently overrides the corresponding
connection setting; omitted fields inherit it. `models = []` clears suggestions, while a profile supplying only `models`
still inherits a connection's explicit `model`. The effective default is `model`, otherwise the first `models` entry.
These IDs are suggestions, not an allowlist: clients can still select any well-formed custom model ID.

Profiles can explicitly share a connection; this shares its credentials and base defaults. A profile's model overrides
do not change the connection definition or another profile. To separate accounts, define distinct connection names and
credential sources, even when they use the same preset. Credentials remain stored by connection name, so logout of a
shared connection affects every profile referencing it.

Profile selection is fixed at launch. Restart Bee with another selector to switch contexts. ACP provider/model choices
stay within the active profile; plan/build modes and mode-specific defaults are not part of this configuration.

## Configured default and last-used model

Each profile keeps two independent preferences:

- Its **configured default** is the default connection's effective `model`, otherwise its first `models` entry.
  Global configuration, the `BEE_CONFIG` file and project configuration merge in that order. For example,
  `[profiles.work.models.work-api] model = "project-model"` in the project file overrides the same global field.
- Its **last-used model** records the exact connection/model/account of a committed successful inference turn.
  A normal completion or output-token-limit completion counts; errors, refusals and cancellations do not.
  Opening sessions, refreshing discovery and changing selectors alone do not change this preference.

New sessions resolve as follows:

| Situation | Selection |
| --- | --- |
| Usable last-used choice in this profile/account | Reuse and pin that connection/model. |
| No last-used choice | Use the configured default. |
| No remembered or configured model | Use native server ordering on the selected connection, when available. |
| Remembered model missing, usable default on the same connection/account | Warn and use that configured default. |
| Removed connection, missing/rejected credentials, changed account/target, or no safe same-account fallback | Warn and require explicit provider/model selection. |
| Discovery unknown, stale, unsupported or temporarily unavailable | Keep the exact remembered choice provisionally; recheck account/authentication before execution. |

Recency takes precedence even over project-configured defaults; configuration precedence resolves the default used
when recency does not apply. Bee never searches other connections for an authenticated model. A configured default on
another account is not permission to switch to it after a remembered choice becomes unavailable.
Confirm a provider or model through ACP to recover from a selection warning. Confirming the displayed provider also
counts as explicit selection and restores that connection's default. A blocked session rejects prompts until confirmation,
including after close/restart/resume. Fresh discovery can update an idle, automatically remembered choice, with a warning
for fallback or blocked selection. An active turn always keeps its frozen input.

Only current complete listings bound to the resolved account constrain automatic reuse. Listing absence does not prohibit
an explicit custom ID; the public Models.dev catalog never establishes account access. Unknown availability does not force
network discovery before a session opens. Actual inference can still fail even for a listed model.

The production binary stores recent choices privately in `$XDG_STATE_HOME/bee/model-preferences.json`, falling back to
`~/.local/state/bee/model-preferences.json`, beside session storage. A separate lock and atomic replacement preserve
other profiles' entries across concurrent processes; the latest serialized update wins. The file contains model/connection
IDs and private account/target continuity digests, never plaintext credentials. OAuth token renewal retains identity;
replacing an account, API key, endpoint or authentication header requires explicit confirmation. API-key rotation is
conservatively treated as a potential account change. This state is not an environment preference or a configuration write.
Reordered OAuth scope sets preserve continuity. Temporary renewal failures remain retryable when the same renewable
registration is retained; confirmed authorization loss and inference authentication rejection require selection again.

Saving recency is a second transaction after successful history commit. If it fails, Bee retains the answer and current
process's recent choice, warns before idle, and leaves the prior valid file intact when publication did not complete.
Preference-lock acquisition is limited to one second, so contention cannot indefinitely delay idle or close.
A crash between the two transactions can leave the older last-used value. Invalid/unreadable preference state fails
startup with a path-only diagnostic; repair or remove that preference file after stopping Bee, then restart.
Embedded runtimes default to isolated memory preferences; use `selection::Store::open` with `Runtime::with_preferences`
to opt into persistence independently of `Runtime::with_history`.

## Default profile and migration

Every configuration has a `default` profile, including a fresh installation with no connections.

- Without named entries under `[profiles]`, Bee constructs `default` in memory from the existing `[providers]`,
  top-level `provider`, and connection model preferences. An empty `[profiles]` table also takes this compatibility path.
  Existing single-connection inference works; several connections still require the legacy top-level `provider`.
- Once named profiles are declared, define `[profiles.default]` explicitly if it should expose any connections. Otherwise
  the guaranteed default is empty, so an omitted selector does not accidentally expose accounts. Explicit groups use their
  own `provider`; the old top-level `provider` does not select within them.

Bee does not rewrite configuration or credentials. Existing login registrations and stored credentials remain usable
through their original connection names. To migrate, add groups referencing those names and select a group.

An empty table in a higher layer does not delete inherited settings. Replacing `connections` with `[]` removes membership,
but an inherited default or model preference then referencing a nonmember must also be corrected.

Durable sessions belong to their original profile. Session listing is scoped, and cross-profile resume fails before
recovery or replay, including when profiles share a connection. Old journals belong to `default`; resume those with that
profile and appropriate membership. A removed explicitly selected connection causes resume to fail instead of choosing a
new account. Unselected connection/model defaults continue to resolve from the current configuration within the owning
profile. New sessions inheriting recency retain their own account-bound selection; another session's later success
does not change them. Explicit and legacy pinned session choices retain their sticky/custom-ID behavior. See
[session history](sessions.md) and [ADR-023](adr/023-profile-model-recency.md).

See [ADR-022](adr/022-named-profiles.md) for the configuration and durability decisions.
