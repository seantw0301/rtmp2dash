#!/usr/bin/env bash
# Restart rtmp2dash: stop then start.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT_DIR="${ROOT}/script"

echo "=== restart: stop ==="
"${SCRIPT_DIR}/stop.sh"

echo "=== restart: start ==="
"${SCRIPT_DIR}/start.sh"
