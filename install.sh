#!/usr/bin/env bash
set -euo pipefail

REPO="farbenmeer/ai-pod"
BINARY_NAME="ai-pod"
INSTALL_DIR="${HOME}/.local/bin"

# Detect OS
OS="$(uname -s)"
case "${OS}" in
  Linux*)  OS_NAME="linux" ;;
  Darwin*) OS_NAME="macos" ;;
  *)
    echo "Unsupported OS: ${OS}" >&2
    exit 1
    ;;
esac

# Detect architecture
ARCH="$(uname -m)"
case "${ARCH}" in
  x86_64)          ARCH_NAME="x86_64" ;;
  aarch64 | arm64) ARCH_NAME="aarch64" ;;
  *)
    echo "Unsupported architecture: ${ARCH}" >&2
    exit 1
    ;;
esac

ASSET_NAME="${BINARY_NAME}-${OS_NAME}-${ARCH_NAME}"

# Resolve latest release tag by following the redirect
echo "Fetching latest release..."
LATEST_TAG="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest" | sed 's|.*/||')"

if [ -z "${LATEST_TAG}" ] || [ "${LATEST_TAG}" = "releases" ]; then
  echo "Could not determine latest release. Does the repo have any releases?" >&2
  exit 1
fi

echo "Installing ${BINARY_NAME} ${LATEST_TAG} (${OS_NAME}/${ARCH_NAME})..."

DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/${ASSET_NAME}"

# Create install directory
mkdir -p "${INSTALL_DIR}"

# Download binary to a temp file, then move into place
TMP="$(mktemp)"
trap 'rm -f "${TMP}"' EXIT

if ! curl -fsSL "${DOWNLOAD_URL}" -o "${TMP}"; then
  echo "Download failed: ${DOWNLOAD_URL}" >&2
  exit 1
fi

chmod +x "${TMP}"
mv "${TMP}" "${INSTALL_DIR}/${BINARY_NAME}"

echo "Installed ${BINARY_NAME} to ${INSTALL_DIR}/${BINARY_NAME}"

# Advise the user if the install dir isn't in PATH
if ! printf '%s\n' "${PATH//:/$'\n'}" | grep -qx "${INSTALL_DIR}"; then
  echo ""
  echo "Note: ${INSTALL_DIR} is not in your PATH."
  echo "Add this line to your shell config (~/.bashrc, ~/.zshrc, etc.) and restart your shell:"
  echo ""
  echo "  export PATH=\"\${HOME}/.local/bin:\${PATH}\""
  echo ""
fi

echo "Done! Run '${BINARY_NAME} --help' to get started."
