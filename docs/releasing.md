# Release readiness

Bee releases through gh-ship, not hand-written version bumps or tags. See
[CONTRIBUTING](https://github.com/noirbizarre/bee/blob/main/CONTRIBUTING.md#releases) and `.github/ship.yml` for the contract.

## Publication flow

1. A push to main runs Ship and prepares or updates the release PR, using git-cliff for the version and changelog.
2. Review and squash-merge that PR only after verification. Ship tags the merge and creates a draft release.
3. Publish Release builds the tag with locked dependencies and warnings denied, uploads binaries and `SHA256SUMS`,
   then gh-ship makes the release public.
4. Homebrew/AUR publication follows the public release. Their availability is a separate verification step.

Do not dispatch Publish Release as a build-only test: it uploads to a release. The crate remains `publish = false`.
The crate name on crates.io belongs to an unrelated project.

## Configured artifacts

The template answers in `.config/git.tpl.toml`, Cargo binstall metadata and Publish Release agree on six targets:

| Rust target | Asset suffix |
| --- | --- |
| `x86_64-unknown-linux-gnu` | `linux-amd64` |
| `aarch64-unknown-linux-gnu` | `linux-arm64` |
| `x86_64-unknown-linux-musl` | `linux-amd64-musl` |
| `aarch64-unknown-linux-musl` | `linux-arm64-musl` |
| `x86_64-apple-darwin` | `darwin-amd64` |
| `aarch64-apple-darwin` | `darwin-arm64` |

Each raw executable is named `bee_<tag>_<suffix>`; tags have no `v` prefix. `SHA256SUMS` covers all six executables.
No signing or artifact-attestation step is configured. Windows host tests do not imply a Windows release asset.
Homebrew uses the two macOS assets and Linux x86-64 musl; AUR uses GNU x86-64/ARM64 assets.
The AUR package is `bee-agent`; its installed command is `bee`.

## Before merging a release PR

Run from a configured development environment:

```bash
mise run ci
UV_PYTHON=3.14 mise run test:compatibility
gh ship validate
git cliff --bumped-version
git cliff --unreleased
```

Inspect the current PR checks, including all host test jobs, MSRV, compatibility and Ship setup. Local `mise run ci`
includes a documentation build; the inherited CI workflow currently does not gate PRs on that build. Compatibility
is a separate task/workflow. Exact green-check evidence matters more than a historical result in documentation.

Check the repository squash settings against `.github/ship.yml`: title **`PR_TITLE`**, message **`BLANK`**.
At the initial #14 audit they were `COMMIT_OR_PR_TITLE` and `COMMIT_MESSAGES`; a maintainer must reconcile this before
release. Inspect the current values rather than treating this historical observation as live status:

```bash
gh api repos/noirbizarre/bee --jq '{squash_merge_commit_title,squash_merge_commit_message}'
```

Verify the release environment's GitHub App, tap token scope, AUR ownership/key and required checks. Configured secret
names alone do not prove working permissions. Do not print secret values.

## Platform and artifact verification

For each Linux target use `cross build --release --locked --target <target>`; for each macOS target use
`cargo build --release --locked --target <target>`, with `RUSTFLAGS="-D warnings"` as in Publish Release.
Use suitable runners/toolchains; a host build alone does not prove the entire matrix.
The Linux Release Builds workflow checks all four Linux targets on pull requests using publication's cross setup.
It builds without publishing; macOS release builds still require separate verification.

On native-compatible runners, check the built executable's exact version, help and clean empty-stdin shutdown with
isolated HOME/XDG directories. Inspect GNU runtime requirements and musl linking rather than promising unmeasured
minimum OS/libc compatibility. The existing publish workflow compiles and uploads but does not smoke-test binaries.

Before public publication, verify the draft release has exactly the six expected executable names plus `SHA256SUMS`.
Download all seven into an isolated directory and run `sha256sum -c SHA256SUMS`; verify the intended tag/version.
After package publication, test actual Homebrew installation and `brew test`, and AUR installation/version, including
namcap dependency findings. A successful core release does not establish package-manager success.

## Inherited workflow follow-ups

These are rust.tpl follow-ups, not guarantees provided by this repository's current workflows:

- Gate PRs on generated documentation builds while keeping Pages deployment main-only.
- Pin the documentation application: locked uv does not pin unversioned `uvx zensical`.
- Extend build-only release validation to macOS and add native-compatible binary smoke tests before publication.

Carry generic workflow changes upstream and integrate through git-tpl. Record unavailable runners, external settings
and unpublished asset/package checks as remaining gates; do not declare complete release readiness from static
configuration inspection alone.

## Recovering a failed release build

Publish Release checks out the requested tag. Rerunning a failed old tag after merging a source fix builds the same
old source. Use gh-ship's release procedure to release the corrected revision; do not move tags or bump versions by
hand. Verify the existing draft/tag state before recovery, and confirm `bee-agent` AUR ownership/key access before
package publication.
