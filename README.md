<p align="center">
  <img src="docs/images/icon.svg" alt="bee" width="160">
</p>

<h1 align="center">bee</h1>

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
binary serves ACPv2 over stdio: a client can start it and complete the `initialize` handshake. That is all it does.
Bee advertises no capabilities, and there are no sessions, no agentic loop and no LLM provider integration yet.

The "Scope" and "Non-goals" sections describe the intended design, not current behaviour.

## Scope

Bee is meant to be the agent side of an ACPv2 connection: it runs the agentic loop and talks to LLM providers, and
leaves everything about presentation and orchestration to the client. The planned boundaries are:

- ACPv2 protocol integration
- agent execution and the agentic loop
- LLM provider integration and model configuration
- session execution and cancellation
- configuration and credential handling
- error handling and observability

None of these exist as code yet. They are boundaries to grow into, not modules to scaffold ahead of need.

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
