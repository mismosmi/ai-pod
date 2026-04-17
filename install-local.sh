#!/usr/bin/env bash
set -euo pipefail

# Detect container runtime (podman preferred, docker fallback)
if command -v podman &>/dev/null; then
  RT="podman"
elif command -v docker &>/dev/null; then
  RT="docker"
else
  echo "Neither podman nor docker found. Install one and ensure it is on your PATH." >&2
  exit 1
fi

ARCH="$(uname -m)"
case "${ARCH}" in
  x86_64)          TARGET="x86_64-unknown-linux-musl" ;;
  aarch64 | arm64) TARGET="aarch64-unknown-linux-musl" ;;
  *)
    echo "Unsupported architecture: ${ARCH}" >&2
    exit 1
    ;;
esac

OUTPUT_DIR="${HOME}/.ai-pod"
OUTPUT="${OUTPUT_DIR}/host-tools"

mkdir -p "${OUTPUT_DIR}"
mkdir -p "${HOME}/.cargo/registry"

echo "Building host-tools for ${TARGET} using ${RT}..."

"${RT}" run --rm \
  -v "$(pwd):/src:z" \
  -v "${HOME}/.cargo/registry:/usr/local/cargo/registry:z" \
  -w /src \
  rust:alpine \
  sh -c "apk add --no-cache musl-dev && rustup target add ${TARGET} && cargo build --release --bin host-tools --target ${TARGET}"

cp "target/${TARGET}/release/host-tools" "${OUTPUT}"
chmod +x "${OUTPUT}"

echo "Installed host-tools to ${OUTPUT}"

echo "Building ai-pod..."
cargo build --release --bin ai-pod

mkdir -p "${HOME}/.local/bin"
cp target/release/ai-pod "${HOME}/.local/bin/ai-pod"
chmod +x "${HOME}/.local/bin/ai-pod"

# On macOS, re-apply ad-hoc signature after copy to avoid "Killed: 9"
if [[ "$(uname -s)" == "Darwin" ]]; then
  codesign --force --sign - "${HOME}/.local/bin/ai-pod"
fi

echo "Installed ai-pod to ${HOME}/.local/bin/ai-pod"
