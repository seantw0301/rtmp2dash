#!/usr/bin/env bash
# Smoke test: start server → push short H.264+AAC RTMP → check DASH → stop.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

START="${ROOT}/script/start.sh"
STOP="${ROOT}/script/stop.sh"
WORKDIR="${ROOT}/script/.smoke"
SAMPLE="${WORKDIR}/sample.mp4"
CHANNEL="smoke"
DASH_URL="http://127.0.0.1:8080/live/${CHANNEL}/index.mpd"
CACHE_DIR="${ROOT}/cache/live/${CHANNEL}"

cleanup() {
  "$STOP" >/dev/null 2>&1 || true
}
trap cleanup EXIT

command -v ffmpeg >/dev/null || { echo "error: ffmpeg is required" >&2; exit 1; }

mkdir -p "$WORKDIR"
rm -rf "$CACHE_DIR"

echo "[1/5] generate sample (6s H.264+AAC)..."
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i testsrc=size=640x360:rate=30 \
  -f lavfi -i sine=frequency=1000:sample_rate=44100 \
  -t 6 -c:v libx264 -pix_fmt yuv420p -profile:v baseline \
  -g 60 -keyint_min 60 -sc_threshold 0 \
  -c:a aac -b:a 128k "$SAMPLE"

echo "[2/5] start rtmp2dash..."
"$START"

echo "[3/5] publish rtmp://127.0.0.1:6136/live/${CHANNEL} ..."
ffmpeg -hide_banner -loglevel error -re -i "$SAMPLE" -c copy -f flv \
  "rtmp://127.0.0.1:6136/live/${CHANNEL}"

echo "[4/5] verify DASH output..."
code="$(curl -sS -o /tmp/rtmp2dash-smoke.mpd -w '%{http_code}' "$DASH_URL" || true)"
if [[ "$code" != "200" ]]; then
  echo "error: MPD HTTP status=${code}" >&2
  exit 1
fi
if ! grep -q 'type="dynamic"' /tmp/rtmp2dash-smoke.mpd; then
  echo "error: MPD missing type=dynamic" >&2
  exit 1
fi
if [[ ! -f "${CACHE_DIR}/init.mp4" ]]; then
  echo "error: init.mp4 missing" >&2
  exit 1
fi
seg_count="$(find "$CACHE_DIR" -name 'seg_*.m4s' | wc -l | tr -d ' ')"
if [[ "$seg_count" -lt 1 ]]; then
  echo "error: no media segments written" >&2
  exit 1
fi

echo "[5/5] ok — mpd=200, segments=${seg_count}"
ls -la "$CACHE_DIR"
