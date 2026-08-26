#!/bin/sh
# Install cswap. Usage:
#   curl -fsSL https://raw.githubusercontent.com/ibrahimthecosmic/cswap/main/install.sh | sh
#
# Environment:
#   CSWAP_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   CSWAP_VERSION       a tag such as v0.1.0 (default: the latest release)
set -eu

REPO="ibrahimthecosmic/cswap"
INSTALL_DIR="${CSWAP_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${CSWAP_VERSION:-latest}"

die() { printf 'install.sh: %s\n' "$1" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64|Linux-amd64) asset="cswap-linux-x86_64" ;;
  Darwin-*) die "no macOS binary is published yet - build from source: cargo install --git https://github.com/$REPO" ;;
  *) die "unsupported platform $os-$arch - build from source: cargo install --git https://github.com/$REPO" ;;
esac

if [ "$VERSION" = latest ]; then
  url="https://github.com/$REPO/releases/latest/download/$asset"
else
  url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  die "neither curl nor wget is available"
fi

tmp=$(mktemp) || die "could not create a temporary file"
trap 'rm -f "$tmp"' EXIT INT TERM

printf 'Downloading %s (%s)...\n' "$asset" "$VERSION"
fetch "$url" "$tmp" || die "download failed - does the release exist? $url"

# Refuse an HTML error page renamed to look like a binary.
head -c 4 "$tmp" | grep -q ELF || die "downloaded file is not a Linux executable"

mkdir -p "$INSTALL_DIR" || die "could not create $INSTALL_DIR"
chmod 755 "$tmp"
mv -f "$tmp" "$INSTALL_DIR/cswap" || die "could not install to $INSTALL_DIR (try CSWAP_INSTALL_DIR=/usr/local/bin with sudo)"
trap - EXIT INT TERM

printf 'Installed %s\n' "$INSTALL_DIR/cswap"
"$INSTALL_DIR/cswap" --version

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf '\n%s is not on your PATH. Add it with:\n  export PATH="%s:$PATH"\n' "$INSTALL_DIR" "$INSTALL_DIR" ;;
esac
