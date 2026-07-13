#!/usr/bin/env bash
# 一鍵啟動：背景執行 rtmp2dash（binary dist / install.sh 產出的 ./bin）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

PID_FILE="${ROOT}/logs/rtmp2dash.pid"
LOG_FILE="${ROOT}/logs/rtmp2dash.log"
CONFIG="${CONFIG:-${ROOT}/config.yaml}"
BIN="${ROOT}/bin/rtmp2dash"

dump_log() {
  echo "---- last 80 lines of ${LOG_FILE} ----" >&2
  if [[ -f "$LOG_FILE" ]]; then
    tail -n 80 "$LOG_FILE" >&2 || true
  else
    echo "(log file missing)" >&2
  fi
  echo "--------------------------------------" >&2
}

tcp_open() {
  local host="$1"
  local port="$2"
  if command -v nc >/dev/null 2>&1; then
    nc -z -w 1 "$host" "$port" >/dev/null 2>&1 && return 0
  fi
  (echo >/dev/tcp/"$host"/"$port") >/dev/null 2>&1
}

http_ready() {
  local port="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -fsS --max-time 0.5 "http://127.0.0.1:${port}/healthz" >/dev/null 2>&1 && return 0
  fi
  tcp_open 127.0.0.1 "$port"
}

# Best-effort port from config.
DASH_PORT=8080
RTMP_PORT=1935
if [[ -f "$CONFIG" ]]; then
  dash_line="$(awk '/^dash:/{s=1;next} s&&/^[^ \t]/{s=0} s&&/port:/{print $2;exit}' "$CONFIG" 2>/dev/null || true)"
  rtmp_line="$(awk '/^rtmp:/{s=1;next} s&&/^[^ \t]/{s=0} s&&/port:/{print $2;exit}' "$CONFIG" 2>/dev/null || true)"
  [[ -n "${dash_line:-}" ]] && DASH_PORT="${dash_line//\"/}"
  [[ -n "${rtmp_line:-}" ]] && RTMP_PORT="${rtmp_line//\"/}"
fi

if [[ -f "$PID_FILE" ]]; then
  old_pid="$(cat "$PID_FILE" 2>/dev/null || true)"
  if [[ -n "${old_pid}" ]] && kill -0 "$old_pid" 2>/dev/null; then
    echo "rtmp2dash already running (pid=${old_pid})"
    exit 0
  fi
  rm -f "$PID_FILE"
fi

if [[ ! -x "$BIN" ]]; then
  echo "error: binary not found: $BIN" >&2
  echo "Run ./install.sh first (Ubuntu source pack)." >&2
  exit 1
fi

if [[ ! -f "$CONFIG" ]]; then
  echo "error: config not found: $CONFIG" >&2
  exit 1
fi

mkdir -p "${ROOT}/cache" "${ROOT}/logs"
: >"$LOG_FILE"

echo "Starting rtmp2dash (config=${CONFIG})..."
nohup "$BIN" --config "$CONFIG" >>"$LOG_FILE" 2>&1 &
pid=$!
echo "$pid" >"$PID_FILE"

ready=false
for _ in $(seq 1 50); do
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "error: process exited during startup" >&2
    dump_log
    rm -f "$PID_FILE"
    exit 1
  fi
  if http_ready "$DASH_PORT"; then
    ready=true
    break
  fi
  sleep 0.2
done

if [[ "$ready" != true ]]; then
  echo "error: HTTP :${DASH_PORT}/healthz not ready after 10s" >&2
  dump_log
  kill "$pid" 2>/dev/null || true
  rm -f "$PID_FILE"
  exit 1
fi

echo "rtmp2dash started (pid=${pid})"
echo "  log: $LOG_FILE"
echo "  publish: rtmp://<host>:${RTMP_PORT}/live/<channel>"
echo "  play:    http://<host>:${DASH_PORT}/live/<channel>/index.mpd"
