# ADR-025: Google Cloud Gemini inference with renewable ADC

## Status

Accepted.

## Context

Google AI Studio's Developer API keys do not authenticate managed Google Cloud inference or provide Cloud billing.
Issue #103 requires a distinct Google Cloud provider, explicit project/location settings and supported Cloud credentials.
Bee already preserves Gemini thought signatures in its Chat Completions adapter. The current official Rust auth library
requires Rust 1.91, above Bee's 1.88 MSRV, and brings additional HTTP/credential execution behavior into the runtime.

## Decision

Ship `google-cloud`, displayed as Google Cloud, using the official Vertex AI OpenAI-compatible API and exact `google/...`
Gemini model IDs. Construct global/regional targets from typed resource identifiers. Require ADC and reject arbitrary
URLs, custom headers, incompatible protocols and unverified generic model discovery.

Implement bounded authorized-user refresh, service-account JWT OAuth exchange and attached-account metadata flows using
existing HTTP and JWT components. ADC precedence is explicit file, local gcloud ADC, then attached account; an invalid
selected file never falls through. Unsupported federation/impersonation types fail actionably. No subprocess credential
execution or Bee-owned copy of shared ADC is introduced. Token requests have fixed destinations, bounded responses and
cancellable asynchronous I/O; metadata requests bypass proxies and require Google's metadata response header.

Turn-pinned sessions cache and renew tokens independently of account identity. File authority is privately fingerprinted;
attached-account identity is resolved via metadata. Resource target and quota-project scope participate in model-recency
bindings. Synchronous selectors defer metadata-dependent identity validation to asynchronous prompt preparation; no
inference may run until the remembered binding matches. The explicit quota-project override precedes ADC's quota project.

Reuse signed Gemini Chat continuation and existing usage snapshots. Classify recognized structured Google errors with
fixed advice, keeping upstream messages private. Do not replay billable inference or change project, region or identity
automatically. Explicit model configuration satisfies availability selection without claiming catalogue or quota knowledge.

## Consequences

The integration preserves existing tool/context/history behavior and the MSRV without adding a Cloud inference SDK.
Bee owns a small supported subset of ADC and must maintain its tests against Google's documented OAuth/metadata contract.
Federation, impersonation, native Gemini generation and Cloud discovery require separate concrete work. A native Gemini
adapter was rejected for this change because it would duplicate working compatibility inference and expand continuation
and context-accounting scope. AI Studio remains an independent provider with its existing keys and discovery semantics.

Users must enable the Vertex AI API, configure an eligible billing project and grant resource/quota permissions. Cloud
credits remain subject to Google's terms and are separate from AI Studio free-tier and application/subscription quotas.
