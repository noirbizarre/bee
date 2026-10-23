# bee

Bee is a lightweight, headless agent runtime written in Rust that implements ACPv2. It provides the core agentic loop
and LLM provider integration, allowing it to work with any compatible ACPv2 client.

## Status

Bee is at the **bootstrap stage**: the repository, tooling, CI and release automation exist, and the `bee` binary
serves ACPv2 over stdio, so a client can start it, complete the `initialize` handshake, create sessions and submit
prompts. Bee advertises the baseline session lifecycle (`session/new`, `list`, `resume`, `close`, `prompt`, `cancel`).

**No LLM is integrated yet.** Every prompt is answered with the same fixed placeholder message, which is temporary and
will be replaced by the agentic loop and a provider integration. A running prompt can be stopped with `session/cancel`
or `session/close`, and a prompt sent while one is running is refused. Because the placeholder turn finishes
immediately, this only shows once a real turn takes time. `session/resume` cannot replay history, and prompts accept
`text` and `resource_link` content only. There is no agentic loop either.

## Configuration

Bee reads TOML configuration from up to three files, merged in this order, lowest precedence first:

1. **Global:** `$XDG_CONFIG_HOME/bee/config.toml`, or `~/.config/bee/config.toml` when `XDG_CONFIG_HOME` is unset. The
   same on every platform, macOS included.
2. **`BEE_CONFIG`:** the file named by that environment variable, for settings chosen per client or workspace.
3. **Project:** `.config/bee.toml` in the directory Bee is launched from. Parent directories are not searched.

The global and project files are optional. A `BEE_CONFIG` file that does not exist is an error, and so is any file
that exists but cannot be read or parsed: Bee exits before speaking the protocol and says which file and, for syntax
errors, which line and column. Diagnostics never repeat the contents of a file, since they may hold API keys.

Tables merge key by key, and for a scalar the higher layer wins. An array in a higher layer **replaces** the lower one
instead of extending it. Giving one key different types in two layers (a string over an integer, say) is an error that
names the key and both files. See [ADR-006](adr/006-layered-xdg-configuration.md).

### Providers and models

A provider is a named connection profile, separate from the protocol it speaks and from the model:

```toml
provider = "work"                  # the default profile (optional when there is only one)

[providers.work]                   # any name; it is a label, never looked up
protocol = "openai-chat"           # required: "openai-chat" or "anthropic-messages"
base_url = "https://gateway.corp.example/v1"   # optional; defaults per protocol
model = "corp/ft-2026"             # optional default model: any id, listed anywhere or not
models = ["corp/base"]             # optional suggestions for model selectors, not a limit
credential = { env = "GATEWAY_KEY" }           # optional: names where the secret lives, see Authentication
headers = { "X-Team" = "infra" }   # optional, sent only to this profile

[providers.work.options]           # settings specific to the profile's protocol
max_tokens_field = "max_completion_tokens"
```

Several profiles can use the same protocol, so a gateway, a local server and a vendor API are just three profiles.
Layers merge as above: a project file can override one header or the model of a global profile. Unknown keys,
unsupported protocols and malformed URLs, headers or models stop Bee at launch with an error that names the key and
the file and never the value. A `credential` only *names* where the secret lives; do not put a key in the file (see
[Authentication](#authentication)).

When a provider is configured, Bee offers each ACPv2 session a `provider` and a `model` configuration option and
handles `session/set_config_option`. These are session settings and differ from the configuration above: the client
chooses among configured profiles and can name any model id, but cannot change a profile's endpoint, headers or
credential. The provider is the client's choice, else `provider`, else the only profile; the model is the client's
choice, else the profile's `model`, else the first of its `models`. Choosing another provider resets the model, and
settings cannot change while a turn is running. See [ADR-007](adr/007-provider-profiles-and-model-selection.md).

Profiles are validated and selectable, but no LLM is called yet: they have no effect on the placeholder reply.

## Authentication

A profile's `credential` says where its secret comes from. It is separate from the `protocol` (which only decides how
the secret is presented: `Authorization: Bearer` for `openai-chat`, `x-api-key` for `anthropic-messages`) and from the
model. Exactly one source, or none:

```toml
credential = { env = "OPENROUTER_API_KEY" }            # a variable in the environment that launches Bee
credential = { store = true }                          # Bee's credential store; store a token with `bee auth login`
credential = { store = true, login = "openrouter" }    # the same, and `bee auth login` can sign in with a browser
# no credential: an unauthenticated endpoint, such as a local server
```

An empty variable counts as unset. A credential that is named but missing or expired is an error that says which
command fixes it; Bee never quietly sends a request without it. Credentials are resolved when a request needs them, not
at launch.

### Commands

```bash
bee auth login [PROFILE]                 # browser sign-in, for profiles with `login = "openrouter"`
bee auth login [PROFILE] --paste-code    # no callback: open the URL anywhere, paste the code it shows (SSH, containers)
bee auth login [PROFILE] --with-token    # read an API token from standard input
bee auth logout [PROFILE]                # forget Bee's stored copy
bee auth status [PROFILE]                # where each credential comes from, never the credential itself
```

`PROFILE` defaults to the default profile. `--no-browser` prints the sign-in URL without trying to open it. Prompts and
URLs go to stderr; only `status` writes to stdout. Bare `bee` still serves ACPv2.

### OAuth sign-in, API tokens, inference credentials and MCP authorization

These are different things and Bee keeps them apart:

- **Browser sign-in** is a provider's own flow, and what it returns differs by provider. OpenRouter's authorization-code
  flow with PKCE returns an **API key**, not an OAuth access token: Bee stores it and sends it as a bearer token. The
  key does not expire and cannot be refreshed, so a rejected key is fixed by signing in again. OpenRouter is the only
  provider with a browser flow today.
- **API tokens** are pasted (`--with-token`) or read from a variable. Every provider supports them, and they are the
  only option for providers with no browser flow.
- **Cloudflare** (Workers AI, AI Gateway) uses a *scoped API token* from the environment or the store. Bee does not
  read or reuse Wrangler's browser-login token: it belongs to Wrangler and is not interchangeable with an API token's
  scopes. Put the account id in `base_url`, for example
  `https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1`, with `protocol = "openai-chat"`.
- **Inference credentials** (everything above) authenticate Bee to an *LLM provider*. **MCP server authorization**
  (OAuth 2.1 for a remote tool server) is a separate audience: the credential store keeps the two in separate tables so
  one can never be offered for the other. The MCP authorization flow itself is not implemented; MCP configuration stays
  the client's concern.

### Where credentials are kept

`bee auth login` stores credentials in `$XDG_DATA_HOME/bee/credentials.json` (or `~/.local/share/bee/credentials.json`),
created with mode `0600` in a `0700` directory and replaced atomically. Bee refuses to read the file if other users can
access it, and tells you to `chmod 600` it. `bee auth logout` removes Bee's copy only: the key stays valid with the
provider until you revoke it there. Credentials, authorization codes, PKCE verifiers and response bodies never appear
in diagnostics or debug output. See [ADR-008](adr/008-provider-authentication.md).

OpenRouter example:

```toml
[providers.openrouter]
protocol = "openai-chat"
base_url = "https://openrouter.ai/api/v1"
credential = { store = true, login = "openrouter" }
```

```bash
bee auth login openrouter
```

No LLM is called yet, so a stored credential is not used for requests until the provider backends land.

## Scope

Bee is the agent side of an ACPv2 connection. It runs the agentic loop and talks to LLM providers; presentation and
orchestration belong to the client. The planned boundaries are:

- ACPv2 protocol integration
- agent execution and the agentic loop
- LLM provider integration and model configuration
- session execution and cancellation
- configuration and credential handling
- error handling and observability

## Non-goals

- Bee is an agent runtime, not an orchestration platform.
- Bee does not provide its own TUI, web interface or session-management frontend.
- Bee does not assume it will only be used with Hive or any particular ACPv2 client.
- MCP configuration and orchestration are not Bee-specific responsibilities. Bee should use the capabilities provided
  through the ACPv2 client-agent contract, where the protocol supports them.
- WebAssembly-based extensions are a possible future direction, not an initial requirement.
- Bee does not assume ACPv2 provides a universal implementation of provider authentication. Each browser sign-in is a
  provider-specific adapter, not a generic OAuth client.

## Installation

Bee is not published to crates.io (the `bee` crate name is taken by an unrelated project). Download a binary for your
platform from the [latest release](https://github.com/noirbizarre/bee/releases/latest) once one is published.

## Usage

An ACPv2 client launches `bee` as a subprocess and speaks the protocol on its stdin and stdout. Stdout carries protocol
traffic only; diagnostics go to stderr. Closing stdin ends the session and Bee exits with status 0. `bee auth` is the
one command a person runs by hand.

```bash
bee --help
```
