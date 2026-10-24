#!/bin/sh
set -eu

REPO="0PandaDEV/ufrume"

PREFIX="${HOME}/.local"
VERSION=""

die() {
  echo "error: $*" >&2
  exit 1
}

info() {
  echo "==> $*"
}

warn() {
  echo "warning: $*" >&2
}

usage() {
  cat <<'USAGE'
Install ufrume.

Usage: install.sh [options]

Options:
  --version <version>  Install a specific version instead of the latest
  --prefix <dir>       Install prefix (default: ~/.local)
  --system             Shorthand for --prefix /usr/local
  -h, --help           Show this help
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      [ $# -ge 2 ] || die "--version requires a value"
      VERSION="$2"
      shift 2
      ;;
    --prefix)
      [ $# -ge 2 ] || die "--prefix requires a value"
      PREFIX="$2"
      shift 2
      ;;
    --system)
      PREFIX="/usr/local"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

case "$(uname -s)" in
  Linux) OS="linux" ;;
  Darwin) OS="macos" ;;
  *) die "unsupported operating system: $(uname -s)" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64" ;;
  aarch64 | arm64) ARCH="arm64" ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

ASSET="ufrume-${OS}-${ARCH}.tar.gz"

if [ -n "$VERSION" ]; then
  TAG="v${VERSION#v}"
  BASE="https://github.com/${REPO}/releases/download/${TAG}"
else
  TAG="latest"
  BASE="https://github.com/${REPO}/releases/latest/download"
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

info "downloading ${ASSET} (${TAG})"
curl -fsSL -o "$TMP/$ASSET" "$BASE/$ASSET" || die "download failed: $BASE/$ASSET"

if command -v sha256sum >/dev/null 2>&1; then
  SHA="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA="shasum -a 256"
else
  SHA=""
fi

if [ -n "$SHA" ]; then
  if curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS" 2>/dev/null; then
    info "verifying checksum"
    EXPECTED=$(grep " ${ASSET}\$" "$TMP/SHA256SUMS" | awk '{print $1}' | head -n 1)
    [ -n "$EXPECTED" ] || die "no checksum listed for $ASSET - refusing to install"
    ACTUAL=$($SHA "$TMP/$ASSET" | awk '{print $1}')
    [ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch - refusing to install"
  else
    warn "no checksums published for this release, skipping verification"
  fi
else
  warn "sha256sum not found, skipping checksum verification"
fi

info "extracting"
mkdir -p "$TMP/extract"
tar -xzf "$TMP/$ASSET" -C "$TMP/extract"

BIN=$(find "$TMP/extract" -type f -name ufrume | head -n 1)
if [ -z "$BIN" ]; then
  BIN=$(find "$TMP/extract" -type f | head -n 1)
fi
[ -n "$BIN" ] || die "archive does not contain a binary"

mkdir -p "$PREFIX/bin" || die "cannot create $PREFIX/bin"
[ -w "$PREFIX/bin" ] || die "$PREFIX/bin is not writable (use --prefix, or run with sudo for --system)"

info "installing to $PREFIX/bin"
cp "$BIN" "$PREFIX/bin/ufrume.new"
chmod 0755 "$PREFIX/bin/ufrume.new"
mv -f "$PREFIX/bin/ufrume.new" "$PREFIX/bin/ufrume"

info "installed ufrume to $PREFIX/bin/ufrume"

case ":${PATH}:" in
  *":$PREFIX/bin:"*) ;;
  *) warn "$PREFIX/bin is not on your PATH - add it to your shell profile" ;;
esac
