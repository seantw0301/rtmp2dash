#!/usr/bin/env bash
# 一鍵停止：結束由 start.sh 啟動的 rtmp2dash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PID_FILE="${ROOT}/logs/rtmp2dash.pid"
BIN="${ROOT}/bin/rtmp2dash"

stop_pids() {
  local pids="$1"
  # shellcheck disable=SC2086
  kill ${pids} 2>/dev/null || true
  sleep 0.5
  for p in ${pids}; do
    if kill -0 "$p" 2>/dev/null; then
      kill -9 "$p" 2>/dev/null || true
    fi
  done
}

if [[ ! -f "$PID_FILE" ]]; then
  pids="$(pgrep -f "${BIN}" || true)"
  if [[ -z "${pids}" ]]; then
    echo "rtmp2dash is not running"
    exit 0
  fi
  echo "No pid file; stopping matching process(es): ${pids}"
  stop_pids "$pids"
  echo "rtmp2dash stopped"
  exit 0
fi

pid="$(cat "$PID_FILE")"
if [[ -z "${pid}" ]]; then
  rm -f "$PID_FILE"
  echo "rtmp2dash is not running (empty pid file)"
  exit 0
fi

if ! kill -0 "$pid" 2>/dev/null; then
  rm -f "$PID_FILE"
  echo "rtmp2dash is not running (stale pid=${pid})"
  exit 0
fi

echo "Stopping rtmp2dash (pid=${pid})..."
kill "$pid" 2>/dev/null || true

for _ in 1 2 3 4 5 6 7 8 9 10; do
  if ! kill -0 "$pid" 2>/dev/null; then
    rm -f "$PID_FILE"
    echo "rtmp2dash stopped"
    exit 0
  fi
  sleep 0.2
done

echo "Process did not exit; sending SIGKILL..."
kill -9 "$pid" 2>/dev/null || true
rm -f "$PID_FILE"
echo "rtmp2dash stopped (forced)"
