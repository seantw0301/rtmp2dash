#!/usr/bin/env bash
# 一鍵安裝（二進位 dist 包）：安裝到 PREFIX（預設 /opt/rtmp2dash）
# 解壓後也可不經安裝，直接 ./start.sh
set -euo pipefail

PKG_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-/opt/rtmp2dash}"

if [[ ! -x "${PKG_ROOT}/bin/rtmp2dash" ]]; then
  echo "error: binary not found: ${PKG_ROOT}/bin/rtmp2dash" >&2
  echo "This looks like a source package; use the src tarball and ./install.sh there." >&2
  exit 1
fi

if [[ "$(id -u)" -ne 0 ]] && [[ "$PREFIX" == /opt/* || "$PREFIX" == /usr/* ]]; then
  echo "Installing to ${PREFIX} requires root. Re-run with sudo, or set PREFIX:"
  echo "  PREFIX=\$HOME/rtmp2dash ./install.sh"
  exit 1
fi

echo "=== rtmp2dash install (binary) ==="
echo "  source: ${PKG_ROOT}"
echo "  prefix: ${PREFIX}"

mkdir -p "${PREFIX}/bin" "${PREFIX}/cache" "${PREFIX}/logs"

install -m 755 "${PKG_ROOT}/bin/rtmp2dash" "${PREFIX}/bin/rtmp2dash"
install -m 755 "${PKG_ROOT}/start.sh" "${PREFIX}/start.sh"
install -m 755 "${PKG_ROOT}/stop.sh" "${PREFIX}/stop.sh"

if [[ -f "${PREFIX}/config.yaml" ]]; then
  echo "  keeping existing config: ${PREFIX}/config.yaml"
else
  install -m 644 "${PKG_ROOT}/config.yaml" "${PREFIX}/config.yaml"
  echo "  installed config: ${PREFIX}/config.yaml"
fi

echo
echo "Installed successfully."
echo "  start:  ${PREFIX}/start.sh"
echo "  stop:   ${PREFIX}/stop.sh"
echo "  config: ${PREFIX}/config.yaml"
echo
echo "Publish: rtmp://<host>:6136/live/<channel>"
echo "Play:    http://<host>:8080/live/<channel>/index.mpd"
