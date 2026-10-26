# Named profiles

A **profile** groups references to provider **connections** and its own model preferences. A connection, defined under
`[providers.<name>]`, owns endpoints, headers and a credential source. Profiles do not authenticate independently.

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
argument to `bee auth` or `bee models list` must also be a member. Positional arguments still name connections;
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
profile. See [session history](sessions.md).

See [ADR-022](adr/022-named-profiles.md) for the configuration and durability decisions.
