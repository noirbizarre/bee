# Architecture Decisions

Records of the decisions that shape this project, and — more usefully — the
reasons behind them. An ADR is written when a choice is hard to reverse or
likely to be re-proposed.

The point is not the decision; it is the alternatives that were rejected and
why. A record that only states the outcome saves nobody the argument.

A decision is changed by writing a new ADR that supersedes the old one, never
by editing the old one. The history is the value.

## Format

`NNN-kebab-case-title.md`, numbered in the order written, with the sections:

- **Status** — Proposed, Accepted, or Superseded by ADR-NNN
- **Context** — the forces in play, before any decision
- **Decision** — what was decided
- **Consequences** — what this costs, including what it makes harder

## Index

- [ADR-001](001-acpv2-rust-sdk.md) — Build on the ACP Rust SDK's draft v2 API
- [ADR-002](002-mcp-over-acp.md) — Reach client-hosted tools through the SDK's MCP-over-ACP path
- [ADR-003](003-stdio-transport.md) — Serve ACPv2 over the SDK's stdio transport, behind a transport-agnostic seam
- [ADR-004](004-session-and-prompt-lifecycle.md) — Serve the baseline session lifecycle with a placeholder turn
- [ADR-005](005-prompt-cancellation-and-error-mapping.md) — Run turns off the dispatch loop so prompts can be cancelled
- [ADR-006](006-layered-xdg-configuration.md) — Layer configuration from the XDG directory, `BEE_CONFIG` and the project
- [ADR-007](007-provider-profiles-and-model-selection.md) — Describe providers as named profiles that sessions select
  among
- [ADR-008](008-provider-authentication.md) — Authenticate providers through credential sources, with one browser
  sign-in per provider that has one
- [ADR-009](009-provider-presets-and-login-flows.md) — Describe provider presets and browser sign-ins as data
- [ADR-010](010-several-protocols-per-provider.md) — Let a provider speak several protocols, ordered by `prefer`
- [ADR-011](011-openai-responses-protocol.md) — Add `openai-responses` as a protocol of its own
- [ADR-012](012-streaming-inference.md) — SDK-free inference with cancellable HTTP and caller-owned tools
- [ADR-013](013-per-model-protocol-selection.md) — Select protocols by exact model ID independently of catalogs
- [ADR-014](014-conversation-context-and-token-limits.md) — Budget explicit context windows and truncate whole turns
- [ADR-015](015-client-hosted-mcp-execution.md) — Execute client-hosted MCP tools through the ACP boundary
- [ADR-016](016-project-instructions-and-agent-skills.md) — Load scoped AGENTS.md instructions and standard Agent Skills
- [ADR-017](017-acp-permission-and-elicitation.md) — Keep Bash authorization, user input and browser login turn-owned
