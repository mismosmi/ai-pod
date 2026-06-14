#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/codex.sh | bash
set -e

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64)  TRIPLE="x86_64-unknown-linux-musl" ;;
  aarch64) TRIPLE="aarch64-unknown-linux-musl" ;;
  *)
    echo "Unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

# The musl-static binary runs on every ai-pod base image (Alpine and glibc alike).
URL="https://github.com/openai/codex/releases/latest/download/codex-${TRIPLE}.tar.gz"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL "$URL" -o "$TMPDIR/codex.tar.gz"
tar -xzf "$TMPDIR/codex.tar.gz" -C "$TMPDIR"

# The release tarball names the binary with the full target triple, not "codex".
# Glob for it so minor naming/layout changes don't break the install.
BIN="$(find "$TMPDIR" -maxdepth 2 -name 'codex-*-unknown-linux-musl' -type f | head -n1)"
if [ -z "$BIN" ]; then
  echo "Could not locate codex binary in release archive" >&2
  exit 1
fi
install -m 0755 "$BIN" /usr/local/bin/codex
echo "Installed codex at /usr/local/bin/codex"

# Completion-notification helper invoked by codex's `notify` config option.
# Codex passes a JSON event as $1; we ignore it and send a generic message,
# reading the ai-pod credentials from the container env at runtime.
cat > /usr/local/bin/ai-pod-codex-notify <<'NOTIFY'
#!/bin/sh
url="${AI_POD_SERVER_URL%/}"
[ -n "$url" ] && [ -n "$AI_POD_API_KEY" ] && [ -n "$AI_POD_PROJECT_ID" ] || exit 0
curl -fsS -X POST \
  -H "X-Api-Key: $AI_POD_API_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"project_id\":\"$AI_POD_PROJECT_ID\",\"message\":\"Codex: Task completed\"}" \
  "$url/notify_user" >/dev/null 2>&1 || true
NOTIFY
chmod 0755 /usr/local/bin/ai-pod-codex-notify
echo "Installed codex notify helper at /usr/local/bin/ai-pod-codex-notify"
