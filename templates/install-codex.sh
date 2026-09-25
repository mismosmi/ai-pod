#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/codex.sh | bash
set -e

# Install a tiny shim that lazily fetches the official Codex installer on
# first invocation, then execs into the real binary. This keeps the image
# small and lets users always run the latest agent without rebuilding.
cat > /usr/local/bin/codex <<'SHIM'
#!/bin/sh
set -e
if [ ! -x "$HOME/.local/bin/codex" ]; then
  curl -fsSL https://chatgpt.com/codex/install.sh | bash
fi
exec "$HOME/.local/bin/codex" "$@"
SHIM
chmod 0755 /usr/local/bin/codex
echo "Installed codex shim at /usr/local/bin/codex"

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
