# ADR-006: Layer configuration from the XDG directory, `BEE_CONFIG` and the project

## Status

Accepted.

## Context

Bee needs settings with three scopes: defaults for the user, a choice made by
whoever launches it (per client or workspace), and overrides committed with a
project. Provider and model selection ([#7](https://github.com/noirbizarre/bee/issues/7))
and project-local MCP servers ([#19](https://github.com/noirbizarre/bee/issues/19))
will read them, but neither exists yet.

- Configuration files are where users put API keys. Anything that echoes a file
  into a log or a diagnostic leaks them.
- Stdout carries protocol traffic only, so a configuration problem must surface
  on stderr, and early.
- macOS has its own convention (`~/Library/Application Support`), which the
  project does not want: one dotfile repository should work on every platform.

## Decision

### Locations, lowest precedence first

| Layer   | File                                                          | If missing |
| ------- | ------------------------------------------------------------- | ---------- |
| Global  | `$XDG_CONFIG_HOME/bee/config.toml`, else `~/.config/bee/config.toml` | skipped    |
| Chosen  | the file named by `BEE_CONFIG`                                | **error**  |
| Project | `<project>/.config/bee.toml`                                  | skipped    |

- The XDG rule is the same on every platform, macOS included. An empty or
  relative `XDG_CONFIG_HOME` is ignored, as the specification requires. With
  neither `XDG_CONFIG_HOME` nor `HOME` there is no global layer, which is not an
  error. A directory crate is not used: the rule is five lines and a crate would
  bring the platform conventions this decision rejects.
- An empty `BEE_CONFIG` counts as unset. A relative one resolves against the
  working directory.
- The project file is looked up in exactly the project directory, never in a
  parent, so the effective configuration cannot depend on directories outside
  the project.
- There is one global file. Drop-in directories can be added later without
  breaking it.

### Merge semantics

| Value                               | Result                                              |
| ----------------------------------- | --------------------------------------------------- |
| Table in both layers                | merged key by key, recursively                      |
| Scalar in both layers, same type    | the higher layer wins                               |
| Array in both layers                | the higher layer's array **replaces** the lower one |
| Key in one layer only               | kept                                                |
| Different types for one key         | **error** naming the key, both files and both types |

Arrays are replaced, not concatenated, so a project can drop an entry the
global file lists, and the result never depends on hidden accumulation. A type
change is an error rather than a silent override because it is almost always a
mistake (`retries = "3"` over `retries = 3`) that would otherwise surface far
from its cause. Integers and floats are different types.

ADR-017's #78 extension retains source-layer permission arrays separately from
the generic merged table: trusted `[[permissions]]` arrays concatenate, while
project rules can only tighten their evaluated result. This explicit consumer
does not change generic array replacement for other settings.

### Errors

| Code                            | When                                                          |
| ------------------------------- | ------------------------------------------------------------- |
| `bee::config::read`             | a file exists but cannot be read (permissions, directory, not UTF-8) |
| `bee::config::selected_missing` | `BEE_CONFIG` names a file that does not exist                 |
| `bee::config::parse`            | a file is not valid TOML; reports path, line and column      |
| `bee::config::conflict`         | two layers give one key different types                       |

A file that exists but cannot be read is an error even when optional:
skipping it would run Bee with settings the user believes are in force.

### Secrets

The TOML parser's own error text quotes the offending source line. Bee keeps
only the parser's message and a position, so a mistyped `api_key = sk-...` is
reported by location and never repeated. Conflicts name types, not values.
`Config`'s `Debug` lists keys and source files, never values. `tests/config.rs`
pins all three.

### Shape

`bee::config` exposes `Environment` (the variables that matter, passed as
values so tests never mutate the process environment) and `Config::load`,
which returns a merged `toml::Table` plus the file each key came from. There is
no typed schema: the code that gives a setting meaning will deserialize from
the table.

`bee::run()` loads the configuration once, from the working directory, before
any protocol traffic, so a bad file fails the launch with a diagnostic on
stderr. Nothing reads a setting yet.

## Alternatives rejected

- **A directory crate (`dirs`, `etcetera`).** Follows the platform convention on
  macOS, which is what was ruled out.
- **Concatenating arrays.** A project could never remove a global entry.
- **Silently preferring the higher layer on a type change.** Hides typos.
- **Searching parent directories for the project file.** Non-deterministic.
- **A typed schema now.** It would invent settings before the code that needs
  them.
- **Loading per session from the session's `cwd`.** More correct for a client
  that serves several projects through one process, but it reaches into the
  session layer with no consumer to justify it.

## Consequences

- The project layer is the launch directory's, not each session's. A client
  serving several projects from one Bee process will get the first project's
  file until a consumer needs per-session loading; `Config::load` already takes
  the directory, so that change is local.
- A bad configuration file stops Bee from starting, even though no setting is
  read yet. That is deliberate: it fails where the user is looking.
- A key containing a dot cannot be addressed through `Config::get`; read it from
  the table.
- No setting is validated beyond being TOML. Unknown keys are kept, not
  rejected.
