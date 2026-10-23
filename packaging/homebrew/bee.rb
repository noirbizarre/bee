# Homebrew formula template.
#
# `@VERSION@` and the `@SHA256_*@` placeholders are substituted by
# .github/workflows/homebrew.yaml from the published release assets, and the
# result is pushed to noirbizarre/homebrew-tap as Formula/bee.rb.
class Bee < Formula
  desc "A minimal, headless Rust agent runtime implementing ACPv2."
  homepage "https://github.com/noirbizarre/bee"
  version "@VERSION@"
  license "MIT"

  # The release asset is the raw executable itself, not an archive — this
  # project's publish workflow uploads one so it can become a `gh` extension
  # without renaming assets later, and Homebrew installs a bare download
  # exactly as well as an archived one.
  #
  # This project tags without a `v` prefix, so the tag is `#{version}` as-is.
  on_macos do
    on_arm do
      url "https://github.com/noirbizarre/bee/releases/download/#{version}/bee_#{version}_darwin-arm64"
      sha256 "@SHA256_DARWIN_ARM64@"
    end
    on_intel do
      url "https://github.com/noirbizarre/bee/releases/download/#{version}/bee_#{version}_darwin-amd64"
      sha256 "@SHA256_DARWIN_AMD64@"
    end
  end

  # musl rather than gnu: the binary is statically linked, so it runs on any
  # distribution Homebrew supports regardless of its glibc.
  on_linux do
    on_intel do
      url "https://github.com/noirbizarre/bee/releases/download/#{version}/bee_#{version}_linux-amd64-musl"
      sha256 "@SHA256_LINUX_AMD64_MUSL@"
    end
  end

  def install
    # Exactly one file lands here, whichever `url` above matched — renamed on
    # the way in because the downloaded asset's name carries the platform
    # suffix, not the command users are meant to type.
    bin.install Dir["*"].first => "bee"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/bee --version")
  end
end
