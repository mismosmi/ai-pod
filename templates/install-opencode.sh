#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/opencode.sh | bash
set -e

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64)  PKG_ARCH="x64" ;;
  aarch64) PKG_ARCH="arm64" ;;
  *)
    echo "Unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

URL="https://github.com/anomalyco/opencode/releases/latest/download/opencode-linux-${PKG_ARCH}.tar.gz"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL "$URL" -o "$TMPDIR/opencode.tar.gz"
tar -xzf "$TMPDIR/opencode.tar.gz" -C "$TMPDIR"
install -m 0755 "$TMPDIR/opencode" /usr/local/bin/opencode
echo "Installed opencode at /usr/local/bin/opencode"
