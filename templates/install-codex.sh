#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/codex.sh | bash
set -e

# Use the portable upstream package on all supported base images.
case "$(uname -m)" in
  x86_64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
curl -fsSL "https://github.com/openai/codex/releases/latest/download/codex-package-${arch}-unknown-linux-musl.tar.gz" -o "$work/codex.tar.gz"
mkdir -p "$work/package" /usr/bin /usr/lib
tar -xzf "$work/codex.tar.gz" -C "$work/package"
# The daemon copies the complete package. Keep its root isolated from system
# directories and expose the native executables on every user's PATH.
rm -rf /usr/lib/codex
mv "$work/package" /usr/lib/codex
for binary in /usr/lib/codex/bin/*; do
  ln -sf "$binary" "/usr/bin/${binary##*/}"
done
# Remove the previous layout, which incorrectly treated all of /usr as a package.
rm -f /usr/codex-package.json
rm -rf /usr/codex-path /usr/codex-resources
rm -f /usr/local/bin/codex
/usr/bin/codex --version

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
