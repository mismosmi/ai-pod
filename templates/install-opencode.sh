#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/opencode.sh | bash
set -e

case "$(uname -m)" in
  x86_64) arch=x64-baseline ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
# Baseline x64 also supports hosts without AVX2.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
curl -fsSL "https://opencode.ai/update/api/latest/cli/opencode/opencode-linux-${arch}.tar.gz" -o "$work/opencode.tar.gz"
tar -xzf "$work/opencode.tar.gz" -C "$work"
mkdir -p /usr/bin
cp "$work/opencode" /usr/bin/opencode
chmod 0755 /usr/bin/opencode
rm -f /usr/local/bin/opencode
/usr/bin/opencode --version
