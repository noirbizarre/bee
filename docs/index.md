# bee

Bee is a lightweight, headless agent runtime written in Rust that implements ACPv2. It provides the core agentic loop
and LLM provider integration, allowing it to work with any compatible ACPv2 client.

## Status

Bee is at the **bootstrap stage**: the repository, tooling, CI and release automation exist, and the `bee` binary
serves ACPv2 over stdio, so a client can start it, complete the `initialize` handshake, create sessions and submit
prompts. Bee advertises the baseline session lifecycle (`session/new`, `list`, `resume`, `close`, `prompt`, `cancel`).

**No LLM is integrated yet.** Every prompt is answered with the same fixed placeholder message, which is temporary and
will be replaced by the agentic loop and a provider integration. A running prompt can be stopped with `session/cancel`
or `session/close`, and a prompt sent while one is running is refused. Because the placeholder turn finishes
immediately, this only shows once a real turn takes time. `session/resume` cannot replay history, and prompts accept
`text` and `resource_link` content only. There is no agentic loop either.

## Configuration

Bee reads TOML configuration from up to three files, merged in this order, lowest precedence first:

1. **Global:** `$XDG_CONFIG_HOME/bee/config.toml`, or `~/.config/bee/config.toml` when `XDG_CONFIG_HOME` is unset. The
   same on every platform, macOS included.
2. **`BEE_CONFIG`:** the file named by that environment variable, for settings chosen per client or workspace.
3. **Project:** `.config/bee.toml` in the directory Bee is launched from. Parent directories are not searched.

The global and project files are optional. A `BEE_CONFIG` file that does not exist is an error, and so is any file
that exists but cannot be read or parsed: Bee exits before speaking the protocol and says which file and, for syntax
errors, which line and column. Diagnostics never repeat the contents of a file, since they may hold API keys.

Tables merge key by key, and for a scalar the higher layer wins. An array in a higher layer **replaces** the lower one
instead of extending it. Giving one key different types in two layers (a string over an integer, say) is an error that
names the key and both files. See [ADR-006](adr/006-layered-xdg-configuration.md).

No setting is read yet: providers and models are not integrated, so the files are validated but have no effect.

## Scope

Bee is the agent side of an ACPv2 connection. It runs the agentic loop and talks to LLM providers; presentation and
orchestration belong to the client. The planned boundaries are:

- ACPv2 protocol integration
- agent execution and the agentic loop
- LLM provider integration and model configuration
- session execution and cancellation
- configuration and credential handling
- error handling and observability

## Non-goals

- Bee is an agent runtime, not an orchestration platform.
- Bee does not provide its own TUI, web interface or session-management frontend.
- Bee does not assume it will only be used with Hive or any particular ACPv2 client.
- MCP configuration and orchestration are not Bee-specific responsibilities. Bee should use the capabilities provided
  through the ACPv2 client-agent contract, where the protocol supports them.
- WebAssembly-based extensions are a possible future direction, not an initial requirement.
- Provider authentication, OAuth flows and model selection will be designed around the actual provider integrations
  and protocol capabilities. Bee does not assume ACPv2 provides a universal implementation of these features.

## Installation

Bee is not published to crates.io (the `bee` crate name is taken by an unrelated project). Download a binary for your
platform from the [latest release](https://github.com/noirbizarre/bee/releases/latest) once one is published.

## Usage

An ACPv2 client launches `bee` as a subprocess and speaks the protocol on its stdin and stdout. Stdout carries protocol
traffic only; diagnostics go to stderr. Closing stdin ends the session and Bee exits with status 0.

```bash
bee --help
```
