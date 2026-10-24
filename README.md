<h1 align="center">
  <img src="docs/images/logo.svg" alt="Bee">
</h1>

<p align="center"><strong>A minimal, headless Rust agent runtime implementing ACPv2.</strong></p>

<p align="center">
  <a href="https://github.com/noirbizarre/bee/actions/workflows/ci.yaml">
    <img src="https://github.com/noirbizarre/bee/actions/workflows/ci.yaml/badge.svg" alt="CI">
  </a>
  <a href="https://codecov.io/gh/noirbizarre/bee">
    <img src="https://codecov.io/gh/noirbizarre/bee/graph/badge.svg" alt="Codecov">
  </a>
  <img src="https://img.shields.io/github/v/release/noirbizarre/bee" alt="Release">
  <a href="https://noirbizarre.github.io/bee/">
    <img src="https://img.shields.io/badge/docs-noirbizarre.github.io-blue" alt="Documentation">
  </a>
  <img src="https://img.shields.io/github/license/noirbizarre/bee" alt="License">
</p>

---

Bee is a lightweight, headless agent runtime written in Rust that implements ACPv2. It provides the core agentic
loop and LLM provider integration, allowing it to work with any compatible ACPv2 client.

## Status

Bee is at the **bootstrap stage**. The repository, tooling, CI and release automation are in place, and the `bee`
binary serves ACPv2 over stdio: a client can start it, complete the `initialize` handshake, create sessions and submit
prompts. Bee advertises the baseline session lifecycle (`session/new`, `list`, `resume`, `close`, `prompt`, `cancel`).

**No LLM is integrated yet.** Every prompt is answered with the same fixed placeholder message, which is temporary and
will be replaced by the agentic loop and a provider integration. A running prompt can be stopped with `session/cancel`
or `session/close`, and a prompt sent while one is running is refused. Because the placeholder turn finishes
immediately, this only shows once a real turn takes time. `session/resume` cannot replay history, and prompts accept
`text` and `resource_link` content only. There is no agentic loop either.

The "Scope" and "Non-goals" sections describe the intended design, not current behaviour.

## Configuration

Bee merges up to three TOML files, lowest precedence first: the global
`$XDG_CONFIG_HOME/bee/config.toml` (or `~/.config/bee/config.toml`, on every platform), the file named by
`BEE_CONFIG`, and the project's `.config/bee.toml`. Tables merge key by key, higher layers win for scalars, and arrays
are replaced rather than extended. A malformed or unreadable file, or a missing `BEE_CONFIG` file, stops Bee at launch
with an actionable error that never repeats the file's contents. Provider profiles (`[providers.<name>]`: the protocols
the provider speaks, each with its base URL and options, plus model, credential reference and headers) are validated and
selectable per ACPv2 session, but no LLM is called yet. See the [documentation](https://noirbizarre.github.io/bee/#configuration).

## Authentication

A profile's `credential` names an environment variable (`{ env = "KEY" }`) or Bee's credential store
(`{ store = true }`, optionally with `login = "openrouter"` for a browser sign-in). `bee auth login`, `logout` and
`status` manage the store, which is a `0600` file under `$XDG_DATA_HOME/bee`. Providers are data:
`preset = "openrouter"` (also `openai`, `anthropic`, `opencode`, `deepseek`, `google`, `groq`, `mistral`, `xai`,
`ollama`) fills in a profile, and your own `presets` and `login_flows` tables extend or override the shipped ones.
OpenRouter's sign-in returns an API key; Cloudflare uses a scoped API token and not Wrangler's login; remote MCP server
authorization is a separate audience and is not implemented. No LLM is called yet, so stored credentials are not used
for requests. See the
[documentation](https://noirbizarre.github.io/bee/#authentication).

## Scope

Bee is meant to be the agent side of an ACPv2 connection: it runs the agentic loop and talks to LLM providers, and
leaves everything about presentation and orchestration to the client. The planned boundaries are:

- ACPv2 protocol integration
- agent execution and the agentic loop
- LLM provider integration and model configuration
- session execution and cancellation
- configuration and credential handling
- error handling and observability

Most of these do not exist as code yet: only the protocol integration, the session lifecycle with cancellation,
protocol error mapping, configuration and credential handling do. They are boundaries to grow into, not modules to
scaffold ahead of need.

## Non-goals

- Bee is an agent runtime, not an orchestration platform.
- Bee does not provide its own TUI, web interface or session-management frontend.
- Bee does not assume it will only be used with Hive or any particular ACPv2 client.
- MCP configuration and orchestration are not Bee-specific responsibilities. Bee should use the capabilities provided
  through the ACPv2 client-agent contract, where the protocol supports them.
- WebAssembly-based extensions are a possible future direction, not an initial requirement.
- Bee does not assume ACPv2 provides a universal implementation of provider authentication. Each browser sign-in is a
  described in data and limited to one mechanism, not a generic OAuth client.

## Installation

There is nothing useful to install yet. Bee is not published to crates.io (the `bee` crate name is taken by an
unrelated project); releases will be distributed as GitHub release binaries, a Homebrew formula and an AUR package.

```bash
brew install noirbizarre/homebrew-tap/bee
```

```bash
paru -S bee-bin  # or your AUR helper of choice
```

Or download a binary for your platform from the
[latest release](https://github.com/noirbizarre/bee/releases/latest).

The Homebrew and AUR packages only exist once the first release has been published.

## Usage

An ACPv2 client launches `bee` as a subprocess and speaks the protocol on its stdin and stdout. Stdout carries protocol
traffic only; diagnostics go to stderr. Closing stdin ends the session.

```bash
bee --help
bee --version
```

## Documentation

<https://noirbizarre.github.io/bee/>

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE](LICENSE).
