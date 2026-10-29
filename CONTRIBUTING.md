# Contributing to bee

## Setup

Development tools are managed by [mise](https://mise.jdx.dev/), with resolved
versions in `mise.lock`. Rust selects the moving stable channel separately;
the documentation application currently runs through unversioned `uvx zensical`.

```bash
mise install          # the tools
prek install          # the Git hooks
mise run ci           # local checks, tests and documentation build
```

Rustup provides the stable channel declared in `rust-toolchain.toml`. The
project-specific mise configuration also selects that channel to enable native
[Mr Boxington](https://mr-boxington.jdx.dev/setup) integration (mise 2026.9.2+).
Explicit `RUSTUP_TOOLCHAIN` selections take precedence; in GitHub Actions the
configuration preserves the rustup default selected by `dtolnay/rust-toolchain`.
The mise install request stays stable so locked installs remain valid.

### Build caching

```bash
mise exec -- cargo build --locked
mise exec -- cargo nextest run
mise exec -- mbx doctor
```

**Mise tasks have mbx built in:** `mise build`, `mise test`, and their explicit
forms `mise run build` and `mise run test` already route Cargo through mbx; no
extra `mbx` prefix is needed. The explicit commands above also work in a coding
agent's non-interactive environment. Plain `cargo` in a terminal needs effective mise
activation or shims; calling `~/.cargo/bin/cargo` directly bypasses wrapping.
Installing mbx alone does not enable wrapping. Doctor can warn about a missing
standalone Cargo shim with native integration; actual build summaries establish
activation. No standalone setup or editor configuration is required.

If personal environment paths put rustup's Cargo before mise's wrapper inside
task shells, use `mise exec -- cargo ...` directly or enable
`[settings] activate_aggressive = true` in ignored `mise.local.toml`. The local
trial uses that checkout-only setting; global shell configuration is unchanged.

For the local trial, an uncommitted `.cargo/config.toml` clears an inherited
compiler wrapper so mbx can cache; it leaves global Cargo settings, build jobs
and linker flags intact. Other checkouts can opt into the same project-only
`[build] rustc-wrapper = ""` override after inspecting their existing wrapper.
An environment-selected `RUSTC_WRAPPER` still wins and prevents mbx caching.

CI uses `jdx/mr-boxington-action` instead of sccache and rust-cache. Its default
GitHub `target` payload includes Cargo outputs and registry/git downloads, so
an additional rust-cache action is unnecessary. Successful main pushes populate
the cache; same-repository pull requests also save in their own PR scope so
later runs can reuse the trial cache. Fork pull requests only restore.
Cross-build jobs retain their
container-produced target outputs through that payload, while native builds
also use mbx compiler caching. Publication jobs use `backend: local`, without
restoring or saving shared compiler outputs. No separate remote cache service
or local-to-CI cache connection is configured.

Local compiler actions, eligible links, build-script executions and C/C++
compilations are cached by default. Compilation scheduling, in-flight
deduplication and learned private incremental reuse are also enabled. This
project additionally enables `[scheduler] tests = true` in `.mbx.toml` to
coordinate concurrent nextest tests and Cargo test binaries; test outcomes are
not cached. Queueing counts toward nextest's slow-test timeouts.

For this trial, mise disables automatic garbage collection and managed target
placement (`MBX_GC_AUTO=0`, `MBX_TARGET_VIEWS=0`) to preserve existing caches and
outputs. If the system temporary directory exceeds its quota, use a short,
local temporary path, for example `TMPDIR="$HOME/.cache/mbx" mise exec -- cargo
build --locked` after the first doctor run has created that cache directory.

To measure reuse, build the same source/toolchain/profile/features into two
fresh temporary target directories with `--target-dir`, using the commands
above and `MBX_SUMMARY=full`. Check for positive mbx hits on the second build;
Cargo skipping fresh outputs does not count. Diagnose misses with
`mise exec -- mbx explain --last`, then remove only those temporary targets.

To undo, remove or disable `mr_boxington` in the project Rust entry first and
run `mise reshim`. Then remove the mbx tool and its lock entry, the introduced
Cargo wrapper override, `.mbx.toml`, and the trial environment settings. Remove
the local `activate_aggressive` override if you added it. Remove
the newly added mise Rust entry if returning to rustup-only ownership. Preserve
existing Rust pins/options and unrelated lock entries; no cache deletion is
needed.

## Tasks

| Command | What it does |
|---|---|
| `mise run build` | Build the project |
| `mise run cli` | Run `bee` from source (passes flags through) |
| `mise run test` | Run the tests (accepts nextest selectors) |
| `mise run cover` | Run the tests with coverage |
| `mise run format` | Format |
| `mise run lint` | Clippy, warnings denied |
| `mise run lint:actions` | actionlint over the workflows |
| `mise run lint:md` | markdownlint over AGENTS.md, CONTRIBUTING.md, README.md and docs/ |
| `mise run spell` | typos |
| `mise run snapshots` | Review pending insta snapshots |
| `mise run check` | Everything that does not modify the working tree |
| `mise run ci` | `check` plus the documentation build |
| `mise run docs` | Serve the documentation locally |
| `mise run changelog` | Preview the changelog for unreleased commits |
| `mise run release` | Show the version the next release would take |
| `mise run tpl:update` | Bring the rendered template ref up to date |
| `mise run tpl:diff` | Show what merging the template would change |

`mise <task>` is a shorthand for `mise run <task>`, but a builtin subcommand of
the same name wins it silently — which is why the format task is `format` and
not `fmt` (`mise fmt` formats `mise.toml`), and why running the binary is
`mise cli` and not `mise run` (`mise run` runs a task). Prefer the explicit
`mise run <task>` in scripts: mise can claim a new name in any release.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/), enforced by
commitlint on `commit-msg`. The type selects the changelog section, and a `!`
or a `BREAKING CHANGE:` footer drives the version bump — so the message is part
of the release, not paperwork around it.

The official typos hooks check source files on `pre-commit` and commit messages
on `commit-msg`. Message spelling is checked without rewriting the message, so
fix any reported errors before retrying the commit.

## Releases

Releases are run by [gh-ship](https://github.com/noirbizarre/gh-ship).
**Never bump a version or push a tag by hand.**

1. A push to `main` triggers 🚢 Ship, which runs `gh ship run` and selects preparation.
2. `prepare` dispatches 🚀 Prepare Release, which asks git-cliff for the next
   version, writes `CHANGELOG.md`, bumps `Cargo.toml`, commits, and uploads a
   `ship.release.json` artifact describing what would ship.
3. gh-ship opens (or updates) the Release PR from `release/next`. Review it.
4. Merging it triggers 🚢 Ship again, where `gh ship run` selects publication: it tags the
   merge commit, creates a draft release, dispatches 📦 Publish Release to
   attach the binaries, then makes the
   release public.

Nothing to release is the normal case for step 1, and costs one workflow run
reporting `changed: false`.

`gh ship validate` runs on every pull request, so a broken release contract
fails on the PR rather than mid-release.

See the [release-readiness runbook](docs/releasing.md) for platform and asset checks,
verification coverage, and external release gates.

### Repository requirements

The release jobs authenticate as a GitHub App, not with `GITHUB_TOKEN` — the
default token cannot trigger workflows, so a Release PR it authored would show
no CI results. That means the repository needs:

- a `release` environment holding the variable `APP_CLIENT_ID` and the secret
  `APP_PRIVATE_KEY`.
- squash-merge settings of `squash_merge_commit_title: PR_TITLE` and
  `squash_merge_commit_message: BLANK`, so the squash commit subject is the
  Conventional Commit title from `.github/ship.yml`.
- a `homebrew` environment holding the secret `TAP_TOKEN`, scoped to push to
  `noirbizarre/homebrew-tap` only.
- an `aur` environment holding the secret `AUR_SSH_PRIVATE_KEY`, for the AUR
  account that owns the `bee-agent` package.

The development prerelease caller uses `secrets: inherit` so the reusable
publisher can access `APP_PRIVATE_KEY`. GitHub requires secret forwarding
across `workflow_call` even when the secret exists only in the publisher's
`release` environment.

## This repository is generated from a template

The toolchain, hooks, CI and release workflows come from
[rust.tpl](https://github.com/noirbizarre/rust.tpl):

```bash
git tpl status         # is there a template update pending?
mise run tpl:update    # advance refs/tpl/<id> — HEAD, index and worktree untouched
mise run tpl:diff      # read what merging it would change
git tpl merge          # take it
```

`tpl:update` is safe to run at any time: it only advances the rendered ref.
Nothing reaches your branch until the merge.

git-tpl is declared in `mise.toml`'s `[tools]`; `mise install` installs it using
its prebuilt release metadata through cargo-binstall. A separate manual source
installation is not required.

Files carrying template-owned content — `mise.toml`, `prek.toml`, `Cargo.toml` —
end with a `# --- project-specific ---` marker. Add below it; Git's 3-way merge
then preserves your additions across updates. A fix that belongs to every
project belongs in the template, not here.
