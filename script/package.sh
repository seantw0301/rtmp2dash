#!/usr/bin/env bash
# 一鍵打包：產出平台二進位包（dist）或原始碼包（src）
#
# 用法：
#   ./script/package.sh          # 同時產出 dist + src
#   ./script/package.sh dist     # 僅當前平台二進位包（佈署同架構機器）
#   ./script/package.sh src      # 僅原始碼包（佈署到其他平台後再編譯）
#   ./script/package.sh all      # 同無參數
#
# 產出：
#   dist/rtmp2dash-<os>-<arch>.tar.gz   # 含預編譯二元檔
#   dist/rtmp2dash-src.tar.gz           # 含原始碼，目標機需 Rust
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT_DIR="${ROOT}/script"
PKG_TMPL="${SCRIPT_DIR}/package"
cd "$ROOT"

MODE="${1:-all}"
ARCH="$(uname -m)"
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "${ROOT}/Cargo.toml" | head -1)"
DIST_DIR="${ROOT}/dist"

usage() {
  cat <<EOF
用法: $0 [dist|src|all]

  dist  打包當前平台二進位安裝包 → dist/rtmp2dash-${OS}-${ARCH}.tar.gz
  src   打包原始碼安裝包（跨平台）→ dist/rtmp2dash-src.tar.gz
  all   同時產出以上兩者（預設）
EOF
}

case "$MODE" in
  dist|src|all) ;;
  -h|--help|help)
    usage
    exit 0
    ;;
  *)
    echo "error: unknown mode: $MODE" >&2
    usage >&2
    exit 1
    ;;
esac

for f in install.sh install-src.sh start.sh stop.sh; do
  if [[ ! -f "${PKG_TMPL}/${f}" ]]; then
    echo "error: missing template: ${PKG_TMPL}/${f}" >&2
    exit 1
  fi
done

mkdir -p "$DIST_DIR"

write_readme_common() {
  local out="$1"
  local kind="$2"
  local platform_line="$3"
  cat > "$out" <<EOF
rtmp2dash v${VERSION} — RTMP → live MPEG-DASH
Package: ${kind}
${platform_line}

推流：rtmp://<host>:6136/live/<channel>
播放：http://<host>:8080/live/<channel>/index.mpd

請依需求編輯 config.yaml。Codec：僅 H.264 + AAC。
EOF
}

package_dist() {
  local name="rtmp2dash-${OS}-${ARCH}"
  local stage="${DIST_DIR}/${name}"
  local out_tgz="${DIST_DIR}/${name}.tar.gz"

  echo "=== [dist] build release (${OS}/${ARCH}) ==="
  cargo build --release --manifest-path "${ROOT}/Cargo.toml"

  TARGET_DIR="$(
    cargo metadata --format-version=1 --no-deps --manifest-path "${ROOT}/Cargo.toml" 2>/dev/null \
      | python3 -c 'import json,sys; print(json.load(sys.stdin).get("target_directory",""))' 2>/dev/null \
      || true
  )"
  TARGET_DIR="${TARGET_DIR:-${CARGO_TARGET_DIR:-${ROOT}/target}}"
  local bin="${TARGET_DIR}/release/rtmp2dash"
  if [[ ! -x "$bin" ]]; then
    echo "error: binary not found or not executable: $bin" >&2
    ls -la "${TARGET_DIR}/release" 2>/dev/null || true
    exit 1
  fi

  echo "=== [dist] stage ${name} ==="
  rm -rf "$stage"
  mkdir -p "${stage}/bin"

  install -m 755 "$bin" "${stage}/bin/rtmp2dash"
  install -m 644 "${ROOT}/config.yaml" "${stage}/config.yaml"
  install -m 755 "${PKG_TMPL}/install.sh" "${stage}/install.sh"
  install -m 755 "${PKG_TMPL}/start.sh" "${stage}/start.sh"
  install -m 755 "${PKG_TMPL}/stop.sh" "${stage}/stop.sh"
  install -m 644 "${ROOT}/LICENSE" "${stage}/LICENSE"

  write_readme_common "${stage}/README.txt" "binary (dist)" "Platform: ${OS}/${ARCH}"
  cat >> "${stage}/README.txt" <<EOF

解壓後直接使用（無需編譯）：
  tar -xzf ${name}.tar.gz
  cd ${name}
  ./start.sh
  ./stop.sh

安裝到固定路徑（預設 /opt/rtmp2dash）：
  sudo ./install.sh
  # 或：PREFIX=\$HOME/rtmp2dash ./install.sh
EOF

  echo "=== [dist] create ${out_tgz} ==="
  rm -f "$out_tgz"
  tar -C "$DIST_DIR" -czf "$out_tgz" "$name"
  # 相容舊檔名
  ln -sfn "$(basename "$out_tgz")" "${DIST_DIR}/rtmp2dash.tar.gz"

  echo
  echo "dist 安裝包: ${out_tgz}"
  ls -lh "$out_tgz"
  tar -tzf "$out_tgz" | head -20
  echo
}

package_src() {
  local name="rtmp2dash-src"
  local stage="${DIST_DIR}/${name}"
  local out_tgz="${DIST_DIR}/${name}.tar.gz"

  echo "=== [src] stage ${name} ==="
  rm -rf "$stage"
  mkdir -p "$stage"

  # 原始碼與建置所需檔案
  install -m 644 "${ROOT}/Cargo.toml" "${stage}/Cargo.toml"
  install -m 644 "${ROOT}/Cargo.lock" "${stage}/Cargo.lock"
  install -m 644 "${ROOT}/LICENSE" "${stage}/LICENSE"
  install -m 644 "${ROOT}/config.yaml" "${stage}/config.yaml"
  install -m 644 "${ROOT}/README.md" "${stage}/README.md"
  install -m 644 "${ROOT}/README_TW.md" "${stage}/README_TW.md"

  # src/
  mkdir -p "${stage}/src"
  tar -C "${ROOT}" \
    --exclude='.DS_Store' \
    -cf - src | tar -C "$stage" -xf -

  # doc/（可選，方便目標機查閱）
  if [[ -d "${ROOT}/doc" ]]; then
    mkdir -p "${stage}/doc"
    tar -C "${ROOT}" \
      --exclude='.DS_Store' \
      -cf - doc | tar -C "$stage" -xf -
  fi

  # 一鍵腳本：安裝會在目標平台 cargo build
  install -m 755 "${PKG_TMPL}/install-src.sh" "${stage}/install.sh"
  install -m 755 "${PKG_TMPL}/start.sh" "${stage}/start.sh"
  install -m 755 "${PKG_TMPL}/stop.sh" "${stage}/stop.sh"

  write_readme_common "${stage}/README.txt" "source (src)" "Build on target: any OS/arch with Rust (rustup)"
  cat >> "${stage}/README.txt" <<EOF

需求：目標機器已安裝 Rust（https://rustup.rs/）

跨平台佈署：
  tar -xzf ${name}.tar.gz
  cd ${name}
  ./install.sh          # 在本機編譯 release → ./bin/rtmp2dash
  ./start.sh
  ./stop.sh

安裝到固定路徑：
  sudo ./install.sh
  # 或：PREFIX=\$HOME/rtmp2dash ./install.sh
EOF

  echo "=== [src] create ${out_tgz} ==="
  rm -f "$out_tgz"
  tar -C "$DIST_DIR" -czf "$out_tgz" "$name"

  echo
  echo "src 安裝包: ${out_tgz}"
  ls -lh "$out_tgz"
  tar -tzf "$out_tgz" | head -40
  echo
}

case "$MODE" in
  dist) package_dist ;;
  src)  package_src ;;
  all)
    package_dist
    package_src
    ;;
esac

echo "完成。產出目錄: ${DIST_DIR}"
ls -lh "${DIST_DIR}"/*.tar.gz 2>/dev/null || true
