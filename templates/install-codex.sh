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

BASE_URL="https://github.com/openai/codex/releases/latest/download"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

# Fetches one release tarball and installs the single binary it contains under
# the given name. The tarballs name their binary with the full target triple,
# so glob for it instead of assuming the exact layout.
install_release_binary() {
  asset="$1"
  dest="$2"
  dir="$TMPDIR/$dest"
  mkdir -p "$dir"

  curl -fsSL "$BASE_URL/${asset}-${TRIPLE}.tar.gz" -o "$dir/archive.tar.gz"
  tar -xzf "$dir/archive.tar.gz" -C "$dir"

  bin="$(find "$dir" -maxdepth 2 -name "${asset}-*-unknown-linux-musl" -type f | head -n1)"
  if [ -z "$bin" ]; then
    echo "Could not locate $asset binary in release archive" >&2
    return 1
  fi
  install -m 0755 "$bin" "/usr/local/bin/$dest"
  echo "Installed $dest at /usr/local/bin/$dest"
}

# The musl-static binaries run on every ai-pod base image (Alpine and glibc alike).
install_release_binary codex codex

# Codex spawns this sibling helper when code mode is enabled; without it every
# code-mode turn fails with "codex-code-mode-host: No such file or directory".
# Not fatal if the asset is missing from a given release.
install_release_binary codex-code-mode-host codex-code-mode-host \
  || echo "Skipping codex-code-mode-host (asset unavailable); code mode will be unavailable" >&2

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
