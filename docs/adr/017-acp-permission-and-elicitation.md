# ADR-017: Turn-owned ACP permission and elicitation

## Status

Accepted. Extends ADR-005, ADR-008 and ADR-015 for issue #46.
Extended for #78 with ordered native routing and client-owned always decisions.

## Context

The shipped agent needs concrete client interactions: authorization before shell
execution, task information from the user, and provider login when stored credentials
are unavailable. ACPv2 already defines these operations; MCP `input_required` and
task continuation are separate contracts. The client owns permission policy.

The SDK can cancel outgoing requests when their futures are dropped. The existing
turn task owns cleanup and terminal idle, while direct close awaits its completion.
Detached interaction workers would violate that lifecycle and permit late answers
to affect a replacement turn.

## Decision

Keep interactions in a private ACP module, wrapping the native executor while
preserving guidance/snapshot dispatch. The original #46 subset requested permission
for each validated Bash call with allow-once/reject-once. The #78 extension replaces
that subset: support all registry-native tools with typed action/resource checks.
Reads/searches/listing default allow; Bash, mutations and custom tools default ask.
Ordered TOML rules adapt OpenCode v2's action/resource/effect syntax. Last matching
trusted rule wins; project policy independently tightens the result and cannot
erase a trusted ask/deny. Multi-resource deny dominates ask, then allow.

Ask uses an exact command/cwd subject for Bash and a pending tool-call subject for
other native operations, correlated with Bee's announced ID. Offer all four
standard allow/reject once/always choices; only an exact offered allow ID permits
this invocation. Clients alone own always persistence and matching. Bee stores
no grants or rejections and still sends later ask-routed requests. Shell matching
uses whole raw command text without parsing. Native rules do not add another
authorization layer to client-hosted MCP. Form input and plan updates retain their
own flows. Keep tool presentation pending until actual authorized dispatch.

Negotiate form and URL capabilities independently. Form support adds `ask_user`,
with one bounded required string answer. This deliberately avoids a generic
arbitrary-schema questionnaire framework. Correlate form input with session and
model call ID; validate accepted content before returning a tool result. Dialog
nonacceptance and request errors become safe tool errors; session cancel/close
aborts the turn.

URL support enables prompt-time browser login for missing/expired store credentials
with a configured login flow. URL acceptance does not mean external completion:
only a validated callback, successful exchange and credential persistence allow
`elicitation/complete`. Use a connection-unique elicitation ID and session scope;
login has no model tool call to correlate. A generic model-supplied URL tool was
rejected because it has no trusted external completion source.

Share SDK-free PKCE, callback interpretation and exchange parsing between CLI and
async login. Own async listener and network futures in the turn. A detached blocking
login worker was rejected because dropping its waiter would not stop its side effects.
Bound human waits to five minutes and exchange transport to thirty seconds.

Publish `requires_action` while blocked and retain that phase for active resume.
Token-gate state/output/completion and linearize Bash startup against cancellation.
Reuse the existing cleanup-before-idle barrier and close-response ordering.

Serialize the short final credential commit across ACP sessions/connections in
the process, under the current-turn gate. Reuse a valid credential installed by
another login instead of overwriting it. Cancellation before commit prevents it;
cancellation after commit does not roll it back. Unsafe/unreadable stores are errors,
not triggers for replacement. No credentials or raw peer/transport errors reach
the model or diagnostics.

## Consequences

The agent remains headless and client-agnostic. SDK types stay at the ACP boundary;
the model loop and login mechanism remain SDK-free. Clients without form/URL support
are never sent unsupported elicitation modes; Bash permission has no capability flag.
MCP interaction continuation remains unsupported.

Final store writes can briefly block dispatch, as existing filesystem publication
does. This bounded design avoids an asynchronous persistence subsystem; independent
CLI processes retain the existing store concurrency limitations. Browser login
requires the browser to reach the agent host's loopback interface. CLI login remains
available for other deployment topologies.

Production tests use all three inference protocols, real protected side effects,
controlled client replies, callback sockets, and a credential-exchange seam.
Compatibility coverage records those tests separately from TCK interaction paths
that the current TCK fixture does not initiate.
