# Outbound ACPv2 client

Bee's Rust library can launch and control configured ACP agents through
`bee::acp::client`. Each explicit `connect_with` call owns a local stdio child,
initializes draft **ACP wire version 2**, and provides a scoped connection for
session creation, sequential prompts, progress, cancellation and cleanup.

The implementation uses maintained `agent-client-protocol = 3.5.0` and schema
`1.12.0`. It requires the baseline session capability and reports unsupported
capabilities. The supported transport is the SDK's `AcpAgent` subprocess stdio
transport. Remote transports and protocol-v1 fallback are outside this API.

## Named agent configuration

Use the normal [layered configuration](index.md#configuration): global XDG file,
`BEE_CONFIG`, then project `.config/bee.toml`.

```toml
[agents.reviewer]
command = "compatible-agent"
args = ["--stdio"]
transport = "stdio"

[agents.reviewer.env]
AGENT_SETTING = "value"

[agents.second]
command = "/opt/team/bin/another-agent"
```

`command` is required. `args` and `env` default to empty; omitted `transport`
means `stdio`. Unknown fields, unsupported transports and invalid process strings
are errors. There is no first-agent default. Loading or validating definitions
never launches a process; selecting and connecting is an explicit host action.
Ordinary Bee startup validates definitions too.

Tables merge recursively and arrays replace. Overriding `command` retains inherited
arguments and environment entries; set `args = []` to clear arguments. An empty
environment table does **not** clear inherited keys. Project definitions follow the
same rules as global definitions. Definition debug output and diagnostics redact
executable settings, arguments, environment values and dynamic keys.

The executable and argument vector are passed literally: no implicit shell,
interpolation or tilde expansion. The SDK inherits the host's process cwd and
environment, then adds configured environment overrides. A session's absolute
`cwd` is a separate ACP field and does not change the child process cwd.

## Library usage

The host supplies a Tokio runtime with timers enabled:

```rust,no_run
use bee::acp::client::{self, ClientOptions, HostHandlers, config::Agents};
use tokio_util::sync::CancellationToken;

async fn review(config: &bee::config::Config) -> Result<(), Box<dyn std::error::Error>> {
    let agents = Agents::from_config(config)?;
    let cwd = std::env::current_dir()?;
    client::connect_with(
        agents.get("reviewer")?,
        HostHandlers::default(),
        ClientOptions::default(),
        CancellationToken::new(),
        move |connection| Box::pin(async move {
            connection.new_session(&cwd).await?;
            let mut prompt = connection.start_prompt(
                vec!["Review the current changes.".into()],
                CancellationToken::new(),
            )?;
            while let Some(update) = prompt.next_update().await? {
                // Render or store the structured update using the host's own presentation.
                let _ = update;
            }
            let result = prompt.finish().await?;
            let _ = (result.acceptance, result.idle, result.messages);
            Ok(())
        }),
    ).await?;
    Ok(())
}
```

The callback borrows its connection. One session and one foreground prompt are
live at a time; a completed session supports repeated prompts. Separate connection
calls can launch different named agents. `close_session` explicitly releases a
session; normal scope completion closes any remaining session.

Prompt acceptance acknowledges user-message insertion. Completion additionally
requires a post-submission `running` or `requires_action` transition followed by
`idle`, even when idle arrives before acceptance. Initial idle snapshots do not
complete a prompt. Session state describes aggregate foreground work; it does not
provide an invented per-turn identifier. The acceptance message ID is preserved.

Progress preserves typed content, tools, thoughts, terminals and plans. Results
reduce agent messages by message ID in first-seen order: chunks append, snapshots
replace content, omission preserves and null clears. Metadata follows the SDK's
top-level patch rules. Non-text content remains structured.

## Host interactions

`HostHandlers` accepts cancellation-safe async permission, form, URL and URL-completion
callbacks. Requests retain their full structured subjects, offered options, scopes,
tool-call correlation and metadata. Callbacks run outside receive dispatch with
bounded concurrency. Host UI and policy remain the embedding application's concern.

Permission handlers return a selected **offered option ID** or cancellation.
Allow/reject and once/always kinds retain their protocol meaning; Bee stores no
permission policy here. A missing handler fails closed with wire cancellation and
a distinct local unavailable diagnostic. Session cancellation resolves pending
permissions exactly once; late host responses cannot approve cancelled work.

Form and URL capabilities are negotiated independently. Forms preserve typed
accept/decline/cancel responses; accepted content is checked against the inline
form schema. URL support requires both URL and completion handlers. URL acceptance
is separate from later completion, correlated by elicitation ID.

Request-scoped elicitation works after initialization, including during session
creation. The SDK rejects elicitation before initialization completes. Unsupported
or unavailable modes receive a fixed protocol error and a distinct local diagnostic;
they are not reported as a user's decline. Host handlers should use their cancellation
token and avoid detached side effects.

## Bounds and shutdown

`ClientOptions` defaults:

| Policy | Default |
| --- | --- |
| Initialization, session setup and prompt acceptance | 30 seconds |
| Prompt foreground completion, measured from submission | 10 minutes |
| Host interaction | 5 minutes |
| Interaction cleanup, cancellation idle and session close | 2 seconds each |
| Queued progress | 256 updates, 4 MiB |
| Reduced result messages | 4 MiB |
| Concurrent interactions / pending URL IDs | 16 each |

Limits and deadlines must be positive and representable. Queue overflow fails the
operation and initiates cleanup rather than silently losing output. Bee-owned queue
and result bounds do not bound the SDK's internal channels or JSON line framing.

Cancel a prompt using its token or `Prompt::cancel`; cancel the full connection scope
using the connection token. Local cancellation remains distinct from remote stop
reasons. Dropping unfinished prompts requests session cancellation and prevents
reuse until session cleanup. Failed or timed-out close leaves the connection unusable.

Normal shutdown cancels interactions, cancels unfinished work, waits a bounded idle
grace, and closes the session before ending the SDK connection. Dropping the outer
future cannot run an asynchronous close handshake; SDK child guards provide termination
fallback. Unix cleanup terminates the process group; other platforms terminate the
immediate child. This is process ownership, not a sandbox for escaped descendants.
Child stderr is drained by the SDK and never copied into Bee's outbound diagnostics.

`tests/acp_client.rs` exercises this public API against an independent deterministic
stdio peer, including protocol ordering, interactions, failures, cancellation and
process cleanup.
