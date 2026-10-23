# AGENTS.md

Notes for anyone — human or otherwise — changing this repository.

## What this project is

A minimal, headless Rust agent runtime implementing ACPv2.

Bee is the agent side of an ACPv2 connection: the agentic loop and LLM provider
integration, usable with any ACPv2 client. It is **not** an orchestration
platform, has no UI of its own, and is not tied to Hive or any one client. MCP
configuration and orchestration are the client's concern, reached through the
ACPv2 client-agent contract. Do not propose a TUI, a web frontend, a plugin
framework or a provider abstraction ahead of a concrete need.

The repository is at the bootstrap stage: Bee serves ACPv2 over stdio and
implements the baseline session lifecycle (new, list, resume, close, prompt,
cancel), answering every prompt with a fixed placeholder. A running prompt
can be cancelled and a failed turn is reported, but the placeholder turn never takes time.
The agentic loop and provider integrations are planned, not implemented.
Documentation must keep saying that the placeholder reply is temporary and that
no LLM is integrated, until that changes.

## Non-negotiable invariants

Each of these should be enforced by a hook or a test. An invariant nothing
checks is a comment, and it will be violated.

1. **Stdout carries ACPv2 protocol traffic only.** A stray line from a log or a
   debug print corrupts the client's JSON-RPC stream, so diagnostics go to
   stderr and nothing in the library prints to stdout — enforced by
   `tests/stdio.rs` (`stdout_carries_only_json_rpc_messages`) and
   `tests/cli.rs`.
2. **Not published to crates.io.** The `bee` crate name belongs to an unrelated
   project, so `publish = false` stays in `.config/git.tpl.toml` — enforced by
    the template, which renders it into `Cargo.toml`.
3. **Configuration values never reach a diagnostic or `Debug` output.** Files
   are where users keep API keys, so errors carry paths, positions and types,
   never contents — enforced by `tests/config.rs` and `tests/cli.rs`.

## Layout

```text
src/
├── lib.rs      the library surface
├── main.rs     the bee binary
├── acp.rs      the ACPv2 boundary: the only module that imports the SDK;
│               the agent core, and `serve` over any transport
├── session.rs  session state, free of the SDK
├── config.rs   layered XDG configuration (ADR-006), free of the SDK
├── cli.rs      argument types only
└── error.rs    the crate's error type
tests/
├── cli.rs      the binary's observable contract
├── stdio.rs    the stdio transport, through the real binary
├── acp.rs      the agent core, in-process
├── session.rs  the session and prompt lifecycle, in-process
├── cancel.rs   cancellation, failed turns and error mapping, in-process
├── config.rs   the configuration layers, merge rules and errors, in-process
└── mcp.rs      the MCP-over-ACP spike (ADR-002)
```

New modules for the agent loop and providers are added when the code that needs them is, not before.

Dependencies point inward. Nothing in the library knows a command exists.

## Style

**Every non-obvious line carries a comment saying why.** Not what — the code
says what. Ideally naming the failure it prevents. A comment that restates the
code is worse than none.

**Errors are typed and actionable.** `thiserror` for the library, `miette` at
the binary edge. A diagnostic must carry the two things the user does not
already know: what specifically failed, and what to do about it. Diagnostic
codes are `bee::<module>::<kind>`, and a code is a public identifier
users grep for — renaming one is a breaking change.

**Test names are sentences.** `an_unchanged_input_produces_no_output`, not
`test_run_2`. The name should say what would be broken if it failed.

## Commits

Conventional Commits, enforced by commitlint on `commit-msg`. The type becomes a
changelog heading, so choose it as if someone will read it in release notes —
because they will.

## Releases

Driven by gh-ship. Never bump a version or push a tag by hand: `cliff.toml`
derives the version from the commit history, `prepare-release` applies it, and
`.github/ship.yml` is the contract between them. See CONTRIBUTING.md.

## Before you push

```sh
mise run ci
```

Formatting, Clippy, spelling, workflow and Markdown linting, tests and the documentation build. Same as CI.

## This repository is generated from a template

The toolchain, hooks, CI and release workflows come from
[rust.tpl](https://github.com/noirbizarre/rust.tpl) and are updated with
`git tpl update`. Files carrying template-owned content end with a
`# --- project-specific ---` marker: add below it, never above.

Changing template-owned content here fixes it in one repository. Changing it in
the template fixes it in all of them — prefer that.
