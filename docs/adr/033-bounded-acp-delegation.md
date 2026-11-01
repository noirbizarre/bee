# ADR-033: Bound ACP task delegation at the execution and ownership boundaries

## Status

Accepted. Extends the generic outbound ACPv2 client from issue #20 and the
turn-owned interaction/lifecycle contracts of ADR-005 and ADR-017.

## Context

Issue #21 needs a parent model to delegate focused tasks to configured ACPv2
agents and synthesize their answers. The model loop executes tools sequentially.
Simply registering a one-child blocking tool would not support sibling concurrency;
parallelizing all tools would change native side-effect and guidance ordering.
Dropping inference on cancellation also cannot await child-session cleanup.

## Decision

- Offer one opt-in batch-capable `delegate` tool at the ACP boundary, preserving
  the SDK-free core and sequential native-tool contract. Each task launches a
  fresh configured process and ACPv2 session. No Hive-specific behavior is needed.
- Use ordinary configuration layering, disabled by default. Enforce total child
  admission per parent turn and concurrency across the inbound ACP connection.
  Reserve batches atomically; admitted failures still consume count. Deadlines
  include queueing, while cleanup has separate bounded grace.
- Apply default-ask `delegate` permissions to named agents before admission.
  Forward child structured permissions/forms/URLs with client-owned policy,
  namespaced URL IDs and scoped aggregate waiting leases. Delegation approval
  grants no automatic child-operation approval.
- Enforce depth in code, default one. Apply the inherited
  `BEE_DELEGATION_REMAINING_DEPTH` ceiling after configuration and install a
  decremented runtime-owned child environment override. Invalid inherited
  authority fails closed. This cooperative Bee contract is not a sandbox for
  arbitrary executables or a tree-wide concurrency/count service.
- Keep child supervisors outside abortable inference, registered before launch.
  Retain capacity and completion ownership until cleanup finishes. Use the
  pinned SDK's process-launch/byte-stream APIs for delegation so normal cleanup
  can explicitly kill/reap before parent idle, while future drop retains a
  synchronous termination fallback.
- Send only explicit task/context strings. Reuse the outbound message reducer
  for bounded partial capture outside callback lifetime. Return ordered valid
  JSON results, sanitized failures and explicit incomplete/truncated markers.
  Use compact standard parent tool progress instead of experimental child-session
  entities or forwarding child IDs/thoughts/terminals.

## Consequences

The parent can synthesize independent successful and partial child answers across
all three inference protocols. Ordinary failure remains tool data. Cleanup and
overlapping interaction state require independent ownership barriers and scoped
leases rather than a task-local future or one waiting flag per session.

Each task pays process/session startup cost; connection reuse and persistent task
handles are intentionally unnecessary. Child runtimes own their model selection,
authentication and filesystem/environment access. A Bee child may retain its own
durable session journal. Parent replay is presentation/context restoration, never
an execution queue. Non-Bee agents may ignore the inherited-depth contract.

`tests/delegation.rs` covers production synthesis, real Bee depth enforcement,
concurrency, interaction forwarding, limits, cancellation and cleanup ordering.
