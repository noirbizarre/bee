# Providers, instances and credentials

Bee distinguishes three identities:

- A **provider type** is a compiled-in integration, such as `openai`, `github`, or `custom`. It declares defaults,
  authentication mechanisms and private fields. Adding a type does not extend a central provider enum.
- A **provider instance** is a named account/endpoint configuration under `[providers.NAME]`. Credentials, discovery
  and durable selection use this instance name. Several instances can reference the same type.
- A **user profile** groups instance references and model preferences. It does not create an authentication identity.
  Sharing an instance between profiles shares its credentials; see [profiles](profiles.md).

## Discover native types

```sh
bee providers list
bee providers show github
bee providers show google-cloud
```

These commands inspect definitions without loading project configuration or credentials. They are separate from
`bee models providers`, which browses Models.dev. Native types supply protocol, endpoint and authentication defaults;
the full list and provider-specific setup appear under [authentication](index.md#authentication).

## Configure multiple accounts

```toml
provider = "personal"

[providers.personal]
type = "openai"
credential = { env = "PERSONAL_OPENAI_KEY" }
model = "your-personal-model"

[providers.work]
type = "openai"
credential = { store = true }
model = "your-work-model"
```

Authenticate each instance independently. ACP labels retain the instance name even when both display as OpenAI.
Logout affects only the targeted instance, including every profile referencing it. Changing a type, target or account
can require explicit model reselection; Bee never substitutes another account after failure.

## Custom endpoints

```toml
[providers.gateway]
type = "custom"
protocol = "openai-chat"
base_url = "https://gateway.example/v1"
credential = { env = "GATEWAY_KEY" }
headers = { "X-Team" = "engineering" }
options = { max_tokens_field = "max_completion_tokens" }
model = "private/arbitrary-model.v2"
models = ["another-model"]
```

Supported protocols are `openai-chat`, `openai-responses` and `anthropic-messages`. Explicit custom instances require a
base URL. Models remain opaque IDs, not an allowlist. Omit `credential` for unauthenticated endpoints. Options are the
[supported typed options](inference.md), not arbitrary JSON. Multi-protocol instances use `protocols` and `prefer` as
described in [configuration](index.md#providers-and-models). Do not combine the expanded and singular route forms.

Custom instances reuse supported protocols and generic secrets/tokens. Configuration cannot implement a new protocol,
OAuth issuer, refresh grant or provider-specific browser flow.

## Login and credential sources

```sh
bee auth login github
bee auth login personal
bee auth login work --with-token
bee auth status
bee auth logout work
```

An existing instance name wins over a type name. If no instance exists but the name identifies a native type, login
proposes an instance of that same name/type and active-profile membership. Custom names must be configured first.
Declaration changes publish after successful authentication. Status and ordinary inference do not create declarations
or import environment credentials.

For an environment-backed instance, plain explicit login validates its credential fields, stores them for the instance
and changes user configuration to `credential = { store = true }`. Subsequent requests use the stored generation even
if the environment changes or disappears. `--with-token` explicitly imports stdin instead. Missing environment values
fail without trying another source. Remove higher-priority environment/membership overrides before switching authority.
Browser/device options use native acquisition instead of environment import.

OpenRouter's browser flow produces an API key. ChatGPT uses verified OAuth registration and renewable tokens; GitHub
supports device OAuth and static user tokens. API-key/OAuth methods are distinct from acquisition and sources. Cloud
ADC stays externally owned; Bee renews supported ADC without importing or modifying shared ADC. No-auth endpoints
need no credential entry.

Native definitions declare required/optional fields, accepted shapes, ownership and secrecy. Secrets belong in private
credential storage or environment bindings. `settings` holds private provider configuration, for example Google Cloud
`settings = { project = "resource-project", location = "global" }`. The existing `google_cloud` table remains supported;
do not use both forms. Account/gateway IDs are potentially sensitive even when not secret. Settings are excluded from
ordinary Debug, public model metadata and ACP selectors. New provider integrations build on these declarations.

Credential files use version 3 with typed material and integration bindings. Existing v1/v2 entries remain readable and
migrate on a locked write, preserving unrelated providers, MCP audiences, host registration and logout revisions.
Legacy tokens do not prove their acquisition history. Refresh, expiry and revocation remain provider-owned;
incompatible material requires reauthentication. API-key logout removes Bee's copy, not the remote key. Stored authority
never falls back to an environment key after logout or rejection.

Configuration and credentials are separate atomic files. If credentials publish but configuration publication fails,
login reports partial success and asks you to review configuration and repeat login. It does not blindly restore or
delete credentials another process may have changed. Success output follows configuration commit; browser prompts
remain live during authorization.

## Migrating configuration

The old `preset` key and top-level `[presets]`/`[login_flows]` sections are rejected, including in overridden layers.
Diagnostics identify the file and structural key without echoing values.

1. Replace native `preset = "openai"` with `type = "openai"`, retaining the instance name and overrides.
2. Inline custom preset endpoints/settings into instances using `type = "custom"` and explicit URLs.
3. Move shipped-preset overrides into the affected instances; configuration cannot override registry entries.
4. Remove custom login-flow definitions. Use a supported native flow or import an API token.
5. Remove explicit `auth` and `discovery` integration selectors. Select the matching native `type` instead: for example,
   a protocol-defined Copilot instance using `auth = "github"` or `discovery = "github"` becomes `type = "github"`.
   Control automatic listing with `discover_models`; custom instances cannot borrow native integration authority.
6. Retain instance names/profile memberships to preserve credential and session identity. Automatic model choices bound
   to the old authority representation can require explicit confirmation once after migration.

Bare protocol-defined connections retain their implicit-custom meaning; explicit `type = "custom"` is preferred for
new configuration. No auth-profile migration or credential renaming is required.
