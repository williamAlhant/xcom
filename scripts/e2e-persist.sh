#!/usr/bin/env bash
# M3 reattach test: the session and app survive the client, a second client
# resumes the same display and app, the last-window dialog can detach, a
# dropped client releases held keys, and `xcom kill` tears everything down.
#
# Usage: scripts/e2e-persist.sh [seconds]
#
# The clients run on a private Xvfb: focus injection must not fight with the
# developer's desktop (an XTEST Enter would go to whatever window has focus).
# Needs the xcom test VM (scripts/testvm.sh).
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
XCOM=${XCOM:-$REPO/target/release/xcom}
SECS=${1:-4}
KEY=/tmp/opencode/e2e/id_ed25519
OUT=/tmp/opencode/e2e/persist-$$
DISP=${PERSIST_DISPLAY:-:98}
X11I=$REPO/target/debug/examples/x11_input
mkdir -p "$OUT"

SSH_ARGS="-p 2222 -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
SSH="ssh $SSH_ARGS xcom@127.0.0.1"
APP='echo $$ > /tmp/persist.pid; exec xterm -T persist-test -e sleep 600'

PASS=1
STARTED_XVFB=0
CPID=
XPID=
fail() {
    echo "check: FAIL - $*"
    PASS=0
}
cleanup() {
    set +e
    [ -n "$CPID" ] && kill "$CPID" 2>/dev/null
    [ "$STARTED_XVFB" = 1 ] && [ -n "$XPID" ] && kill "$XPID" 2>/dev/null
    ${SSH:-true} 'xcomd kill --session persist' >/dev/null 2>&1
}
trap cleanup EXIT

"$REPO/scripts/testvm.sh" up >/dev/null
cargo build -q -p xcom-x11 --example x11_input
podman cp "$X11I" xcom-testvm:/usr/local/bin/x11_input
$SSH 'xcomd kill --session persist' >/dev/null 2>&1 || true
$SSH 'rm -f /tmp/persist.pid' >/dev/null 2>&1 || true

# A private display for the clients; reuse one only if it already exists.
if ! DISPLAY=$DISP "$X11I" pointer >/dev/null 2>&1; then
    XVFB=${XCOM_XVFB:-$(command -v Xvfb || echo "$HOME/.local/xvfb/usr/bin/Xvfb")}
    "$XVFB" "$DISP" -screen 0 1280x1024x24 -nolisten tcp +extension XTEST \
        > "$OUT/xvfb.log" 2>&1 &
    XPID=$!
    STARTED_XVFB=1
    sleep 1
fi

client() {
    local log=$1
    shift
    DISPLAY=$DISP "$XCOM" connect --ssh xcom@127.0.0.1 \
        --ssh-args "$SSH_ARGS" --session persist --exec "$APP" "$@" \
        > "$log" 2>&1 || true
}

echo "--- first attach (creates the session) ---"
client "$OUT/client1.log" --seconds "$SECS" --dump-ppm "$OUT/first.ppm"
grep -qE '[1-9][0-9]* frames blitted' "$OUT/client1.log" || fail "first client got no frames"
grep -q 'window .* opened' "$OUT/client1.log" || fail "first client opened no window"

PID1=$($SSH 'cat /tmp/persist.pid' 2>/dev/null | tr -d '\r' || true)
[ -n "$PID1" ] || fail "app did not record its pid"
sleep 1
LIST1=$($SSH 'xcomd sessions' | tr -d '\r')
echo "$LIST1"
grep -q 'persist' <<< "$LIST1" || fail "session missing after detach"
grep -q 'detached' <<< "$LIST1" || fail "session still marked attached after the client left"
D1=$(awk '{print $3}' <<< "$LIST1")
$SSH "kill -0 $PID1" 2>/dev/null || fail "app $PID1 died with the client"

echo "--- second attach (same session) ---"
client "$OUT/client2.log" --seconds "$SECS" --dump-ppm "$OUT/second.ppm"
grep -qE '[1-9][0-9]* frames blitted' "$OUT/client2.log" || fail "reattached client got no frames"
grep -q 'session app:' "$OUT/client2.log" || fail "reattach did not report the running app"
PID2=$($SSH 'cat /tmp/persist.pid' 2>/dev/null | tr -d '\r' || true)
LIST2=$($SSH 'xcomd sessions' | tr -d '\r')
D2=$(awk '{print $3}' <<< "$LIST2")
[ "$PID1" = "$PID2" ] || fail "app restarted on reattach ($PID1 -> $PID2)"
[ "$D1" = "$D2" ] || fail "display changed on reattach ($D1 -> $D2)"

echo "--- last-window dialog: disconnect ---"
DISPLAY=$DISP "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --session persist --exec "$APP" \
    > "$OUT/client3.log" 2>&1 &
CPID=$!
for _ in $(seq 1 100); do
    grep -q 'window .* opened' "$OUT/client3.log" 2>/dev/null && break
    sleep 0.1
done
DISPLAY=$DISP "$X11I" close persist-test || fail "close helper found no window"
for _ in $(seq 1 60); do
    grep -q 'close requested for the last window: asking' "$OUT/client3.log" 2>/dev/null && break
    sleep 0.05
done
grep -q 'close requested for the last window: asking' "$OUT/client3.log" \
    || fail "close request did not open the dialog"
# Enter activates the focused button (Disconnect). Wait for the focus event,
# then retry: a synthetic focus can race the client's event loop.
DISPLAY=$DISP "$X11I" focus persist-test || fail "could not focus the client window"
sleep 0.3
for _ in 1 2 3; do
    kill -0 "$CPID" 2>/dev/null || break
    DISPLAY=$DISP "$X11I" keys 36 || true
    sleep 0.3
done
wait "$CPID" || true
CPID=
grep -q 'close dialog: disconnect' "$OUT/client3.log" || fail "dialog did not disconnect"
sleep 1
LIST3=$($SSH 'xcomd sessions' | tr -d '\r')
grep -q 'detached' <<< "$LIST3" || fail "session did not stay up after the dialog disconnect"
$SSH "kill -0 $PID1" 2>/dev/null || fail "app died after the dialog disconnect"

echo "--- held key released when the client dies ---"
DISPLAY=$DISP "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --session persist --exec "$APP" \
    > "$OUT/client4.log" 2>&1 &
CPID=$!
for _ in $(seq 1 100); do
    grep -q 'window .* opened' "$OUT/client4.log" 2>/dev/null && break
    sleep 0.1
done
DISPLAY=$DISP "$X11I" focus persist-test || fail "could not focus client4"
sleep 0.3
DISPLAY=$DISP "$X11I" down 38 # physical 'A'
sleep 0.4
kill -9 "$CPID" 2>/dev/null || true
wait "$CPID" 2>/dev/null || true
CPID=
sleep 1
RDISP=$($SSH 'xcomd sessions' | awk '{print $3}' | tr -d '\r')
if podman exec xcom-testvm sh -c \
    "A=\$(ls -t /tmp/xcom-xauth-* 2>/dev/null | head -1); DISPLAY=$RDISP XAUTHORITY=\$A /usr/local/bin/x11_input keystate 38"; then
    echo "check-keyrelease: OK (detach released the held key)"
else
    fail "key 38 stayed down on the remote display"
fi

echo "--- last-window dialog: close the app ---"
DISPLAY=$DISP "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --session persist --exec "$APP" \
    > "$OUT/client5.log" 2>&1 &
CPID=$!
for _ in $(seq 1 100); do
    grep -q 'window .* opened' "$OUT/client5.log" 2>/dev/null && break
    sleep 0.1
done
DISPLAY=$DISP "$X11I" close persist-test || fail "close helper found no window"
for _ in $(seq 1 60); do
    grep -q 'close requested for the last window: asking' "$OUT/client5.log" 2>/dev/null && break
    sleep 0.05
done
grep -q 'close requested for the last window: asking' "$OUT/client5.log" \
    || fail "close request did not open the dialog"
# Click "Close window" (the left button): mouse hit-testing is deterministic
# and does not depend on the synthetic keyboard focus.
WIN=$(DISPLAY=$DISP "$X11I" list | grep persist-test | head -1)
GEOM=$(awk '{print $2}' <<< "$WIN")
WX=${GEOM%%+*}
REST=${GEOM#*+}
WY=${REST%%+*}
DISPLAY=$DISP "$X11I" clickat $((WX + 91)) $((WY + 199)) \
    || fail "could not click the dialog button"
for _ in $(seq 1 100); do
    kill -0 "$CPID" 2>/dev/null || break
    sleep 0.1
done
wait "$CPID" 2>/dev/null || true
CPID=
grep -q 'close dialog: closing the window' "$OUT/client5.log" \
    || fail "dialog did not choose Close"
grep -qE 'session ended' "$OUT/client5.log" \
    || fail "client did not report the session ending"
if grep -q 'reconnecting' "$OUT/client5.log"; then
    fail "client tried to reconnect after deliberately ending the session"
fi
if grep -q 'session is gone' "$OUT/client5.log"; then
    fail "client reported a gone session after the app was closed"
fi

echo "--- teardown ---"
sleep 1
LEFT=$($SSH "kill -0 $PID1 2>/dev/null && echo app || true; pgrep -x Xvfb || true; ps -eo args | grep '^xcomd session' || true" | tr -d '\r')
[ -z "$LEFT" ] || fail "left behind after closing the app: $LEFT"
$SSH 'xcomd sessions' | grep -q 'no sessions' || fail "session still listed after the app closed"

echo "--- client 1 ---"; tail -4 "$OUT/client1.log"
echo "--- client 2 ---"; tail -4 "$OUT/client2.log"
echo "--- client 3 ---"; tail -4 "$OUT/client3.log"
if [ "$PASS" = 1 ]; then
    echo "=== e2e-persist: PASS ==="
else
    echo "=== e2e-persist: FAIL ==="
    exit 1
fi
