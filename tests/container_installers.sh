#!/usr/bin/env bash
# Real installer smoke test. Usage: tests/container_installers.sh <agent> <image>
# CONTAINER_ENGINE defaults to podman; image is ubuntu, node, rust, or python.
set -euo pipefail
agent="${1:?agent required}"
image="${2:?image required}"
engine="${CONTAINER_ENGINE:-podman}"
root="$(cd "$(dirname "$0")/.." && pwd)"
case "$agent" in claude|codex|opencode) ;; *) exit 2 ;; esac
case "$image" in
  ubuntu|node|rust|python)
    case "$image" in
      ubuntu) base=docker.io/library/ubuntu:latest ;;
      node) base=docker.io/library/node:lts ;;
      rust) base=docker.io/library/rust:latest ;;
      python) base=docker.io/library/python:latest ;;
    esac
    packages='apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git vim && rm -rf /var/lib/apt/lists/*'
    user='useradd -ms /bin/bash ai-pod'
    ;;
  *) exit 2 ;;
esac
work=$(mktemp -d)
tag="localhost/ai-pod-installer-test-${agent}-${image}-$$"
trap '"$engine" image rm -f "$tag" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
cp "$root/templates/install-$agent.sh" "$work/install.sh"
package_check="test ! -L /usr/bin/$agent"
if [[ "$agent" == codex ]]; then
  package_check='test -L /usr/bin/codex && test -f /usr/lib/codex/codex-package.json && test -x /usr/bin/codex-exec-server && test -x /usr/lib/codex/codex-path/rg && test -x /usr/lib/codex/codex-resources/bwrap'
fi
cat > "$work/Dockerfile" <<DOCKERFILE
FROM $base
RUN $packages
COPY install.sh /tmp/install-agent.sh
RUN bash /tmp/install-agent.sh
RUN $user
USER ai-pod
ENV HOME=/home/ai-pod
RUN $package_check && test "\$(command -v $agent)" = /usr/bin/$agent && $agent --version
CMD ["$agent", "--version"]
DOCKERFILE
"$engine" build -t "$tag" "$work"
# A fresh, network-isolated runtime must work without downloading into HOME.
"$engine" run --rm --network=none "$tag"
if [[ "$agent" == codex ]]; then
  # Version checks alone do not exercise package discovery or daemon installation.
  "$engine" run --rm --network=none --entrypoint codex "$tag" app-server daemon start
fi
echo "PASS: $agent on $image"
