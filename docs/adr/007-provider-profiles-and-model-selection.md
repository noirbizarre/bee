# ADR-007: Describe providers as named profiles and let sessions select among them

## Status

Accepted.

## Context

Bee must reach any endpoint (a commercial API, a gateway, a local server) and any
model id (private, fine-tuned, absent from every catalog) without a release. Two
things get conflated easily:

- **Provider configuration**: where an endpoint is, how to talk to it, which
  headers it needs, where its credential lives. It belongs to the user's
  configuration files and may hold secrets.
- **Session settings**: which provider and model *this session* uses. They belong
  to the client, over ACPv2, and change from session to session.

[#7](https://github.com/noirbizarre/bee/issues/7) asks for the first, with a defined
relationship to the second. Credential resolution is
[#12](https://github.com/noirbizarre/bee/issues/12); the backends that call the
endpoints are [#8](https://github.com/noirbizarre/bee/issues/8) and
[#23](https://github.com/noirbizarre/bee/issues/23). No LLM is integrated yet, so
this decision describes endpoints and selection; nothing is sent anywhere.

## Decision

### A profile is a name, a protocol and a connection

```toml
provider = "work"                  # the default profile

[providers.work]                   # any name
protocol = "openai-chat"           # required: how to talk to it
base_url = "https://gateway.corp.example/v1"   # optional
model    = "corp/ft-2026"          # optional default model, any id
models   = ["corp/base"]           # optional suggestions for selectors
credential = { env = "GATEWAY_KEY" }           # optional reference
headers  = { "X-Team" = "infra" }  # optional
[providers.work.options]           # per-protocol settings
```

- **The name is a label.** It is never looked up and implies nothing: a profile
  called `anthropic` may speak `openai-chat`. There is no registry of commercial
  provider names and no plugin system.
- **`protocol` selects the backend** and is a closed set, because each value is
  code that exists: `openai-chat` and `anthropic-messages`. A new *endpoint* needs
  no change in Bee; a new *protocol* is a new backend.
- **`base_url`** defaults per protocol (`https://api.openai.com/v1`,
  `https://api.anthropic.com`), not per provider, and is overridable.
- **Models are opaque.** Any non-empty id without control characters or edge
  whitespace is accepted. `models` is a hint for selectors, never an allowlist.
  Model metadata, if ever wanted, is optional (#24).
- **`credential`** is a *reference*: exactly one source kind, today `env` naming a
  variable. It is shape-checked (a variable name, so a pasted key is refused) and
  stored; #12 resolves it and may add kinds. Literal keys are not accepted.
- **`headers`** are valid HTTP token names with values free of control characters,
  and names that differ only in case are refused. They belong to their profile.
- **`options`** are checked against the profile's protocol: `max_tokens_field`
  (`max_tokens` or `max_completion_tokens`) for `openai-chat`; `version` and
  `max_tokens` for `anthropic-messages`. An option for the other protocol is an
  unknown key.
- **Unknown keys are errors** in a profile, `credential` and `options`: a typo such
  as `base_ulr` would otherwise send traffic to the default endpoint silently.
- **The default.** `provider` must name a defined profile. With one profile it may
  be omitted; with several it is required, because picking one by name order would
  make the default depend on spelling.

Profiles come from the layered configuration of ADR-006 and inherit its merge
rules: a project can change one header or the model of a global profile, `models`
is replaced whole, and a type change is a conflict.

### Validation and secrets

`Providers::from_config` runs in `bee::run()` before any protocol traffic, so a bad
profile stops the launch with a diagnostic on stderr. It is written by hand over the
TOML value rather than derived with serde, because serde's messages quote the
offending value and configuration is where API keys live. Every diagnostic names a
dotted key, a file and fixed text, never a value. `Debug` for a profile lists header
*names* only. `tests/provider.rs` pins this for every kind of error.

| Code                             | When                                                    |
| -------------------------------- | ------------------------------------------------------- |
| `bee::provider::wrong_type`      | a setting has the wrong TOML type                       |
| `bee::provider::unknown_protocol`| `protocol` names no supported protocol (lists them)     |
| `bee::provider::unknown_key`     | a table holds a key Bee does not know (lists the valid) |
| `bee::provider::missing_key`     | a profile has no `protocol`                             |
| `bee::provider::invalid`         | right type, unusable value (URL, header, model, option) |
| `bee::provider::unknown_default` | `provider` names an undefined profile                   |
| `bee::provider::no_default`      | several profiles and no `provider`                      |

### ACPv2 session settings and their precedence

Bee offers two ACPv2 session configuration options when a provider is configured,
on `session/new` and `session/resume`, and handles `session/set_config_option`:

| Option     | Category    | Values                                                        |
| ---------- | ----------- | ------------------------------------------------------------- |
| `provider` | `_provider` | the configured profile names                                  |
| `model`    | `model`     | the profile's `model` and `models`, plus the client's choice  |

With no provider configured no option is offered, and Bee advertises no new
capability: the options are the protocol's own mechanism.

Precedence, highest first:

1. **Provider.** The client's choice for the session, then `provider` in the merged
   configuration, then the only profile.
2. **Model.** The client's choice for the session, then the profile's `model`, then
   the first of its `models`.

Semantics that keep the two apart:

- The client chooses **among configured profiles** and may name any model id. It can
  never change a profile's URL, headers, options or credential: those are
  configuration, and a client is not the owner of the user's secrets.
- `session/set_config_option` accepts **any well-formed model id**, listed or not, so
  a model absent from every list is usable; the response then offers it. Listed
  values are suggestions for a selector. An unknown provider is refused
  (`invalid_params`), as is a boolean value or an unknown option id.
- Choosing a **different provider drops the model choice**, since ids belong to a
  provider; choosing the same one keeps it.
- Choices are **per session, in memory**, survive `close` and `resume`, and are never
  written back to configuration.
- A change **while a turn runs is refused** (`invalid_request`): a turn uses the
  settings it started with, so its answer cannot depend on timing.
- A profile with no `model` and no `models` has no model to show, so no `model`
  option is offered for it; a client may still set one by id.

## Alternatives rejected

- **A registry of provider names with built-in defaults.** Every new gateway would
  need a release, and a name would silently imply a protocol.
- **A dynamic plugin system.** Nothing needs it: an endpoint is data.
- **A catalog of models, or an allowlist.** Private and local models are absent from
  any list; catalog presence must never gate use. Optional metadata is #24.
- **Deriving the schema with serde.** Its errors quote values.
- **Literal API keys in configuration.** Keeps secrets in files that get committed;
  #12 owns secret handling.
- **Letting ACP change a profile's endpoint or headers.** It would let a client
  redirect the user's credential.
- **Falling back to another profile when the default is unknown.** It would send
  prompts where the user did not choose.
- **A `url` crate.** The check only has to catch typos before they become connection
  errors; there is none in the tree.

## Consequences

- Nothing calls a provider yet: profiles are validated and selectable, and the
  placeholder reply remains. #8 and #23 add backends behind `protocol`.
- Validation of URLs is deliberately shallow (scheme, host, port, no userinfo, query
  or fragment); an unreachable host surfaces when a backend connects.
- `serve`/`serve_stdio` now take the validated `Providers`.
- Like ADR-006, profiles come from the launch directory; per-session loading is a
  later, local change.
- `provider` (a string) and `providers` (a table) are now meaningful top-level keys,
  so a configuration giving `provider` another type fails at launch. ADR-006's
  "no setting is validated beyond being TOML" no longer holds for them.
