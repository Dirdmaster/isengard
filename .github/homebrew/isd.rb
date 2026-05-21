# Hand-written Homebrew formula skeleton for `isd`. This file is the
# bootstrap copy for the v0.6.0 release; the operator hand-publishes it
# into `Weavers-Engineering/homebrew-isengard/Formula/isd.rb` for the
# first release. Once the cargo-dist Homebrew auto-bump flow runs once
# (after `HOMEBREW_TAP_TOKEN` is provisioned and the v0.6.0 release lands),
# this file can be deleted; cargo-dist will keep the tap formula in sync
# from then on.
#
# To update by hand (until automation lands):
#   1. cut the release tag (e.g. v0.6.0)
#   2. wait for cargo-dist to publish the tarballs and the source archive
#   3. compute sha256 for each tarball:
#        for t in aarch64-apple-darwin x86_64-apple-darwin \
#                 aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
#          curl -fsSL "https://github.com/Weavers-Engineering/Isengard/releases/download/v0.6.0/isd-${t}.tar.xz" | shasum -a 256
#        done
#   4. paste the digests below
#   5. copy this file into Weavers-Engineering/homebrew-isengard/Formula/isd.rb
#   6. open a PR on the tap repo
#
# Spec: 3 Resources/Superpowers/specs/2026-05-21-isengard-v0.6.0-distribution-design.md

class Isd < Formula
  desc "Isengard operator CLI"
  homepage "https://isengard.app"
  version "0.6.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Weavers-Engineering/Isengard/releases/download/v#{version}/isd-aarch64-apple-darwin.tar.xz"
      sha256 "REPLACE_WITH_AARCH64_DARWIN_SHA256"
    else
      url "https://github.com/Weavers-Engineering/Isengard/releases/download/v#{version}/isd-x86_64-apple-darwin.tar.xz"
      sha256 "REPLACE_WITH_X86_64_DARWIN_SHA256"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Weavers-Engineering/Isengard/releases/download/v#{version}/isd-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "REPLACE_WITH_AARCH64_LINUX_SHA256"
    else
      url "https://github.com/Weavers-Engineering/Isengard/releases/download/v#{version}/isd-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "REPLACE_WITH_X86_64_LINUX_SHA256"
    end
  end

  def install
    bin.install "isd"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/isd --version")
  end
end
