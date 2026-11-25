#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/claude.sh | bash
set -e

# Download Anthropic's release signing key to $1 and refuse anything else.
fetch_signing_key() {
  curl -fsSL https://downloads.claude.ai/keys/claude-code.asc -o "$1"
  fingerprints=$(gpg --batch --show-keys --with-colons "$1" | awk -F: '$1 == "fpr" {print $10}')
  [ "$fingerprints" = '31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE' ] || {
    echo 'Unexpected Claude Code signing key' >&2
    exit 1
  }
  chmod 0644 "$1"
}

# Use Anthropic's signed stable repository rather than a per-user install.
if command -v apt-get >/dev/null 2>&1; then
  apt-get update
  apt-get install -y --no-install-recommends ca-certificates curl gnupg
  install -d -m 0755 /etc/apt/keyrings
  fetch_signing_key /etc/apt/keyrings/claude-code.asc
  echo 'deb [signed-by=/etc/apt/keyrings/claude-code.asc] https://downloads.claude.ai/claude-code/apt/stable stable main' > /etc/apt/sources.list.d/claude-code.list
  apt-get update
  apt-get install -y --no-install-recommends claude-code
  rm -rf /var/lib/apt/lists/*
elif command -v dnf >/dev/null 2>&1; then
  command -v curl >/dev/null 2>&1 || dnf install -y curl
  command -v gpg >/dev/null 2>&1 || dnf install -y gnupg2
  install -d -m 0755 /etc/pki/rpm-gpg
  fetch_signing_key /etc/pki/rpm-gpg/RPM-GPG-KEY-claude-code
  cat > /etc/yum.repos.d/claude-code.repo <<'REPO'
[claude-code]
name=Claude Code
baseurl=https://downloads.claude.ai/claude-code/rpm/stable
enabled=1
gpgcheck=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-claude-code
REPO
  dnf install -y claude-code
  dnf clean all
else
  echo 'Unsupported package manager: Claude Code requires apt or dnf' >&2
  exit 1
fi
rm -f /usr/local/bin/claude
claude --version
