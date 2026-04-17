#!/usr/bin/env bash
# End-to-end test: verify one agent x base-image combination produces a
# working container where the agent binary is installed and runnable.
#
# Usage:  ./tests/e2e_agents.sh <agent> <image>
# Example: ./tests/e2e_agents.sh claude alpine
set -euo pipefail

AGENT="${1:?Usage: e2e_agents.sh <agent> <image>}"
IMAGE="${2:?Usage: e2e_agents.sh <agent> <image>}"

case "$AGENT" in
  claude)   VERIFY_ARGS="claude --version" ;;
  opencode) VERIFY_ARGS="opencode version" ;;
  *)        echo "Unknown agent: $AGENT"; exit 1 ;;
esac

COMBO="${AGENT} + ${IMAGE}"
echo "=== Testing: ${COMBO} ==="

WORK="$(mktemp -d)"
trap 'ai-pod --workdir "$WORK" clean 2>/dev/null || true; rm -rf "$WORK"' EXIT

ai-pod init --workdir "$WORK" --agent "$AGENT" --image "$IMAGE"
git init -q "$WORK"

# shellcheck disable=SC2086
ai-pod --workdir "$WORK" --no-credential-check run $VERIFY_ARGS

echo "PASS: ${COMBO}"
