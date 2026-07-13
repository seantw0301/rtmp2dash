#!/usr/bin/env bash
# Start rtmp2dash in the background (release build).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PID_FILE="${ROOT}/script/rtmp2dash.pid"
LOG_FILE="${ROOT}/script/rtmp2dash.log"
CONFIG="${CONFIG:-${ROOT}/config.yaml}"
BIN="${ROOT}/target/release/rtmp2dash"

if [[ -f "$PID_FILE" ]]; then
  old_pid="$(cat "$PID_FILE" 2>/dev/null || true)"
  if [[ -n "${old_pid}" ]] && kill -0 "$old_pid" 2>/dev/null; then
    echo "rtmp2dash already running (pid=${old_pid})"
    exit 0
  fi
  rm -f "$PID_FILE"
fi

echo "Building release binary..."
cargo build --release --manifest-path "${ROOT}/Cargo.toml"

if [[ ! -x "$BIN" ]]; then
  echo "error: binary not found: $BIN" >&2
  exit 1
fi

if [[ ! -f "$CONFIG" ]]; then
  echo "error: config not found: $CONFIG" >&2
  exit 1
fi

mkdir -p "${ROOT}/cache" "$(dirname "$LOG_FILE")"

echo "Starting rtmp2dash (config=${CONFIG})..."
nohup "$BIN" --config "$CONFIG" >>"$LOG_FILE" 2>&1 &
pid=$!
echo "$pid" >"$PID_FILE"

# Brief readiness check
sleep 0.3
if ! kill -0 "$pid" 2>/dev/null; then
  echo "error: process exited immediately; see $LOG_FILE" >&2
  rm -f "$PID_FILE"
  exit 1
fi

echo "rtmp2dash started (pid=${pid})"
echo "  log: $LOG_FILE"
echo "  publish: rtmp://<host>:1935/live/<channel>"
echo "  play:    http://<host>:8080/live/<channel>/index.mpd"
