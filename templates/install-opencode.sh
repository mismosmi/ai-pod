#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/opencode.sh | bash
set -e

# Install a tiny shim that lazily fetches the official OpenCode V2 installer
# on first invocation, then execs into the real binary, like Claude and Codex.
cat > /usr/local/bin/opencode <<'SHIM'
#!/bin/sh
set -e
if [ ! -x "$HOME/.opencode/bin/opencode" ]; then
  # The installer probes `opencode --version`, which resolves to this shim.
  # Report no installed version during that probe instead of installing again.
  [ "${AI_POD_OPENCODE_INSTALLING:-}" != 1 ] || exit 1
  curl -fsSL https://opencode.ai/v2/install | AI_POD_OPENCODE_INSTALLING=1 bash -s -- --no-modify-path
fi
exec "$HOME/.opencode/bin/opencode" "$@"
SHIM
chmod 0755 /usr/local/bin/opencode
echo "Installed opencode shim at /usr/local/bin/opencode"
