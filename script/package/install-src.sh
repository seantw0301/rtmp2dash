#!/usr/bin/env bash
# 一鍵安裝（原始碼包）：在當前平台編譯 release，再安裝到 PREFIX
#
# 用法：
#   ./install.sh                         # 編譯並安裝到本目錄（./bin）
#   PREFIX=/opt/rtmp2dash sudo ./install.sh
#   PREFIX=$HOME/rtmp2dash ./install.sh
set -euo pipefail

PKG_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$PKG_ROOT"

# 預設原地安裝（解壓後直接 ./install.sh && ./start.sh）
PREFIX="${PREFIX:-$PKG_ROOT}"

if [[ "$PREFIX" != "$PKG_ROOT" ]] && [[ "$(id -u)" -ne 0 ]] && [[ "$PREFIX" == /opt/* || "$PREFIX" == /usr/* ]]; then
  echo "Installing to ${PREFIX} requires root. Re-run with sudo, or set PREFIX:"
  echo "  PREFIX=\$HOME/rtmp2dash ./install.sh"
  echo "  # 或不設 PREFIX：編譯到本目錄 ./bin"
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. Install Rust first: https://rustup.rs/" >&2
  exit 1
fi

if [[ ! -f "${PKG_ROOT}/Cargo.toml" ]]; then
  echo "error: Cargo.toml not found (not a source package?)" >&2
  exit 1
fi

# Avoid being pulled into a parent directory's Cargo workspace.
if ! grep -q '^\[workspace\]' "${PKG_ROOT}/Cargo.toml"; then
  printf '\n# Standalone package: ignore any parent Cargo workspace.\n[workspace]\n' >> "${PKG_ROOT}/Cargo.toml"
fi

echo "=== rtmp2dash install (from source) ==="
echo "  source: ${PKG_ROOT}"
echo "  prefix: ${PREFIX}"
echo "  rustc:  $(rustc --version 2>/dev/null || echo unknown)"

echo "=== cargo build --release ==="
cargo build --release --manifest-path "${PKG_ROOT}/Cargo.toml"

# Parent workspace / CARGO_TARGET_DIR may redirect artifacts away from ./target.
TARGET_DIR="$(
  cargo metadata --format-version=1 --no-deps --manifest-path "${PKG_ROOT}/Cargo.toml" 2>/dev/null \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("target_directory",""))' 2>/dev/null \
    || true
)"
TARGET_DIR="${TARGET_DIR:-${CARGO_TARGET_DIR:-${PKG_ROOT}/target}}"
BIN_SRC="${TARGET_DIR}/release/rtmp2dash"
if [[ ! -x "$BIN_SRC" ]]; then
  echo "error: binary not found or not executable: $BIN_SRC" >&2
  echo "  CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-<unset>}" >&2
  ls -la "${TARGET_DIR}/release" 2>/dev/null || true
  exit 1
fi

mkdir -p "${PREFIX}/bin" "${PREFIX}/cache" "${PREFIX}/logs"

install -m 755 "$BIN_SRC" "${PREFIX}/bin/rtmp2dash"
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
echo "  binary: ${PREFIX}/bin/rtmp2dash"
echo "  start:  ${PREFIX}/start.sh"
echo "  stop:   ${PREFIX}/stop.sh"
echo "  config: ${PREFIX}/config.yaml"
echo
echo "Publish: rtmp://<host>:6136/live/<channel>"
echo "Play:    http://<host>:8080/live/<channel>/index.mpd"
