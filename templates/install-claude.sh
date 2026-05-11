#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/claude.sh | bash
set -e

# Install a tiny shim that lazily fetches the official Claude installer on
# first invocation, then execs into the real binary. This keeps the image
# small and lets users always run the latest agent without rebuilding.
cat > /usr/local/bin/claude <<'SHIM'
#!/bin/sh
set -e
if [ ! -x "$HOME/.local/bin/claude" ]; then
  curl -fsSL https://claude.ai/install.sh | bash
fi
exec "$HOME/.local/bin/claude" "$@"
SHIM
chmod 0755 /usr/local/bin/claude
echo "Installed claude shim at /usr/local/bin/claude"
