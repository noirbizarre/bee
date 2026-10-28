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

The target selection in `.config/git.tpl.toml`, Cargo binstall metadata and publication agree on seven targets:

| Rust target | Asset suffix |
| --- | --- |
| `x86_64-unknown-linux-gnu` | `linux-amd64` |
| `aarch64-unknown-linux-gnu` | `linux-arm64` |
| `x86_64-unknown-linux-musl` | `linux-amd64-musl` |
| `aarch64-unknown-linux-musl` | `linux-arm64-musl` |
| `x86_64-apple-darwin` | `darwin-amd64` |
| `aarch64-apple-darwin` | `darwin-arm64` |
| `x86_64-pc-windows-msvc` | `windows-amd64.exe` |

Each raw executable is named `bee_<tag>_<suffix>`; tags have no `v` prefix. `SHA256SUMS` covers all seven executables.
No signing or artifact-attestation step is configured. The Windows executable statically links the MSVC runtime.
Homebrew uses the two macOS assets and Linux x86-64 musl; AUR uses GNU x86-64/ARM64 assets.
The AUR package is `bee-agent`; its installed command is `bee`.

## CI builds and the development channel

CI's PR-only Build matrix compiles and links all seven targets using the non-release `ci` profile, which retains
line-table debug information without release optimization/LTO costs. It produces no download artifacts. Superseded PR
runs are cancelled. Main skips this matrix; CI's `prerelease` job calls `publish-release.yaml` with `ref: github.sha`.
Publication depends only on that workflow's release build matrix. It stages final asset names and uploads
seven-day artifacts, then one publisher generates checksums and uploads to either destination.
Both matrices use `matrix.build` objects with short platform labels. Compatibility/TCK remains a separate check.

`ref` distinguishes version-tag runs, such as `refs/tags/0.1.2`, from prerelease runs. gh-ship dispatches on the version
tag; main CI supplies its run SHA. Every checkout uses the recorded `github.sha`; no separate resolution job is needed.
Stable publication attaches assets to gh-ship's existing draft without editing its notes or visibility.

For an exceptional manual rebuild, select main in Run workflow and leave `ref` blank. It rebuilds the recorded run SHA
even if main advances during the build. Uploads and tag updates are serialized, and the tag update follows a successful
bulk upload.

The optional repository variable `PRERELEASE_NAME` changes the prerelease tag, name and filename channel, defaulting to
`development`. Use a single Git-valid filename/tag component without spaces, slashes or `#`.
The draft template also packages configured archives/man pages for both
channels through the same path; Bee's configuration remains raw binaries only.

`.github/ship.yml` explicitly selects `release.publish_input: ref` (gh-ship 0.6+) so stable releases are dispatched with
qualified tag refs. Ship uses `gh ship run` and renewable App authentication; release preparation stays build-free and
signs the prepared commit after updating the manifest/lockfile and changelog. PR CI validates that commit before merging.

These workflows come from rust.tpl. Bee uses its existing reduced-debug `ci` profile for PR builds instead of the
template's default debug profile.

The development channel is a prerelease explicitly excluded from Latest. Its rolling tag and release notes identify
the published source SHA. Publication views the deterministic prerelease name, creates it if needed, and uses one
`gh release upload "$TAG" dist/* --clobber` call for both channels. Tagged releases are already created by gh-ship.
Predictable raw binary names use `bee_development_<suffix>`. SHA256SUMS covers the binaries. No version bump, stable-release
preparation or crates.io publication runs for this channel. Homebrew/AUR automatically publish on stable `released`
events, excluding development prereleases; maintainers can explicitly rerun either workflow with a tag.

Channel asset replacement is not atomic. Consumers verify checksums, retry during updates and record the source SHA;
stable release URLs are the immutable option. Both channels use the release environment's GitHub App. After every upload
succeeds, a conditional step moves the rolling prerelease tag to the binaries' source SHA. Main CI gates automatic
publication; stable
dispatches continue uploading to gh-ship's existing draft before the orchestrator makes it public.

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

For each Linux target use `cross build --release --locked --target <target>`; for each macOS or Windows target use
`cargo build --release --locked --target <target>`, with `RUSTFLAGS="-D warnings"` as in Publish Release.
Use suitable runners/toolchains; a host build alone does not prove the entire matrix.
CI checks every selected target on pull requests with non-release builds; optimized release builds run in Publish Release.
For Windows, include `-C target-feature=+crt-static` in RUSTFLAGS, as both build matrices do.

On native-compatible runners, check the built executable's exact version, help and clean empty-stdin shutdown with
isolated HOME/XDG directories. Inspect GNU runtime requirements and musl linking rather than promising unmeasured
minimum OS/libc compatibility. The existing publish workflow compiles and uploads but does not smoke-test binaries.

Before public publication, verify the draft release has exactly the seven expected executable names plus `SHA256SUMS`.
Download all eight into an isolated directory and run `sha256sum -c SHA256SUMS`; verify the intended tag/version.
For development also verify that `refs/tags/development` matches the source SHA in release notes, along with prerelease
status and unchanged latest stable release.
After package publication, test actual Homebrew installation and `brew test`, and AUR installation/version, including
namcap dependency findings. A successful core release does not establish package-manager success.

## Inherited workflow follow-ups

These are rust.tpl follow-ups, not guarantees provided by this repository's current workflows:

- Gate PRs on generated documentation builds while keeping Pages deployment main-only.
- Pin the documentation application: locked uv does not pin unversioned `uvx zensical`.
- Add native-compatible binary smoke tests before publication.

Carry generic workflow changes upstream and integrate through git-tpl. Record unavailable runners, external settings
and unpublished asset/package checks as remaining gates; do not declare complete release readiness from static
configuration inspection alone.

## Recovering a failed release build

Publish Release checks out the requested tag. Rerunning a failed old tag after merging a source fix builds the same
old source. Use gh-ship's release procedure to release the corrected revision; do not move tags or bump versions by
hand. Verify the existing draft/tag state before recovery, and confirm `bee-agent` AUR ownership/key access before
package publication.
