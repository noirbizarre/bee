# ADR-014 — Explicit context windows and whole-turn truncation

## Status

Accepted. Extends ADR-012's transactional in-memory context with optional token budgeting.

## Context

The explicit-only capacity policy is extended by [ADR-018](018-optional-model-discovery.md) for opted-in catalog windows.

Successful turns already retain normalized user messages, assistant steps, tool pairs and native continuation.
Unbounded accumulation eventually exceeds a model's capacity. Models are opaque IDs, providers may be private
gateways, and protocol selection can change with the model; a guessed global capacity would be misleading.
Provider usage measures work performed, including repeatedly supplied history, rather than retained context size.
Dropping individual messages can strand a tool result or lose signed/encrypted continuation required by a provider.

## Decision

Profiles and presets accept optional positive 32-bit `context_window` and `model_context_windows` settings. Exact model
entries win, and maps merge per key across layers and presets. Without an applicable explicit capacity, context is
estimated but not truncated. No provider catalog, tokenizer package or extra token-counting HTTP request is required.

Estimate the actual selected protocol's compact JSON conversation and tools with rounded-up UTF-8 bytes divided by
three plus a 256-token framing allowance. Native replay replaces portable fields in this accounting. Before every
request, reserve its effective maximum output, including protocol overrides. This heuristic is deterministic and
deliberately labeled approximate; it cannot guarantee that a provider tokenizer agrees.

The agent loop evicts oldest complete user turns until the request fits, keeping the entire active turn. Every
assistant step, correlated tool result and opaque native item belonging to a removed turn goes together. Invalid
removable call/result batches fail rather than being silently repaired. A protected turn that cannot fit fails
explicitly. Direct `Model::step` calls validate a budget but leave the caller's messages untouched.

Eviction remains transactional with inference: only a successful, non-refused current turn commits the reduced history.
Failures, cancellation and refusals leave prior committed context intact, including close/resume. Output-token-limited
answers continue to be retained. No separate full transcript, summary, disk memory or ACP replay is introduced.

Provider overflow has the typed `bee::llm::context_limit` diagnostic. Inspect bounded error bodies privately, recognize
explicit codes and narrowly identified Anthropic context failures, and retain safe fallback classifications for
unknown errors. Anthropic's context-window-exhaustion stop is a context failure, distinct from output exhaustion.
Do not retry automatically: streamed text, consumed tokens and tool effects cannot safely be replayed implicitly.

After successful commit, ACP reports estimated context occupancy only when capacity is configured, labels provenance
in `bee.dev/context` metadata, and sends an informational notice for removed turns. Sends share the terminal/current
token lock. Provider usage totals stay separate in the library outcome, with per-counter reporting-step counts so
partial known totals are identifiable. Failed attempts do not yield a usage ledger.

## Consequences

Whole-turn truncation is coarse and can discard useful old interaction; it preserves recent interaction without
breaking tool pairs or opaque continuation. Protecting the active turn means long tool loops can fail even after old
history is removed. Side effects already executed cannot be rolled back when a tool result exceeds the next budget.

Capacity requires configuration appropriate to selected models. Estimates can undercount or overcount real tokens,
especially for encrypted reasoning, source code and multilingual text, so provider rejection remains possible and
explicit. Native request-building helpers are shared with estimation to prevent serialization drift. Local fixture
tests exercise all protocols, budget boundaries, tool growth, transactional rollback, safe errors and real stdio reports.

Summarization and model-catalog/tokenizer integrations would add dependencies and semantic decisions unsupported by
this initial need. They can be considered later without changing the whole-turn transaction boundary.
