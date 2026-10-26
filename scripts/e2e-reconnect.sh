#!/usr/bin/env bash
# M4 reattach test: the session drops the link on purpose, the client
# reconnects in the background (attach-only), keeps the same app/window, and
# `xcom kill` tears everything down.
#
# Usage: scripts/e2e-reconnect.sh [seconds]
#
# The client runs on a private Xvfb so nothing lands on the developer's desktop.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
XCOM=${XCOM:-$REPO/target/release/xcom}
# shellcheck source=scripts/e2e-lib.sh
. "$REPO/scripts/e2e-lib.sh"
SECS=${1:-10}
KEY=/tmp/opencode/e2e/id_ed25519
OUT=/tmp/opencode/e2e/reconnect-$$
CLIENT_DISPLAY=${PERSIST_DISPLAY:-${CLIENT_DISPLAY:-}}
X11I=$REPO/target/debug/examples/x11_input
mkdir -p "$OUT"

SSH_ARGS="-p 2222 -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
SSH="ssh $SSH_ARGS xcom@127.0.0.1"
APP='echo $$ > /tmp/reconnect.pid; exec xterm -T reconnect-test -e sleep 600'

PASS=1
fail() {
    echo "check: FAIL - $*"
    PASS=0
}
cleanup() {
    set +e
    ${SSH:-true} 'xcomd kill --session default' >/dev/null 2>&1
    e2e_stop_xvfb
}
trap cleanup EXIT

"$REPO/scripts/testvm.sh" up >/dev/null
cargo build -q -p xcom-x11 --example x11_input
$SSH 'xcomd kill --session default' >/dev/null 2>&1 || true
$SSH 'rm -f /tmp/reconnect.pid' >/dev/null 2>&1 || true

e2e_choose_display 0 1280x1024 "$OUT/xvfb.log" || exit 2
DISP=$E2E_DISPLAY

echo "--- client (drops every 3 s, exits after ${SECS}s) ---"
DISPLAY=$DISP "$XCOM" connect --ssh xcom@127.0.0.1 --ssh-args "$SSH_ARGS" \
    --session default --exec "$APP" --drop-after 3 --seconds "$SECS" \
    --dump-ppm "$OUT/frame.ppm" > "$OUT/client.log" 2>&1 || true

CHANNELS=$(grep -c 'secure channel established' "$OUT/client.log" || true)
RECONNECTS=$(grep -c 'reconnected (attempt' "$OUT/client.log" || true)
OPENS=$(grep -c 'window .* opened' "$OUT/client.log" || true)
echo "channels=$CHANNELS reconnects=$RECONNECTS window-opens=$OPENS"
[ "$CHANNELS" -ge 3 ] || fail "expected the initial link plus two reconnects"
[ "$RECONNECTS" -ge 2 ] || fail "reconnect supervisor did not reconnect twice"
[ "$OPENS" -eq 1 ] || fail "the window was not reused across reconnects ($OPENS opens)"
grep -q 'connection lost, keeping' "$OUT/client.log" || fail "no frozen-window banner"
grep -q 'reconnected; waiting for the window resync' "$OUT/client.log" \
    || fail "no resync notice"

PID=$($SSH 'cat /tmp/reconnect.pid 2>/dev/null' | tr -d '\r' || true)
[ -n "$PID" ] || fail "app did not record its pid"
$SSH "kill -0 $PID" 2>/dev/null || fail "app $PID died across reconnects"
LIST=$($SSH 'xcomd sessions' | tr -d '\r')
echo "$LIST"
grep -q 'detached' <<< "$LIST" || fail "session not detached after the client exited"

PPMKIND=${CHECK_PPM:-xterm}
if [ -f "$OUT/frame.ppm" ]; then
    python3 "$REPO/scripts/check-ppm.py" "$OUT/frame.ppm" "$PPMKIND" || fail "reconnected frame is wrong"
fi

echo "--- session gone during reattach: clean exit, no retry loop ---"
DISPLAY=$DISP "$XCOM" connect --ssh xcom@127.0.0.1 --ssh-args "$SSH_ARGS" \
    --session default --exec "$APP" > "$OUT/client-gone.log" 2>&1 &
CPID=$!
for _ in $(seq 1 100); do
    grep -q 'window .* opened' "$OUT/client-gone.log" 2>/dev/null && break
    sleep 0.1
done
OWNER=$($SSH 'xcomd sessions' | awk '{print $5}' | tr -d '\r')
[ -n "$OWNER" ] || fail "session owner pid missing"
# SIGKILL sends no SessionEnded: the client must notice the crash, retry once
# (attach-only) and classify the missing session as permanent.
$SSH "kill -9 $OWNER" 2>/dev/null || fail "could not kill the owner"
ALIVE=1
for _ in $(seq 1 100); do
    if ! kill -0 "$CPID" 2>/dev/null; then
        ALIVE=0
        break
    fi
    sleep 0.1
done
if [ "$ALIVE" = 1 ]; then
    fail "client kept retrying after the session was killed"
    kill "$CPID" 2>/dev/null || true
fi
wait "$CPID" 2>/dev/null || true
CPID=
grep -q 'session is gone' "$OUT/client-gone.log" \
    || fail "client did not report the gone session"
grep -q 'connection lost, keeping' "$OUT/client-gone.log" \
    || fail "client did not notice the link drop"
# Every bootstrap ssh is reaped: the old error path left one zombie per retry.
ZOMBIES=$(ps -eo stat=,args= | awk '$1 ~ /^Z/ && $0 ~ /ssh/ {n++} END {print n+0}')
[ "$ZOMBIES" -eq 0 ] || fail "$ZOMBIES defunct ssh process(es) left behind"

sleep 1
LEFT=$($SSH "kill -0 $PID 2>/dev/null && echo app || true; pgrep -x Xvfb || true; ps -eo args | grep '^xcomd session' || true" | tr -d '\r')
[ -z "$LEFT" ] || fail "left behind: $LEFT"
$SSH 'xcomd sessions' | grep -q 'no sessions' || fail "session still listed after kill"

echo "--- client log (reconnects) ---"
grep -E 'secure channel|connection lost|reconnecting|reconnected|session is gone' \
    "$OUT/client.log" | tail -15
echo "--- client-gone log ---"
tail -5 "$OUT/client-gone.log"
if [ "$PASS" = 1 ]; then
    echo "=== e2e-reconnect: PASS ==="
else
    echo "=== e2e-reconnect: FAIL ==="
    exit 1
fi
