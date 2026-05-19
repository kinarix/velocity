# typed: false
# frozen_string_literal: true

# Homebrew formula for the `velocity` operator CLI.
#
# This file is rewritten on every CLI release by the `formula` job in
# .github/workflows/release.yml — version + sha256 values are pulled
# from the just-published GitHub Release. Editing by hand is fine for
# local testing but anything you commit will be clobbered on the next
# `make release-cli`.
#
# Install via tap (recommended once the tap repo exists):
#
#   brew tap kinarix/velocity
#   brew install velocity
#
# Or install from this file directly without a tap:
#
#   brew install --formula \
#     https://raw.githubusercontent.com/kinarix/velocity/main/Formula/velocity.rb

class Velocity < Formula
  desc "Operator CLI for Velocity — schema-driven Kubernetes backend platform"
  homepage "https://velocity.kinarix.com"
  version "0.0.0"
  license "Apache-2.0"

  # Pre-built tarballs from the GitHub Release. The `formula` job will
  # rewrite the version + each sha256 after every CLI release; the
  # placeholder zeros here just exist so the file parses on a fresh
  # checkout before the first release lands.
  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/kinarix/velocity/releases/download/v#{version}/velocity-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/kinarix/velocity/releases/download/v#{version}/velocity-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/kinarix/velocity/releases/download/v#{version}/velocity-v#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/kinarix/velocity/releases/download/v#{version}/velocity-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    # The release tarball unpacks to `velocity-v<ver>-<target>/velocity`.
    # Homebrew unpacks into a temp dir whose contents we move into the
    # formula's cellar prefix via `bin.install`.
    bin.install "velocity"
  end

  test do
    # `--version` exits 0 on a healthy binary and prints the version we
    # just installed. If the formula's `version` drifts from what the
    # binary reports, this catches it.
    assert_match version.to_s, shell_output("#{bin}/velocity --version")
  end
end
