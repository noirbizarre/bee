# Providers, accounts and credentials

Bee distinguishes three identities:

- A **provider** is a compiled-in implementation, such as `openai`, `github` or `custom`. It declares protocol and
  endpoint defaults, native capabilities, trusted authentication mechanisms and private fields.
- An **account** is a configured identity under `[accounts.ID]`, referencing its implementation with `provider = "NAME"`.
  It owns credential bindings, endpoint overrides, discovery and model availability. Several accounts can use one provider.
- A **profile** scopes account references and an optional qualified default model. It does not create an authentication
  identity. Sharing an account shares credentials; see [profiles](profiles.md).

## Inspect compiled-in providers

```sh
bee providers list
bee providers show github
bee providers show google-cloud
```

These commands inspect definitions independently of configuration, credentials and public Models.dev discovery.
`bee models providers` instead browses Models.dev. The complete compiled-in inventory is `openai`, `anthropic`,
`openrouter`, `chatgpt`, `github`, `ollama`, `opencode`, `opencode-go`, `deepseek`, `google-aistudio`, `google-cloud`,
`groq`, `mistral`, `xai`, `cloudflare` and generic `custom`. Groq uses `groq` and `GROQ_API_KEY`;
Grok models belong to `xai`, using `XAI_API_KEY`. DeepSeek uses `DEEPSEEK_API_KEY`, and Mistral uses `MISTRAL_API_KEY`.
See [provider setup and credentials](index.md#authentication) for endpoints, protocols and vendor-specific facts.

## Configure multiple accounts

```toml
model = "personal/your-personal-model"

[accounts.personal]
provider = "openai"
credential = { env = "PERSONAL_OPENAI_KEY" }
discovery = false
models = ["your-personal-model"]

[accounts.work]
provider = "openai"
display_name = "Work"
credential = { store = true }
discovery = false
models = ["your-work-model"]
```

`enabled` defaults to true and controls baseline profile membership; explicit profile `accounts` lists replace that
baseline and can include disabled accounts. Root/profile `model` is optional and qualified. Account model declarations
are raw IDs, not defaults. No first account/model is selected automatically.

ACP provides one model picker across active accounts, with qualified IDs such as `work/your-work-model`. Labels combine
the account display name (provider label, then account ID as fallback) and model display name (raw ID as fallback).
Qualified IDs remain visible when labels collide and are always the machine values. Relabeling does not change identity.
Display names must be nonblank, control-free and at most 256 UTF-8 bytes.

Authenticate accounts independently. Logout affects every profile referencing that account. Changed credentials or
targets may require explicit reselection; Bee never substitutes another account after failure.

## Custom endpoints

```toml
model = "gateway/private/arbitrary-model.v2"

[accounts.gateway]
provider = "custom"
protocol = "openai-chat"
base_url = "https://gateway.example/v1"
credential = { env = "GATEWAY_KEY" }
headers = { "X-Team" = "engineering" }
options = { max_tokens_field = "max_completion_tokens" }
discovery = false
models = ["private/arbitrary-model.v2", "another-model"]
```

Supported protocols are `openai-chat`, `openai-responses` and `anthropic-messages`. Custom accounts require an explicit
base URL. With discovery off, exactly the declared `models` are selectable and executable; omitted or empty means none.
With discovery on, a successful account listing is intersected with optional `models`; omission applies no filter and
`[]` permits none. Public catalog entries, defaults and metadata/routing keys never expand availability.
See [availability and discovery](models.md#account-model-availability).

Omit `credential` for unauthenticated endpoints. Options are [typed protocol settings](inference.md), not arbitrary JSON.
Multi-protocol accounts use `protocols` and `prefer` as described in [configuration](index.md#providers-and-models).
Do not combine expanded and singular route forms. Configuration cannot implement a new protocol, OAuth issuer,
refresh grant or browser flow.

## Login and credential sources

```sh
bee auth login github
bee auth login personal
bee auth login work --with-token
bee auth status
bee auth logout work
```

An omitted login/logout target resolves only when exactly one account is active; root/profile defaults and recency are
irrelevant. Explicit targets may name configured disabled accounts. Authenticating an existing account does not change
its `enabled` setting or profile membership.

An existing account ID wins over a provider name. If none exists and the name is a native provider, successful first-use
login can create `[accounts.NAME]` with `provider = "NAME"`, enabled by default. A newly created account may be appended
to an explicitly restricted active profile's `accounts`, subject to effective-layer/origin checks. Higher-priority
membership overrides may require editing their source first. Custom names must be configured first. Login writes no
default model. Status and ordinary inference do not create declarations or import environment credentials.

For an environment-backed account, explicit login validates and stores the complete credential generation, then changes
user configuration to `credential = { store = true }`. Subsequent requests use stored authority even if the environment
changes or disappears. `--with-token` imports stdin instead; missing environment values fail without another source.
Cloudflare interactive login can ask for a manual token when its environment variable is unset.
Higher-priority credential-source overrides must be resolved before authority can switch. Browser/device options use
native acquisition. Publication preserves comments, unrelated concurrent edits and no-op reauthentication behavior.

OpenRouter browser sign-in produces an API key. ChatGPT uses verified OAuth registration and renewable tokens; GitHub
supports device OAuth and explicit user tokens. Cloud ADC stays externally owned: Bee renews supported ADC without
importing or modifying it. Native authentication, headers and trusted targets remain active with `discovery = false`;
the boolean controls listing, independently of compiled-in native capabilities.

Native definitions declare field shapes, ownership and secrecy. Secrets belong in private storage or environment
bindings. `settings` holds provider-specific configuration, such as Google Cloud project/location; the `google_cloud`
table is also supported, but the two forms cannot be combined. Private settings never enter ordinary Debug, public
model metadata or ACP selectors. Scalars and arrays replace provider defaults; headers, model maps and protocol tables
merge per key, while `credential` replaces whole.

Credential files read v1/v2 material and write v3 typed material under a lock, preserving unrelated entries, MCP
audiences, host registration and logout revisions. Older tokens do not prove their acquisition history. Refresh,
expiry and revocation remain provider-owned; incompatible material requires reauthentication. API-key logout removes
Bee's copy, not the remote key. Stored authority never falls back to an environment key after logout or rejection.

Configuration and credentials are separately atomic files. If credentials publish but configuration publication fails,
login reports partial success and asks you to review configuration and repeat login. It never blindly restores or
deletes credentials another process may have changed. Success output follows configuration commit; browser prompts
remain live during authorization.

See [ADR-027](adr/027-provider-registry-and-instance-authentication.md) for registry and native-runtime rationale and
[ADR-028](adr/028-account-model-availability-and-selection.md) for the current account contract.

## Cloudflare AI Gateway

`cloudflare` uses Cloudflare's current [REST inference API](https://developers.cloudflare.com/ai-gateway/usage/rest-api/)
with Chat Completions, Responses and Anthropic Messages. Create a Cloudflare API token with
**Account → Workers AI → Read**, restricted to the intended Cloudflare account. An AI Gateway-only token cannot call
these `/ai/*` endpoints. Third-party models require the documented Unified Billing credits. Gateway permissions are
account-scoped; separate local accounts do not create remote per-gateway token scopes.

Find the Account ID in the Cloudflare dashboard's account details and the Gateway ID in **AI → AI Gateway**.
Create the gateway first, then run:

```sh
bee auth login cloudflare
```

Login asks for missing Account ID and Gateway ID. It imports `CLOUDFLARE_API_TOKEN` if set; otherwise interactive login
asks for the token with terminal echo disabled. The identifiers are persisted as private account settings;
the token goes only into Bee's
credential store. Login does not select a model. Add explicit raw `models` afterward or configure the account first.
Bee accepts unpadded ASCII identifiers containing letters, digits, dots, hyphens and underscores, at most 64 bytes.
Use the exact dashboard IDs; local structural validation does not verify that the resources exist.

Two named accounts can use different gateways, Cloudflare accounts and token environment bindings:

```toml
[accounts.cf-work]
provider = "cloudflare"
settings = { account_id = "YOUR_WORK_ACCOUNT_ID", gateway_id = "work-gateway" }
credential = { env = "WORK_CLOUDFLARE_TOKEN" }
discovery = false
models = ["anthropic/claude-sonnet-4", "@cf/openai/gpt-oss-120b"]
model_protocols = { "anthropic/claude-sonnet-4" = "anthropic-messages", "@cf/openai/gpt-oss-120b" = "openai-responses" }

[accounts.cf-personal]
provider = "cloudflare"
settings = { account_id = "YOUR_PERSONAL_ACCOUNT_ID", gateway_id = "personal-gateway" }
credential = { store = true }
models = ["@cf/meta/llama-3.1-8b-instruct"]
```

`bee auth login cf-work` imports its configured environment token. For noninteractive login, configure both identifiers
first and pipe one token line into `bee auth login cf-personal --with-token`. Piped stdin is never used to collect IDs.
Configured identifiers are preserved, including during reauthentication. Existing-account login does not enable an account
or change profile membership. In ACP, explicitly select a qualified model such as `cf-work/anthropic/claude-sonnet-4`.

Every request derives its official account endpoint from `account_id` and sends `cf-aig-gateway-id` from `gateway_id`.
Authentication and gateway headers cannot be overridden. Discovery is disabled and cannot be enabled for this provider;
declare available models explicitly. See [Cloudflare requests](inference.md#cloudflare-ai-gateway-requests) for protocol
restrictions. Bee uses API tokens and does not import Wrangler browser credentials. Logout removes only the selected
account's stored token, does not revoke it remotely, and never falls back to an environment token.
