# ADR-009: Describe provider presets and browser sign-ins as data

The preset/configurable-flow decisions below are superseded by
[ADR-027](027-provider-registry-and-instance-authentication.md). This document records the original rationale.

## Status

Accepted. Amends [ADR-008](008-provider-authentication.md), which made OpenRouter's
sign-in a piece of Rust.

## Context

After ADR-008, a profile is already data: adding a gateway or a local server needs no
code. Two things were still coded per provider:

- **The OpenRouter sign-in.** Its URLs, query parameter names, JSON field names and
  the field the key comes back in were constants. A second provider with the same
  shape would have needed a new adapter, for no new idea.
- **The profile itself.** Every user wrote the same `protocol`, `base_url` and
  `credential` for the same well-known providers.

The part of a sign-in that is genuinely code is the *mechanism*: PKCE, a loopback
callback with `state`, an exchange. What differs between providers of one mechanism
is names and URLs. ADR-008 rejected a universal OAuth client because providers
differ in what they return and whether it refreshes. That argument is about
*mechanisms*, not about parameter names, and it still holds.

## Decision

### A catalogue of presets and login flows, as TOML

`data/presets.toml` is embedded in the binary. It holds two tables:

- `presets.<name>`: a profile's keys (`protocol`, `base_url`, `credential`, ...). A
  profile writes `preset = "<name>"` and overlays its own keys.
- `login_flows.<name>`: a browser sign-in, by the provider's own names.

```toml
[login_flows.openrouter]
kind = "pkce-key"
authorize_url = "https://openrouter.ai/auth"
exchange_url = "https://openrouter.ai/api/v1/auth/keys"
headless = true                       # the provider can show the code on screen

[login_flows.openrouter.authorize]    # query parameter names
callback = "callback_url"
challenge = "code_challenge"
challenge_method = "code_challenge_method"
state = "state"
extra = { key_label = "Bee" }

[login_flows.openrouter.exchange]     # JSON field names
code = "code"
verifier = "code_verifier"
challenge_method = "code_challenge_method"
response_key = "key"                  # where the credential comes back
```

The user's own `presets` and `login_flows` tables, in any layer where that is allowed
(below), are merged over the shipped ones **by name, key by key**: adding an entry
extends the catalogue and naming a shipped one overrides it. A flow is read when a
profile uses it, so an unused entry in a shared file cannot stop Bee from starting.

### What stays in Rust

- **Protocols** (`openai-chat`, `anthropic-messages`): each is a backend.
- **Flow kinds.** A flow states its `kind`, and only `pkce-key` exists: an
  authorization code with PKCE whose exchange returns a credential directly. The
  method is always `S256`, because the alternative puts the verifier in the URL. A
  flow with refresh tokens, a device code or a form-encoded exchange is a new `kind`
  and new code. Bee does not pretend the data format covers them.

### Overlaying a profile on a preset

Scalars and arrays (`model`, `models`, `base_url`) from the profile replace the
preset's. `headers` and `options` merge key by key. `credential` is replaced
**whole**: merging would put the profile's `{ env }` beside the preset's `{ store }`,
which is two sources, and a profile's choice of one has to win outright. There is no
way to unset a key; a profile that wants no credential picks a preset without one. A
preset cannot name another preset, so there are no chains and no cycles.

The combined table goes through the same validation as any profile, so there is one
set of rules. A diagnostic about a value the preset supplied names the file that
defines the preset, or `<built-in preset NAME>`, instead of `<unknown>`.

### The project file may not define a login flow

A login flow chooses the URLs that receive the authorization code, the PKCE verifier
and the key. The project file (`.config/bee.toml`) can come from a repository the user
only cloned, so any `login_flows` key it sets, including one field of a shipped flow,
is refused with `bee::provider::untrusted_layer`. The global file and the file
`BEE_CONFIG` names are the user's own and are accepted. Presets are allowed in the
project file: they are shorthand for a profile, and a project could already write that
profile out. Endpoints must be `https://` with no credentials, query or fragment, and
parameter names are restricted to letters, digits, `_`, `-` and `.`.

### What the shipped catalogue leaves out

- **Model lists.** They are stale the day they are written. A profile names its own
  `model`.
- **Cloudflare.** Its base URL carries an account id and there is no templating; the
  documentation shows a profile to copy. Introducing templating for one provider would
  be a feature of its own.

## Consequences

- A provider with an authorization-code-with-PKCE sign-in that returns a key needs no
  release: a `login_flows` entry in the global file is enough.
- `LoginFlow`, the closed enum of ADR-008, is gone. `credential.login` is a flow
  *name*, resolved when the profile is read, so a dangling or malformed flow stops Bee
  at launch with the key at fault.
- Overriding a shipped flow is possible and is therefore a way to redirect a sign-in;
  that is why only the user's own files may do it.
- The catalogue ships with the binary, so correcting a preset is a release. A user can
  correct it in the meantime by overriding it.

## Alternatives rejected

- **A remote catalogue, fetched at runtime.** Bee would send the authorization code
  wherever a server told it to. Embedded data has no such trust problem.
- **A flow definition in the project file.** See above.
- **Per-provider Rust adapters (ADR-008's position).** Correct while there was one
  provider; with two it is a copy.
- **Templating in presets** (`${ACCOUNT_ID}`). It would solve Cloudflare and invite an
  expression language into configuration. Deferred until more than one provider needs
  it.
- **`credential` merged key by key.** See above.
