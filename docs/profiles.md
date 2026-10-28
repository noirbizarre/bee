# Named profiles

A **profile** scopes configured [accounts](providers.md) and durable sessions. Accounts own credentials, endpoints
and model availability; profiles reference them and may choose one qualified default model.

## Configure and select

```toml
profile = "work"
model = "work-api/vendor/model-a"

[accounts.work-api]
provider = "openai"
credential = { env = "WORK_OPENAI_API_KEY" }
discovery = false
models = ["vendor/model-a", "vendor/model-b"]

[accounts.personal-chatgpt]
provider = "chatgpt"
enabled = false

[profiles.work]
accounts = ["work-api"]
model = "work-api/vendor/model-b"

[profiles.personal]
accounts = ["personal-chatgpt"]
```

Select a profile in configuration or in the environment launching Bee:

```sh
BEE_PROFILE=personal bee
BEE_PROFILE=personal bee auth login personal-chatgpt
BEE_PROFILE=work bee models list
```

Selector precedence, lowest first, is implicit `default` → global `profile` → `BEE_PROFILE` → `profile` in
`BEE_CONFIG` → project `profile`. Empty environment values count as unset. An unknown profile fails without fallback.
Profile selection is fixed at launch; restart Bee to switch contexts. All declared accounts and profiles are validated,
including inactive ones. Tables merge recursively, scalars replace same-typed values, and arrays replace whole.

## Account membership

Each account has `enabled = true` by default. The **baseline** contains every account whose `enabled` is omitted or true.
Every profile independently inherits this baseline when its `accounts` field is omitted, including the implicit `default`.

- An explicit `accounts = [...]` replaces the baseline for that profile. It may include accounts with `enabled = false`.
- `accounts = []` exposes no accounts. References must exist and be unique.
- `[profiles.default]` changes only `default`; it never changes the baseline inherited by other profiles.
- Sharing an account shares credentials and availability. Use distinct account IDs and credential sources to isolate identities.

For example, with the accounts above, an additional `[profiles.review]` with no `accounts` field exposes only `work-api`,
even if `[profiles.default] accounts = []` is also configured. `personal` explicitly includes the disabled ChatGPT account.

Only active accounts appear in the unified ACP model picker and account model listings. `bee models list` aggregates all
active accounts; an explicit account argument restricts that scope. Public Models.dev browsing is separate.
`bee auth status` aggregates active accounts. Explicit authentication targets may name any configured account, including
a disabled one. Login/logout with no target require exactly one active account; neither defaults nor recency choose a
credential-mutation target. Authenticating an existing account does not enable it or change profile membership.
First-use native-name login can create an enabled account and append it to an explicitly restricted active profile,
subject to effective-layer/origin checks; see [authentication](providers.md#login-and-credential-sources).

## Configured default and last-used model

Optional root and profile `model` fields contain `<account-id>/<raw-model-id>`. Split only on the first slash:
`work-api/vendor/model-a` means account `work-api` and wire ID `vendor/model-a`. Account IDs cannot contain slashes or
outer whitespace; raw suffixes retain case and additional slashes. Account-local `models`, routing and metadata keys
always use raw IDs. There is no account-level singular default model or separate default-account setting.

A profile's own `model` must name an active account. An explicit out-of-scope profile default is an error. Without its
own default, the profile inherits the root `model` only if that account is active; otherwise the root preference is
dormant. All references must still identify configured accounts and valid raw IDs. Statically impossible defaults fail
at startup; discovery-dependent defaults are checked asynchronously before use.

Selection precedence is current/durable explicit choice → usable pinned account-bound recent choice → applicable
configured default → unselected. A sole account, a sole model, the first declared model, native server ordering and
public catalog entries never create a default. An unselected session offers “Choose a model” and cannot run inference
until a real qualified choice is made.

Last-used state records the exact account/raw model and credential/target continuity of a committed successful inference
turn. Normal and output-token-limit completions count; failures, refusals, cancellations, selector changes and discovery
alone do not. Recency takes precedence over project defaults but never authorizes crossing accounts after a failure.

| Situation | Selection behavior |
| --- | --- |
| Usable remembered choice | Reuse and pin that account/model. |
| No remembered choice | Use an applicable configured default, otherwise wait for selection. |
| Discovery pending or first attempt failed | Preserve the candidate provisionally, but authorize no inference until availability is established. |
| Same-account transient discovery failure with last-good results | Use those stale results under the existing stale policy. |
| Remembered model authoritatively excluded | Permit only an available configured fallback on the same account with matching authority; otherwise require selection. |
| Account removed, credentials rejected, or authority/target changed | Require explicit selection; never search another account. |

Every selection and production prompt enforces [model availability](models.md#account-model-availability), including
configured defaults and resumed choices. Explicit selection cannot bypass the check. OAuth renewal preserves account
identity; account, API-key, target or authentication-header replacement can require confirmation. Authorization loss
clears private visibility. Pending evidence does not permanently invalidate recency. Active turns keep their frozen input.

Recent choices live privately in `$XDG_STATE_HOME/bee/model-preferences.json`, falling back to
`~/.local/state/bee/model-preferences.json`. A separate lock and atomic replacement preserve other profiles' entries.
The file contains account/model IDs and private continuity digests, never plaintext credentials. Saving recency follows
successful history commit as a second transaction; failure warns without undoing the answer or in-memory choice.
Preference-lock acquisition is limited to one second. Invalid/unreadable state fails startup with a path-only diagnostic.
Embedded runtimes use isolated memory preferences unless persistence is explicitly configured.

## Durable session ownership

Sessions belong to their original profile. Listing is scoped, and cross-profile resume fails before recovery or replay,
even when profiles share accounts. A removed explicitly selected account causes resume to fail rather than switching
identities. Availability and authority are rechecked before inference. A later success in another session does not change
a session's pinned choice. See [session history](sessions.md).

[ADR-028](adr/028-account-model-availability-and-selection.md) defines the current configuration and selection contract.
[ADR-022](adr/022-named-profiles.md) and [ADR-023](adr/023-profile-model-recency.md) retain the original rationale for
profile ownership and successful account-bound recency.
