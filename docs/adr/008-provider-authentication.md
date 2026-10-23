# ADR-008: Authenticate providers through credential sources, with one browser sign-in per provider that has one

## Status

Accepted.

## Context

[#12](https://github.com/noirbizarre/bee/issues/12) asks for secure, reusable provider
authentication, with browser sign-in early for the providers Bee targets. Several
things look alike and are not:

- **The provider protocol** (how requests are shaped) and **the credential**
  (what proves who is asking). ADR-007 keeps the first in the profile; the second
  must not be welded to it.
- **An OAuth access token** and **an API key**. OpenRouter's browser flow is an
  authorization-code flow with PKCE, but what it returns is a long-lived *API key*
  used as a bearer token, not an access and refresh token pair. Cloudflare's
  Workers AI and AI Gateway use scoped API tokens; Wrangler's browser login yields
  an OAuth token for Cloudflare's own tooling, whose client, scopes and file
  format belong to Wrangler.
- **LLM provider credentials** and **MCP server authorization**. MCP's
  authorization specification is OAuth 2.1 with metadata discovery, for a
  different audience (a tool server), obtained and held for different reasons.
  MCP configuration is the client's concern (ADR-002), and the need for Bee to be
  an MCP *client* itself only becomes concrete with
  [#20](https://github.com/noirbizarre/bee/issues/20).

No LLM is integrated yet. This decision resolves credentials and runs sign-in; the
backends of [#8](https://github.com/noirbizarre/bee/issues/8) and
[#23](https://github.com/noirbizarre/bee/issues/23) will call `auth::resolve` and
`auth::header_for` when they send requests.

## Decision

### A credential is a source on the profile, apart from protocol and model

`credential` (ADR-007) gains a second source. Exactly one of:

```toml
credential = { env = "OPENROUTER_API_KEY" }                # a variable
credential = { store = true }                              # Bee's credential store, token only
credential = { store = true, login = "openrouter" }        # store, and browser sign-in available
# no credential: an unauthenticated endpoint, such as a local server
```

`login` names a provider-specific sign-in and is only valid with `store`. A literal
key is still not accepted anywhere in configuration. The protocol decides only how
the resolved secret is *presented* (`auth::header_for`): `Authorization: Bearer`
for `openai-chat`, `x-api-key` for `anthropic-messages`. Where the secret came from
never changes that.

A named but unavailable credential (unset variable, nothing stored, expired) is an
error with the command that fixes it, never a silent unauthenticated request.
Resolution happens when a backend needs the credential, not at launch, so a
profile that is not used cannot stop Bee from starting.

### Bee keeps what it obtains in a private file

`$XDG_DATA_HOME/bee/credentials.json` (else `~/.local/share/bee/credentials.json`),
the same hand-rolled XDG rule as ADR-006, in the data directory because it is state
Bee writes and not configuration to commit. The file is created `0600` in a `0700`
directory, replaced atomically (temporary file, `fsync`, rename), and **refused on
read** if group or others have any access, with `chmod 600` as the fix. It is read
by hand over a generic JSON value: serde's type errors quote the offending value,
which here is a key (the reason ADR-006 gives for TOML). Nothing is written on
platforms where Bee cannot make a file private; `env` still works there.

The store is a trait, `CredentialStore`, with the file as its one implementation,
so an OS keyring can be added beside it without touching callers.

### Audiences are stored apart

Entries live in separate tables for **provider profiles** and **MCP servers**. The
API takes an `Audience` and no lookup can cross them, even for equal names. Only the
provider table is used today; the MCP table is reserved so that the MCP
authorization flow can arrive without a migration and without any chance of
offering an LLM key to a tool server or the reverse.

### OpenRouter: PKCE with a loopback callback, or a pasted code

`bee auth login` on a profile with `login = "openrouter"`:

1. generates a 32-byte PKCE verifier and a `state` from the OS random generator;
2. binds `127.0.0.1` on a free port, so nothing off the machine can reach it;
3. prints the `https://openrouter.ai/auth` URL (S256 challenge, `state`, a
   `http://localhost:<port>/callback` redirect, which OpenRouter allows on any
   port) and tries to open it, unless `--no-browser`;
4. waits (five minutes) for the redirect, rejecting a `state` that is not its own,
   answering other paths with 404 and reading at most 8 KiB per request;
5. `POST`s the code and the verifier to `/api/v1/auth/keys` and stores the returned
   key.

`--paste-code` is the headless variant for SSH sessions and containers: the URL has
no redirect, OpenRouter displays the code, the user pastes it. A challenge is always
sent, which is what makes a displayed code useless to anyone but Bee. `--with-token`
skips sign-in and reads a key from standard input, the path for any provider without
a flow.

The flow is OpenRouter's own adapter over small shared primitives (PKCE, the
callback listener, percent encoding), not a generic OAuth client: a second provider
differs in what it returns, how long it lives and whether it refreshes, and a
universal abstraction would have to erase exactly that.

### No refresh

The only flow Bee has returns a key that does not expire and has no refresh token,
so Bee implements no refresh. The store records an optional expiry, and a credential
past it, or a key the provider rejects, is reported with `bee auth login` as the
remedy. Refresh arrives with the first provider whose flow has one, as that
provider's adapter.

### Cloudflare: scoped API token only

Workers AI and AI Gateway are reached with a scoped API token, from the
environment or the store:

```toml
[providers.cloudflare]
protocol = "openai-chat"
base_url = "https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1"
credential = { env = "CLOUDFLARE_API_TOKEN" }
```

Bee does **not** read or reuse Wrangler's OAuth token or run its login: the OAuth
client and token file are Wrangler's, are not documented as a stable interface, and
its scopes are not the same as an API token's. Treating them as interchangeable
would fail in ways that look like a Bee bug. The account id is part of the base URL,
which is profile configuration. A gateway's own authentication header is not part of
this decision.

### An HTTP client in a library with no runtime

`ureq` with `rustls`, blocking: it needs no async runtime, which keeps ADR-003
intact, and no system TLS library, so the musl targets link as before. The client
sits behind an `Http` trait with one method, so every flow is tested against a fake
and one test checks the real client against a loopback server. It only speaks HTTPS
unless a test asks otherwise.

### Secrets stay out of every output

`Secret` has no `Display` and a `Debug` that prints `<redacted>`; the one way out is
`expose()`. The authorization code, the verifier and the key are `Secret`s. Errors
carry a profile, a variable *name*, a path or an HTTP status, and fixed text; a
provider's response body is never included. `Environment`'s `Debug` lists variable
names only. `bee auth` prints sign-in URLs and prompts to stderr and writes only
`status` to stdout, which the ACPv2 path never touches. Credential variables are read
through an injectable `Environment`, so tests never mutate the process's own.

### Commands

`bee auth login [PROFILE] [--with-token | --paste-code | --no-browser]`,
`bee auth logout [PROFILE]` and `bee auth status [PROFILE]`. The profile defaults to
the default profile. `login` replaces an existing credential; `logout` is idempotent,
removes only Bee's copy and never an environment variable; `status` never prints a
credential. Bare `bee` still serves ACPv2.

## Consequences

- Adding a provider with a browser flow is a `LoginFlow` variant and an adapter;
  adding a credential source is a variant and an accepted key.
- Bee depends on `ureq`, `sha2`, `base64`, `getrandom` and `serde_json`, and
  `rustls`'s `ring` provider needs a C compiler where it is built from source.
- Credentials are plain text at rest, protected by file permissions. That matches
  common CLI practice and is what the OS keyring option, deferred behind the store
  trait, would improve.
- A key that leaks from the file stays valid until revoked with the provider;
  `logout` says so.
- Nothing refreshes. If a provider's flow expires keys, it must be added with that
  provider.
- Remote MCP authorization (discovery, registration, PKCE, refresh) is not built.
  It will be its own flow and its own audience, driven by #20.

## Alternatives rejected

- **Literal keys in configuration.** Keeps secrets in files that get committed.
- **An OS keyring now.** Needs a session bus or keychain that headless machines and
  containers lack, and pulls platform crates into every build. Kept possible by the
  store trait.
- **A universal OAuth client (the `oauth2` crate) behind one config shape.** It
  models access and refresh tokens, and OpenRouter returns neither; the differences
  between providers are the part that matters.
- **Reusing Wrangler's token for Cloudflare.** See above.
- **`reqwest`.** Needs tokio in the binary, against ADR-003.
- **Driving sign-in through ACPv2's `authenticate` request.** It would tie
  credentials to whichever client launched Bee, and ACPv2 provides no universal
  implementation of provider authentication.
- **Resolving credentials at launch.** A missing key for a profile nobody uses would
  stop every session.
