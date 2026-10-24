# ADR-010: Let a provider speak several protocols, ordered by `prefer`

## Status

Accepted. Amends [ADR-007](007-provider-profiles-and-model-selection.md), which gave a
profile a single `protocol`, `base_url` and `options`.

## Context

A single provider often serves more than one protocol, and not always at one URL or
for every model: a gateway can expose the OpenAI Chat and the Anthropic Messages
protocols, each under its own path, and which one suits depends on the model. With one
`protocol` per profile, a user needs one profile per protocol, duplicating the
credential and headers and giving the provider two names.

The settings a protocol needs are not shared either. `base_url` differs per protocol,
and `options` are typed per protocol (`max_tokens_field` means nothing to Anthropic
Messages), so neither can stay on the profile once there are several.

## Decision

A profile lists the protocols it speaks as a table, one entry per protocol, and each
entry carries what belongs to that protocol:

```toml
[providers.gateway]
prefer = ["anthropic-messages", "openai-chat"]
credential = { env = "GATEWAY_KEY" }          # shared by every protocol

[providers.gateway.protocols.openai-chat]
base_url = "https://gw.example/openai/v1"     # optional; the protocol's default
options = { max_tokens_field = "max_completion_tokens" }

[providers.gateway.protocols.anthropic-messages]
base_url = "https://gw.example/anthropic"
```

`protocols` is required and non-empty. `base_url` and `options` move under their
protocol and are no longer accepted on the profile, and neither is the singular
`protocol`: they are refused as unknown keys, since silently reading them as the first
protocol's would hide the change. `model`, `models`, `credential` and `headers` stay on
the profile: they describe the provider, not a wire format.

### The table has no order, so the preference is stated

A TOML table is sorted, and configuration layers and presets merge into it key by key,
so an order read from it would depend on how the names sort. `prefer` is an array of
protocol names, most preferred first, and it is what orders the endpoints:

- one protocol needs no `prefer`;
- several protocols without one is `bee::provider::no_preference`, the same rule as
  several profiles without a default `provider` (ADR-007): Bee does not guess;
- `prefer` may name a protocol only once, and only one the profile lists;
- a listed protocol that `prefer` leaves out follows the preferred ones, in the fixed
  order of the supported protocols, so the result never depends on the file.

`Profile::endpoints()` returns them in that order, `Profile::endpoint(protocol)` looks
one up, and `Profile::preferred()` is the first. The profile's `protocol()`,
`base_url()` and `options()` are the preferred endpoint's.

### Which protocol a request uses

The first, for now. Choosing by model (the same provider serving some models over one
protocol and others over another, as Cloudflare does) is wanted, and is left for when a
backend needs it. The shape leaves room: a per-model choice is a new key that selects
among `endpoints()`, and `prefer` stays the answer when it selects nothing. It adds
configuration and changes none that exists.

### Presets and layers

Presets use the same shape. Because each protocol is its own table, a profile or a
higher layer overrides one protocol's `base_url` and leaves the others, and adding a
protocol to a single-protocol preset is a table, not a rewrite. `prefer` is an array
and replaces whole, like `models`. A profile that adds a second protocol to a preset
that names one therefore has to say which it prefers.

### Names

The protocol names stay `openai-chat` and `anthropic-messages`. A name says which API
it speaks, because a vendor can offer several (`openai-responses` is another protocol
and will be another name), so a short `openai` would be ambiguous. A name is added with
the backend that implements it, not before.

## Consequences

- Any configuration written with a `protocol`, a profile-level `base_url` or `options`
  is refused at launch with `bee::provider::unknown_key`. Bee is unreleased, so no
  migration or alias is provided.
- Diagnostics about an endpoint name its full key, for example
  `providers.gateway.protocols.openai-chat.base_url`.
- An unknown name under `protocols` is reported against the `protocols` table and the
  name is not echoed, as an unknown value would not be.
- A provider that serves two protocols from one URL repeats it in both entries.
- The shipped presets each list a single protocol, so they need no `prefer`.

## Alternatives rejected

- **`protocols` as a list of names with `base_url` and `options` on the profile.** One
  URL cannot fit both protocols, and the options are typed per protocol.
- **A list of names plus a separate `endpoints` table.** Two places name a protocol, and
  a higher layer replaces the whole list instead of adding one protocol.
- **File order as the preference** (an order-preserving table). Implicit, and ambiguous
  once layers merge.
- **Keeping the singular `protocol` as shorthand.** A second way to write the same thing,
  with a rule for what happens when both appear.
- **A per-session protocol option now.** More surface with no backend to need it.
