#!/usr/bin/env bash
# Input routing for overlapping remote windows: a click on the first local
# window must reach the first app even though the second app mapped on top of
# it on the virtual display (apps like kitty put every top-level at 0,0).
#
# Usage: scripts/e2e-overlap.sh [seconds]
#
# The two anim windows sit at (0,0) on the remote display, 500x400 and
# 400x300. There is no WM on the private client display, so the script raises
# the first window locally (as a WM would on click) and clicks a point that is
# inside both windows. The daemon must raise the remote first window before
# injecting; without that the second window receives the button.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
XCOM=${XCOM:-$REPO/target/release/xcom}
# shellcheck source=scripts/e2e-lib.sh
. "$REPO/scripts/e2e-lib.sh"
SECS=${1:-6}
OUT=/tmp/opencode/e2e/$$
mkdir -p "$OUT"

A_LOG=/tmp/xcom-overlap-a.log
B_LOG=/tmp/xcom-overlap-b.log
SESSION=overlap

e2e_choose_display 0 1600x1000 "$OUT/xvfb.log" || exit 2
cleanup() {
    if [ -n "${CPID:-}" ]; then
        kill "$CPID" 2>/dev/null || true
        wait "$CPID" 2>/dev/null || true
    fi
    "$XCOM" kill --ssh xcom@127.0.0.1 --ssh-args "$E2E_SSH_ARGS" \
        --session "$SESSION" >/dev/null 2>&1 || true
    e2e_stop_xvfb
}
trap cleanup EXIT

e2e_prepare_x11_input
podman exec xcom-testvm rm -f "$A_LOG" "$B_LOG" 2>/dev/null || true

# Sequential mapping keeps overlap-a at the bottom of the remote stack and
# gives each app a distinct title for the local raise.
APP="xcomd m0 anim --title overlap-a --mode box --fps 30 --size 500x400 \
--seconds 20 --input-log $A_LOG & sleep 0.5; \
xcomd m0 anim --title overlap-b --mode box --fps 30 --size 400x300 \
--seconds 20 --input-log $B_LOG & wait"

DISPLAY=$CLIENT_DISPLAY "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$E2E_SSH_ARGS" --session "$SESSION" --replace --exec "$APP" \
    --seconds $((SECS + 6)) >"$OUT/client.log" 2>&1 &
CPID=$!

OPENED=0
for _ in $(seq 1 200); do
    OPENED=$(grep -c 'window .* opened' "$OUT/client.log" || true)
    [ "$OPENED" -ge 2 ] && break
    sleep 0.05
done
sleep 0.5

PASS=1
if [ "$OPENED" -lt 2 ]; then
    echo "--- client log ---"
    tail -20 "$OUT/client.log"
    echo "=== e2e-overlap: FAIL (both windows did not open) ==="
    exit 1
fi

# Raise the first window locally so the click is delivered to it: both local
# windows are at (32,32) and the second is on top until a WM or this test
# raises the first. The click point is inside both windows.
DISPLAY=$CLIENT_DISPLAY "$REPO/target/debug/examples/x11_input" raise overlap-a
DISPLAY=$CLIENT_DISPLAY "$REPO/target/debug/examples/x11_input" move 1200 900
DISPLAY=$CLIENT_DISPLAY "$REPO/target/debug/examples/x11_input" clickat 132 132 1
sleep 0.5

A=$(podman exec xcom-testvm cat "$A_LOG" 2>/dev/null || true)
B=$(podman exec xcom-testvm cat "$B_LOG" 2>/dev/null || true)
echo "--- overlap-a received ---"
echo "$A" | tail -4
echo "--- overlap-b received ---"
echo "$B" | tail -4

grep -q 'button=1 pressed=true' <<<"$A" || PASS=0
if grep -q 'button=' <<<"$B"; then
    echo "the click also reached overlap-b"
    PASS=0
fi

if [ "$PASS" = 1 ]; then
    echo "=== e2e-overlap: PASS ==="
else
    echo "=== e2e-overlap: FAIL ==="
    exit 1
fi
