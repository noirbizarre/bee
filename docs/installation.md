# Installation

Bee is not published to crates.io: `bee` there is an unrelated project. Release binaries and package-manager installs
become available after the first release is published. Until then, build from source.

## Release binaries

Choose a raw executable from [GitHub releases](https://github.com/noirbizarre/bee/releases). Assets are not archives.
Tags have no `v` prefix; for version `0.1.0` the configured assets are:

| Platform | Architecture | Asset |
| --- | --- | --- |
| Linux, GNU libc | x86-64 | `bee_0.1.0_linux-amd64` |
| Linux, GNU libc | ARM64 | `bee_0.1.0_linux-arm64` |
| Linux, musl | x86-64 | `bee_0.1.0_linux-amd64-musl` |
| Linux, musl | ARM64 | `bee_0.1.0_linux-arm64-musl` |
| macOS | Intel | `bee_0.1.0_darwin-amd64` |
| macOS | Apple Silicon | `bee_0.1.0_darwin-arm64` |

Use GNU assets on compatible glibc systems and musl assets on musl systems such as Alpine. No Windows release asset is
configured, although CI tests Windows source builds. The target list does not establish minimum OS or libc versions.

Download the selected asset and `SHA256SUMS` from the **same release** into an empty directory. On Linux, for example:

```bash
sha256sum --ignore-missing -c SHA256SUMS
chmod +x bee_0.1.0_linux-amd64
mkdir -p "$HOME/.local/bin"
install -m 755 bee_0.1.0_linux-amd64 "$HOME/.local/bin/bee"
```

Check that the chosen filename reports `OK` and the checksum command succeeds before installing. On macOS, compare
`shasum -a 256 bee_0.1.0_darwin-arm64` with that filename's entry in `SHA256SUMS`, then use the corresponding filename
in the installation commands above. Change the version/architecture consistently for your download.
Checksums detect corrupted or mismatched downloads; this release workflow does not configure signatures or attestations.

Ensure `$HOME/.local/bin` is in the client process's PATH, or configure the absolute executable path. Verify:

```bash
bee --version
bee --help
```

## Package managers

After their post-release publication succeeds:

```bash
brew install noirbizarre/homebrew-tap/bee
```

The formula supports Intel/Apple Silicon macOS and x86-64 Linux (musl asset); it does not currently provide Linux ARM64.

```bash
paru -S bee-agent
```

The `bee-agent` AUR package installs the `bee` command and supports x86-64 and ARM64 Linux using GNU assets.
Use your preferred AUR helper or review/build the
PKGBUILD manually. Package publication follows the core GitHub release, so availability can lag that release.

## From source

Install Git and Rust via rustup, clone the repository and select the desired release tag when available:

```bash
git clone https://github.com/noirbizarre/bee.git
```

From the cloned directory, run:

```bash
cargo install --path . --locked
bee --version
```

The repository selects the stable Rust channel; the minimum supported Rust version is declared by `rust-version` in
`Cargo.toml`. Cargo normally installs into `~/.cargo/bin`; make that directory visible to your client. Contributors
should use the [development setup](https://github.com/noirbizarre/bee/blob/main/CONTRIBUTING.md).

Bash must be installed for Bee's native Bash tool, including on source-built Windows installations. Continue with
[getting started](getting-started.md) to configure inference and your ACPv2 client.
