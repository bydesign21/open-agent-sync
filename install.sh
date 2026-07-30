#!/bin/sh
# Install agentsync from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/bydesign21/open-agent-sync/master/install.sh | sh
#
# Environment:
#   VERSION            tag to install (default: the latest release)
#   AGENTSYNC_BIN_DIR  where to put the binary (default: ~/.local/bin)
#
# This verifies the download against the release's SHA256SUMS. If you would
# rather not pipe a script into a shell, the README has the same steps written
# out to run by hand.
set -eu

REPO=${REPO:-bydesign21/open-agent-sync}
BIN_DIR=${AGENTSYNC_BIN_DIR:-$HOME/.local/bin}

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# ---- target -----------------------------------------------------------------

os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Darwin) os_part=apple-darwin ;;
  Linux)  os_part=unknown-linux-gnu ;;
  MINGW*|MSYS*|CYGWIN*)
    die "Windows is not installable from this script. Download
  agentsync-<version>-x86_64-pc-windows-msvc.zip from
  https://github.com/$REPO/releases and put agentsync.exe on your PATH." ;;
  *) die "unsupported operating system: $os" ;;
esac

case "$arch" in
  arm64|aarch64) arch_part=aarch64 ;;
  x86_64|amd64)  arch_part=x86_64 ;;
  *) die "unsupported architecture: $arch" ;;
esac

TARGET="$arch_part-$os_part"

# ---- version ----------------------------------------------------------------

if [ -z "${VERSION:-}" ]; then
  say "Looking up the latest release..."
  # Parsed with sed rather than jq, which is not universally installed.
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1)
  [ -n "$VERSION" ] || die "could not determine the latest release; set VERSION=vX.Y.Z"
fi

ASSET="agentsync-$VERSION-$TARGET.tar.gz"
BASE="https://github.com/$REPO/releases/download/$VERSION"

# ---- checksum tool ----------------------------------------------------------

if command -v shasum >/dev/null 2>&1; then
  verify() { shasum -a 256 -c "$1" --ignore-missing; }
elif command -v sha256sum >/dev/null 2>&1; then
  verify() { sha256sum -c "$1" --ignore-missing; }
else
  die "need shasum or sha256sum to verify the download"
fi

# ---- download ---------------------------------------------------------------

tmp=$(mktemp -d)
# shellcheck disable=SC2064
trap "rm -rf '$tmp'" EXIT INT TERM
cd "$tmp"

say "Downloading $ASSET"
curl -fsSLO "$BASE/$ASSET" || die "no such release asset: $ASSET
  Check https://github.com/$REPO/releases for $VERSION."
curl -fsSLO "$BASE/SHA256SUMS" || die "could not download SHA256SUMS"

say "Verifying checksum"
# The published filename is kept deliberately: SHA256SUMS refers to assets by
# name, so renaming the download would make --ignore-missing verify nothing and
# still exit 0.
verify SHA256SUMS >/dev/null || die "checksum verification FAILED for $ASSET"

tar -xzf "$ASSET"
[ -f agentsync ] || die "the archive did not contain an agentsync binary"

mkdir -p "$BIN_DIR"
mv agentsync "$BIN_DIR/agentsync"
chmod +x "$BIN_DIR/agentsync"

# A binary downloaded by curl is quarantined on macOS and refuses to run.
if [ "$os" = Darwin ] && command -v xattr >/dev/null 2>&1; then
  xattr -d com.apple.quarantine "$BIN_DIR/agentsync" 2>/dev/null || true
fi

# ---- report -----------------------------------------------------------------

say ""
say "Installed $("$BIN_DIR/agentsync" --version) to $BIN_DIR/agentsync"

case ":$PATH:" in
  *":$BIN_DIR:"*) say "Run: agentsync" ;;
  *)
    say ""
    say "$BIN_DIR is not on your PATH. Add this to your shell profile:"
    say "    export PATH=\"$BIN_DIR:\$PATH\""
    say ""
    say "Or run it directly: $BIN_DIR/agentsync"
    ;;
esac
