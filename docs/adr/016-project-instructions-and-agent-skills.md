# ADR-016: Scoped project instructions and standard Agent Skills

## Status

Accepted.

## Context

Projects already describe agent conventions in AGENTS.md, and reusable skills use the vendor-neutral SKILL.md format.
Bee needs both without a proprietary replacement. Instructions must reach all three native inference protocols and
remain correctly counted under ADR-014's whole-turn eviction. Simply prepending user messages would break turn boundaries;
retaining activation tool results after eviction would leave orphaned calls.

Nested instructions introduce another constraint: a tool call generated without a package's rules must not mutate its
files before the model sees those rules. Eagerly loading every nested file would confuse unrelated scopes and unnecessarily
consume context. Bash cannot reliably expose all paths it might touch.

## Decision

Use separate, non-evictable instruction payloads: Chat system messages, Responses instructions and Anthropic system text.
Compose built-in guidance, additive user instructions, explicitly scoped project sources, skill metadata and activated
skill snapshots. Count the representation actually sent through the same request builders used for inference.

Find the project boundary at the nearest Git marker, falling back to session cwd. Load its instruction chain initially,
then use typed native-tool targets for lazy nested discovery. New guidance defers the tool batch with correlated results
so the model can reconsider its arguments; recheck between invocations when earlier operations change instructions.
Do not silently replay old arguments. Custom tool registration does not acquire filesystem semantics merely by name.

Discover project, configured and user skill roots deterministically, in that precedence order. Validate real YAML against
the standard. Skip malformed skills with ACP warnings, allowing valid lower-precedence fallbacks. Use existing read
operations for progressive activation and existing tools for supporting files. A cataloged read obtains one bounded
complete snapshot shared by display and activation, independently of the displayed line window.

Commit semantic activation state together with successful history under the existing current-turn token check. Keep
activated snapshots outside evictable messages. Inject discovery inputs explicitly for embedding and tests. Additional
roots use existing launch-time configuration layering and leaf-origin-relative resolution; this does not change ADR-006's
configuration traversal rules.

## Consequences

Standard skills work without conversion, extra tools or a plugin framework. Native protocol continuation and whole-turn
eviction remain intact. Recoverable discovery errors travel as bounded ACP notices without corrupting stdout.

Guidance barriers cost additional model requests and tool-call budget. Active snapshots consume non-evictable context
until the session ends. Strict metadata validation and bounded sources intentionally exclude malformed or oversized
files. Scope labels do not isolate model memory. Bash and custom operations without structured targets remain
model-coordinated, and filesystem checks retain the optimistic race limitations of native tools.

The detailed contract, bounds, precedence, symlink policy and examples are in [project instructions](../instructions.md).
