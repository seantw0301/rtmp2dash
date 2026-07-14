#!/usr/bin/env bash
# Start rtmp2dash in the background (release build).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PID_FILE="${ROOT}/script/rtmp2dash.pid"
LOG_FILE="${ROOT}/script/rtmp2dash.log"
CONFIG="${CONFIG:-${ROOT}/config.yaml}"

# Resolve where cargo actually writes artifacts. When this crate is a member of a
# parent workspace (or CARGO_TARGET_DIR is set), binaries are NOT under
# ${ROOT}/target/release/.
resolve_release_bin() {
  local name="${1:-rtmp2dash}"
  local target_dir=""
  if command -v cargo >/dev/null 2>&1; then
    target_dir="$(
      cargo metadata --format-version=1 --no-deps --manifest-path "${ROOT}/Cargo.toml" 2>/dev/null \
        | python3 -c 'import json,sys; print(json.load(sys.stdin).get("target_directory",""))' 2>/dev/null \
        || true
    )"
  fi
  if [[ -z "$target_dir" ]]; then
    target_dir="${CARGO_TARGET_DIR:-${ROOT}/target}"
  fi
  echo "${target_dir}/release/${name}"
}

# Prefer ./bin from ./install.sh when present; else cargo release artifact.
resolve_bin() {
  if [[ -x "${ROOT}/bin/rtmp2dash" ]]; then
    echo "${ROOT}/bin/rtmp2dash"
    return
  fi
  resolve_release_bin rtmp2dash
}

# Read dash/rtmp ports from YAML (falls back to defaults).
read_ports() {
  local dash_port=8080
  local rtmp_port=6136
  if [[ -f "$CONFIG" ]] && command -v python3 >/dev/null 2>&1; then
    eval "$(
      python3 - "$CONFIG" <<'PY'
import sys
path = sys.argv[1]
dash, rtmp = 8080, 6136
try:
    import yaml  # type: ignore
    with open(path) as f:
        doc = yaml.safe_load(f) or {}
    dash = int(((doc.get("dash") or {}).get("port")) or 8080)
    rtmp = int(((doc.get("rtmp") or {}).get("port")) or 6136)
except Exception:
    text = open(path).read().splitlines()
    section = None
    for line in text:
        s = line.strip()
        if s.startswith("dash:"):
            section = "dash"
            continue
        if s.startswith("rtmp:"):
            section = "rtmp"
            continue
        if s and not s.startswith("#") and not line.startswith(" ") and not line.startswith("\t") and s.endswith(":"):
            section = None
            continue
        if section in ("dash", "rtmp") and s.startswith("port:"):
            try:
                val = int(s.split(":", 1)[1].strip().strip('"').strip("'"))
            except Exception:
                continue
            if section == "dash":
                dash = val
            else:
                rtmp = val
print(f"dash_port={dash}")
print(f"rtmp_port={rtmp}")
PY
    )"
  fi
  DASH_PORT="$dash_port"
  RTMP_PORT="$rtmp_port"
}

# TCP connect check without requiring nc (Ubuntu images often lack it).
tcp_open() {
  local host="$1"
  local port="$2"
  if command -v nc >/dev/null 2>&1; then
    nc -z -w 1 "$host" "$port" >/dev/null 2>&1 && return 0
  fi
  # bash /dev/tcp
  (echo >/dev/tcp/"$host"/"$port") >/dev/null 2>&1
}

http_ready() {
  local port="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -fsS --max-time 0.5 "http://127.0.0.1:${port}/healthz" >/dev/null 2>&1 && return 0
  fi
  # Fallback: port open is enough for readiness when curl is missing.
  tcp_open 127.0.0.1 "$port"
}

dump_log() {
  echo "---- last 80 lines of ${LOG_FILE} ----" >&2
  if [[ -f "$LOG_FILE" ]]; then
    tail -n 80 "$LOG_FILE" >&2 || true
  else
    echo "(log file missing)" >&2
  fi
  echo "--------------------------------------" >&2
}

BIN="$(resolve_bin)"
read_ports

if [[ -f "$PID_FILE" ]]; then
  old_pid="$(cat "$PID_FILE" 2>/dev/null || true)"
  if [[ -n "${old_pid}" ]] && kill -0 "$old_pid" 2>/dev/null; then
    echo "rtmp2dash already running (pid=${old_pid})"
    exit 0
  fi
  rm -f "$PID_FILE"
fi

# Build only when using cargo artifact (not ./bin from install.sh).
if [[ "$BIN" != "${ROOT}/bin/rtmp2dash" ]]; then
  echo "Building release binary..."
  cargo build --release --manifest-path "${ROOT}/Cargo.toml"
  BIN="$(resolve_bin)"
fi

if [[ ! -x "$BIN" ]]; then
  echo "error: binary not found or not executable: $BIN" >&2
  echo "  Prefer: ./install.sh   # creates ./bin/rtmp2dash" >&2
  echo "  CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-<unset>}" >&2
  exit 1
fi

if [[ ! -f "$CONFIG" ]]; then
  echo "error: config not found: $CONFIG" >&2
  exit 1
fi

# Warn when parent workspace steals target/ (common if all.tar.gz extracted under /home).
if [[ "$BIN" == /home/target/* ]] || [[ "$BIN" != "${ROOT}/"* && "$BIN" != "${ROOT}/bin/"* ]]; then
  echo "warning: binary is outside crate dir: $BIN" >&2
  echo "  This usually means a parent Cargo.toml workspace owns target/." >&2
  echo "  Prefer standalone src pack + ./install.sh, or ensure Cargo.toml has [workspace]." >&2
fi

mkdir -p "${ROOT}/cache" "$(dirname "$LOG_FILE")"

# Avoid double-bind: stop leftover listeners if any.
if http_ready "$DASH_PORT" || tcp_open 127.0.0.1 "$RTMP_PORT"; then
  echo "warning: port ${DASH_PORT} and/or ${RTMP_PORT} already in use; start may fail" >&2
  ss -lntp 2>/dev/null | grep -E ":${DASH_PORT}|:${RTMP_PORT}" >&2 || true
fi

: >"$LOG_FILE"
echo "Starting rtmp2dash (config=${CONFIG}, bin=${BIN}, dash=${DASH_PORT}, rtmp=${RTMP_PORT})..."
nohup "$BIN" --config "$CONFIG" >>"$LOG_FILE" 2>&1 &
pid=$!
echo "$pid" >"$PID_FILE"

# Wait for HTTP health (primary). RTMP is secondary — do not require `nc`.
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
  if kill -0 "$pid" 2>/dev/null; then
    echo "hint: process still running (pid=${pid}); check ports / config / privileges" >&2
    kill "$pid" 2>/dev/null || true
  fi
  rm -f "$PID_FILE"
  exit 1
fi

rtmp_ok="no"
if tcp_open 127.0.0.1 "$RTMP_PORT"; then
  rtmp_ok="yes"
else
  echo "warning: RTMP :${RTMP_PORT} not listening yet (HTTP is up)" >&2
fi

echo "rtmp2dash started (pid=${pid}, http=yes, rtmp=${rtmp_ok})"
echo "  log: $LOG_FILE"
echo "  publish: rtmp://<host>:${RTMP_PORT}/live/<channel>"
echo "  play:    http://<host>:${DASH_PORT}/live/<channel>/index.mpd"
