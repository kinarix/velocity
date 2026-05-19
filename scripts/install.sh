#!/bin/sh
#
# Install the `velocity` CLI from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/<org>/velocity/main/scripts/install.sh | sh
#
# Options:
#   VELOCITY_VERSION    Tag to install (default: latest)
#   VELOCITY_PREFIX     Install prefix (default: /usr/local; falls back to $HOME/.local
#                       when /usr/local is not writable)
#   VELOCITY_REPO       GitHub repo (default: anthropic/velocity placeholder — set this
#                       to your fork before publishing)
#
# Detects target as `<arch>-<os>` and downloads the matching tar.gz +
# its .sha256, verifies the checksum, extracts the binary, and copies
# it into $PREFIX/bin. No root required if $PREFIX is user-owned.

set -eu

VELOCITY_REPO="${VELOCITY_REPO:-anthropic/velocity}"
VELOCITY_VERSION="${VELOCITY_VERSION:-}"
VELOCITY_PREFIX="${VELOCITY_PREFIX:-}"

# ── target detection ───────────────────────────────────────────────
uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "$uname_s" in
  Linux)  os="unknown-linux-musl" ;;
  Darwin) os="apple-darwin"        ;;
  *) echo "unsupported OS: $uname_s" >&2; exit 1 ;;
esac

case "$uname_m" in
  x86_64|amd64)   arch="x86_64"  ;;
  aarch64|arm64)  arch="aarch64" ;;
  *) echo "unsupported arch: $uname_m" >&2; exit 1 ;;
esac

target="${arch}-${os}"
echo "==> target: $target"

# ── resolve version ────────────────────────────────────────────────
if [ -z "$VELOCITY_VERSION" ]; then
  echo "==> resolving latest release..."
  if command -v curl >/dev/null 2>&1; then
    VELOCITY_VERSION="$(curl -fsSL "https://api.github.com/repos/${VELOCITY_REPO}/releases/latest" \
      | grep '"tag_name"' | head -n1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
  else
    echo "curl is required" >&2; exit 1
  fi
fi

if [ -z "$VELOCITY_VERSION" ]; then
  echo "could not resolve a release — set VELOCITY_VERSION=v0.1.0 manually" >&2
  exit 1
fi
echo "==> version: $VELOCITY_VERSION"

# ── prefix ─────────────────────────────────────────────────────────
if [ -z "$VELOCITY_PREFIX" ]; then
  if [ -w /usr/local/bin ] 2>/dev/null; then
    VELOCITY_PREFIX="/usr/local"
  else
    VELOCITY_PREFIX="$HOME/.local"
    echo "==> /usr/local not writable; installing to $VELOCITY_PREFIX"
    echo "==> ensure $VELOCITY_PREFIX/bin is on your PATH"
  fi
fi
mkdir -p "$VELOCITY_PREFIX/bin"

# ── download + verify ──────────────────────────────────────────────
asset="velocity-${VELOCITY_VERSION}-${target}.tar.gz"
url="https://github.com/${VELOCITY_REPO}/releases/download/${VELOCITY_VERSION}/${asset}"
sha_url="${url}.sha256"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "==> downloading $url"
curl -fsSL "$url"      -o "$tmp/$asset"
curl -fsSL "$sha_url"  -o "$tmp/$asset.sha256"

echo "==> verifying checksum"
expected="$(awk '{print $1}' "$tmp/$asset.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
fi
if [ "$expected" != "$actual" ]; then
  echo "checksum mismatch" >&2
  echo "  expected: $expected" >&2
  echo "  actual:   $actual"   >&2
  exit 1
fi

# ── install ────────────────────────────────────────────────────────
echo "==> extracting"
tar -xzf "$tmp/$asset" -C "$tmp"
dir="velocity-${VELOCITY_VERSION}-${target}"
install -m 0755 "$tmp/$dir/velocity" "$VELOCITY_PREFIX/bin/velocity"

echo "==> installed $VELOCITY_PREFIX/bin/velocity"
"$VELOCITY_PREFIX/bin/velocity" --version
echo
echo "next: velocity context add <name> --api-url <url> --token <token>"
