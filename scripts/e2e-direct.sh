#!/usr/bin/env bash
# End-to-end test without ssh: daemon + client on this machine.
#
# The daemon spawns a virtual display with an animated test window; the client
# connects over the dedicated TLS channel (direct mode) and must open a native
# window and blit frames.
#
# Usage: scripts/e2e-direct.sh [seconds]
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
XCOMD=${XCOMD:-$REPO/target/release/xcomd}
XCOM=${XCOM:-$REPO/target/release/xcom}
SECS=${1:-3}
OUT=/tmp/opencode/e2e/$$
mkdir -p "$OUT"   # per-run: parallel runs must not share handshake files

# Xvfb: PATH first, then the user-local install used on this dev box.
if [ -z "${XCOM_XVFB:-}" ]; then
    XCOM_XVFB=$(command -v Xvfb || echo $HOME/.local/xvfb/usr/bin/Xvfb)
fi
export XCOM_XVFB

"$XCOMD" serve --json --spawn 800x600 --listen 127.0.0.1:0 \
    --exec "$XCOMD m0 anim --mode box --fps 60 --seconds $((SECS + 8))" \
    > "$OUT/handshake.json" 2> "$OUT/serve.log" &
SERVE_PID=$!
trap 'kill "$SERVE_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
    [ -s "$OUT/handshake.json" ] && break
    sleep 0.1
done
LINE=$(grep "^{" "$OUT/handshake.json" 2>/dev/null | head -1 || true)
if [ -z "$LINE" ]; then
    echo "error: daemon never printed its handshake line" >&2
    cat "$OUT/serve.log" >&2
    exit 1
fi
echo "handshake: $LINE"

ADDR=$(sed -n 's/.*"connect":"\([^"]*\)".*/\1/p' <<< "$LINE")
TOKEN=$(sed -n 's/.*"token":"\([^"]*\)".*/\1/p' <<< "$LINE")
FP=$(sed -n 's/.*"cert_sha256":"\([^"]*\)".*/\1/p' <<< "$LINE")

DISPLAY=${CLIENT_DISPLAY:-:0} "$XCOM" connect --direct "$ADDR" --token "$TOKEN" \
    --fingerprint "$FP" --seconds "$SECS" --dump-ppm "$OUT/frame.ppm" \
    > "$OUT/client.log" 2>&1 || true

echo "--- client ---"
cat "$OUT/client.log"
echo "--- server ---"
grep -vE '^\[serve\] (waiting|client)' "$OUT/serve.log" | tail -8

PASS=1
grep -qE '[1-9][0-9]* frames blitted' "$OUT/client.log" || PASS=0
grep -q 'window .* opened' "$OUT/client.log" || PASS=0
python3 "$REPO/scripts/check-ppm.py" "$OUT/frame.ppm" box || PASS=0
if [ "$PASS" = 1 ]; then
    echo "=== e2e-direct: PASS ==="
else
    echo "=== e2e-direct: FAIL ==="
    exit 1
fi
