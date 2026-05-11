#!/usr/bin/env bash
set -euo pipefail

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
