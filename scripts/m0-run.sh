#!/usr/bin/env bash
# M0 scenario runner: <damage source> -> capture -> sink, prints both sides.
#
#   scripts/m0-run.sh <box|full|glxgears|input> [raw|non-empty] [seconds]
#
# Uses the release build of xcomd against DISPLAY_NUM (default :9).
set -u

XCOMD=${XCOMD:-$HOME/code/xcom/target/release/xcomd}
DISPLAY_NUM=${DISPLAY_NUM:-:9}
MODE=${1:-box}
DAMAGE=${2:-non-empty}
SECS=${3:-8}
FPS=${FPS:-60}
PORT=${PORT:-$((7800 + RANDOM % 200))}

OUT=/tmp/opencode/m0
mkdir -p "$OUT"

if ! DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    echo "error: X display $DISPLAY_NUM is not reachable (start Xvfb first)" >&2
    exit 1
fi
SINK_LOG="$OUT/sink-$MODE-$DAMAGE.log"
SRC_LOG="$OUT/src-$MODE.log"
CAP_LOG="$OUT/cap-$MODE-$DAMAGE.log"
KEY_LOG="$OUT/keys-$MODE.log"

if [ "$MODE" = "input" ]; then
    rm -f "$KEY_LOG"
    "$XCOMD" m0 anim --display "$DISPLAY_NUM" --size 800x600 --fps "$FPS" --mode box \
        --seconds $((SECS + 3)) --input-log "$KEY_LOG" > "$SRC_LOG" 2>&1 &
    SRC_PID=$!
    sleep 1.5
    "$XCOMD" m0 input-test --display "$DISPLAY_NUM" --window auto:first --check-log "$KEY_LOG"
    STATUS=$?
    kill "$SRC_PID" 2>/dev/null
    echo "=== scenario: input-injection (exit $STATUS) ==="
    exit $STATUS
fi

"$XCOMD" m0 sink --listen "127.0.0.1:$PORT" --seconds $((SECS + 5)) > "$SINK_LOG" 2>&1 &
SINK_PID=$!

if [ "$MODE" = "glxgears" ]; then
    DISPLAY="$DISPLAY_NUM" glxgears > "$SRC_LOG" 2>&1 &
elif [ "$MODE" = "firefox" ]; then
    rm -rf "$OUT/ffprofile"; mkdir -p "$OUT/ffprofile"
    DISPLAY="$DISPLAY_NUM" firefox --no-remote --profile "$OUT/ffprofile" \
        --new-instance about:blank > "$SRC_LOG" 2>&1 &
else
    "$XCOMD" m0 anim --display "$DISPLAY_NUM" --size 800x600 --fps "$FPS" --mode "$MODE" \
        --seconds $((SECS + 3)) > "$SRC_LOG" 2>&1 &
fi
SRC_PID=$!

sleep 1.5
"$XCOMD" m0 capture --display "$DISPLAY_NUM" --window auto:first --target "127.0.0.1:$PORT" \
    --seconds "$SECS" --damage "$DAMAGE" > "$CAP_LOG" 2>&1
kill "$SRC_PID" 2>/dev/null
wait "$SINK_PID" 2>/dev/null

echo "=== scenario: mode=$MODE damage=$DAMAGE secs=$SECS ==="
echo "--- capture (server side) ---"
cat "$CAP_LOG"
echo "--- sink (client side) ---"
grep -v '^\[sink\] listening\|connection from' "$SINK_LOG"
