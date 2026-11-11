#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/claude.sh | bash
set -e

# Use Anthropic's signed stable repository rather than a per-user install.
if command -v apt-get >/dev/null 2>&1; then
  apt-get update
  apt-get install -y --no-install-recommends ca-certificates curl gnupg
  install -d -m 0755 /etc/apt/keyrings
  curl -fsSL https://downloads.claude.ai/keys/claude-code.asc \
    -o /etc/apt/keyrings/claude-code.asc
  fingerprints=$(gpg --batch --show-keys --with-colons /etc/apt/keyrings/claude-code.asc | awk -F: '$1 == "fpr" {print $10}')
  [ "$fingerprints" = '31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE' ] || {
    echo 'Unexpected Claude Code signing key' >&2
    exit 1
  }
  chmod 0644 /etc/apt/keyrings/claude-code.asc
  echo 'deb [signed-by=/etc/apt/keyrings/claude-code.asc] https://downloads.claude.ai/claude-code/apt/stable stable main' > /etc/apt/sources.list.d/claude-code.list
  apt-get update
  apt-get install -y --no-install-recommends claude-code
  rm -rf /var/lib/apt/lists/*
else
  echo 'Unsupported package manager: Claude Code requires apt' >&2
  exit 1
fi
rm -f /usr/local/bin/claude
claude --version
