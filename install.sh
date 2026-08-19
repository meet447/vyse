#!/usr/bin/env bash
set -euo pipefail

REPO="meet447/vyse"
INSTALL_DIR="${VYSE_INSTALL_DIR:-$HOME/.vyse/bin}"

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "${os}-${arch}" in
    Darwin-arm64) echo "aarch64-apple-darwin" ;;
    Darwin-x86_64) echo "x86_64-apple-darwin" ;;
    Linux-x86_64) echo "x86_64-unknown-linux-gnu" ;;
    *)
      echo "Unsupported platform: ${os} ${arch}" >&2
      echo "Download a binary from https://github.com/${REPO}/releases/latest" >&2
      exit 1
      ;;
  esac
}

target="$(detect_target)"
url="https://github.com/${REPO}/releases/latest/download/vyse-${target}.tar.gz"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

echo "Downloading vyse for ${target}..."
curl -fsSL "${url}" -o "${tmpdir}/vyse.tar.gz"
tar xzf "${tmpdir}/vyse.tar.gz" -C "${tmpdir}"

mkdir -p "${INSTALL_DIR}"
install -m 755 "${tmpdir}/vyse" "${INSTALL_DIR}/vyse"

echo
echo "Installed vyse to ${INSTALL_DIR}/vyse"
echo
echo "Add to PATH (for example in ~/.bashrc or ~/.zshrc):"
echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
