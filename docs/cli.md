# CLI and troubleshooting

## Serving and help

```bash
bee
bee --help
bee --version
bee auth --help
bee models --help
bee providers list
bee providers show github
bee --scenario success
```

Bare `bee` serves draft ACP wire-v2 on stdin/stdout. It has no terminal chat UI. Normal server stdout contains only
protocol traffic; diagnostics go to stderr. Closing stdin ends the connection successfully. `--scenario` selects a
[deterministic test agent](scenarios.md), bypassing configuration, durable storage and provider credentials; it cannot
be combined with human-facing subcommands.

`-h`/`--help` and `-V`/`--version` work before configuration is loaded. Every nested command accepts help. Global
`-v`/`--verbose` enables detailed diagnostics; repeating it does not currently add levels.

## Authentication

```bash
bee auth login [CONNECTION]
bee auth login [CONNECTION] --no-browser
bee auth login [CONNECTION] --paste-code
bee auth login [CONNECTION] --with-token
bee auth logout [CONNECTION]
bee auth status [CONNECTION]
```

Login/logout target the active profile's default connection when omitted; status lists its member connections.
Login resolves an existing instance first, or creates one when the name identifies a native provider type.
Explicit environment login imports its credential fields and changes that instance to stored authority in user configuration.
Higher-priority source overrides must be removed first. Browser/device flows depend on the provider's login mechanism;
`--paste-code` is not available for every flow. `--with-token` reads a token from stdin, not command arguments.
The three flags `--with-token`, `--paste-code` and `--no-browser` are mutually exclusive.

Store operations are currently Unix-only. ChatGPT-plan login requires its OAuth flow, not a pasted token.
Environment-backed credentials remain usable without login. See [providers](providers.md) for fields and migration.
Status reports local availability, not a live entitlement check; missing credentials can be reported with a successful
exit status. Status output goes to stdout; login prompts and URLs go to stderr.

OpenCode Zen and Go use console API keys, with no documented external OAuth/device flow:
`bee auth login opencode --with-token` and `bee auth login opencode-go --with-token` create independent store-backed
instances on first use. Plain login imports their declared environment key; `--with-token` explicitly reads a new key.
Select a text model after login; offering lists are not entitlement checks or ranked defaults.
See [OpenCode setup](index.md#use-opencode-zen-or-go) for billing/subscription requirements, environment configuration,
key rotation and logout semantics.

Google Cloud uses externally managed ADC. `bee auth status google-cloud` reports that mode without reading or refreshing
tokens. Use `gcloud auth application-default login` for local user ADC; `bee auth login/logout google-cloud` return setup
guidance and never modify shared ADC. Service-account files and attached accounts also work; see
[Google Cloud setup](index.md#use-google-cloud) for source precedence and supported credential types.

## Model discovery

`bee providers list` and `bee providers show TYPE` inspect compiled-in integrations without loading configuration or
credentials. The `bee models providers` command below browses Models.dev, which is a separate public model catalogue.

```bash
bee models providers [--json]
bee models list [CONNECTION] [--endpoint] [--json]
bee models list --catalog-provider PROVIDER [--json]
bee models refresh [CONNECTION] [--json]
```

Connection arguments refer to the active profile. `--catalog-provider` browses Models.dev instead of a connection and
conflicts with a connection argument or `--endpoint`. Manual model IDs remain usable without catalog discovery.
Refresh can emit a structured failure result before exiting unsuccessfully. See [model discovery](models.md).

## Configuration environment

Bee has no `--config`, `--provider` or `--model` CLI options. Use TOML and ACP session selection.
`BEE_CONFIG` selects an extra file, `BEE_PROFILE` supplies a profile selector, and XDG roots control storage locations.
See [configuration precedence and paths](index.md#configuration), including the higher-priority project selector.
Credential variable names are chosen by each connection. Pass variables to the Bee subprocess when launching a client.

## Troubleshooting

| Symptom | Check and remedy |
| --- | --- |
| Executable not found | Install Bee and use its absolute path if the client does not inherit your shell PATH. |
| Bee waits in a terminal | It expects ACP protocol input. Launch it through a compatible wire-v2 client. |
| Configuration failure | Read stderr for the path, key or syntax position; repair the file or unset an unintended `BEE_CONFIG`. Values are deliberately redacted. |
| Missing credentials | Check the selected instance and subprocess environment; explicitly log in to import a key or run its native flow. |
| Login configuration publication failed | Credentials may have been stored; review user configuration and higher-layer overrides, then repeat login. |
| Wrong model/account | Check the active profile, remembered model and explicit ACP selection; see [profile resolution](profiles.md#configured-default-and-last-used-model). |
| Tool permission request stalls | Ensure the client handles permission interactions; see [tools](tools.md). |
| Inference fails after startup | Inspect the ACP terminal result and client diagnostics; verify endpoint, model access and credential source. See [inference](inference.md). |
| Cannot open durable state | Provide a writable private state root through absolute `XDG_STATE_HOME`, or repair existing storage permissions. |

Parser errors exit with status **2**. Startup/application errors exit with status **1** and diagnostics on stderr;
failed serving startup leaves stdout empty. Clean EOF exits **0**. A failed or cancelled accepted ACP turn is a session
outcome, not necessarily a process failure. Use `--verbose` for detailed diagnostics when reporting a problem.
