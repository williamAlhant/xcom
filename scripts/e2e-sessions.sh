#!/usr/bin/env bash
# Session lifecycle: listing, busy rejection, --replace, idle timeout, stale
# socket recovery, kill, socket permissions and name validation.
#
# Usage: scripts/e2e-sessions.sh
#
# Uses the xcom test VM; clients run on a private Xvfb.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
XCOM=${XCOM:-$REPO/target/release/xcom}
KEY=/tmp/opencode/e2e/id_ed25519
OUT=/tmp/opencode/e2e/sessions-$$
DISP=${PERSIST_DISPLAY:-:98}
mkdir -p "$OUT"

SSH_ARGS="-p 2222 -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
SSH="ssh $SSH_ARGS xcom@127.0.0.1"
APP='echo $$ > /tmp/sessions.pid; exec xterm -T sessions-test -e sleep 600'
APP2='echo $$ > /tmp/idle.pid; exec xterm -T idle-test -e sleep 60'

PASS=1
STARTED_XVFB=0
XPID=
CPID=
fail() {
    echo "check: FAIL - $*"
    PASS=0
}
cleanup() {
    set +e
    [ -n "$CPID" ] && kill "$CPID" 2>/dev/null
    [ "$STARTED_XVFB" = 1 ] && [ -n "$XPID" ] && kill "$XPID" 2>/dev/null
    ${SSH:-true} 'xcomd kill --session e2e; xcomd kill --session idle' >/dev/null 2>&1
}
trap cleanup EXIT

wait_window() {
    local log=$1
    for _ in $(seq 1 100); do
        grep -q 'window .* opened' "$log" 2>/dev/null && return 0
        sleep 0.1
    done
    fail "no window opened ($log)"
    return 1
}

"$REPO/scripts/testvm.sh" up >/dev/null
$SSH 'xcomd kill --session e2e; xcomd kill --session idle' >/dev/null 2>&1 || true
$SSH 'rm -f /tmp/sessions.pid /tmp/idle.pid' >/dev/null 2>&1 || true

if [ ! -S "/tmp/.X11-unix/X${DISP#:}" ]; then
    XVFB=${XCOM_XVFB:-$(command -v Xvfb || echo "$HOME/.local/xvfb/usr/bin/Xvfb")}
    "$XVFB" "$DISP" -screen 0 1024x768x24 -nolisten tcp +extension XTEST \
        > "$OUT/xvfb.log" 2>&1 &
    XPID=$!
    STARTED_XVFB=1
    sleep 1
fi

echo "--- create + list + permissions ---"
DISPLAY=$DISP "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --session e2e --exec "$APP" \
    > "$OUT/client1.log" 2>&1 &
CPID=$!
wait_window "$OUT/client1.log" || true
LIST=$($SSH 'xcomd sessions' | tr -d '\r')
echo "$LIST"
grep -q '^e2e' <<< "$LIST" || fail "session not listed"
grep -q 'attached' <<< "$LIST" || fail "session not marked attached"
OWNER1=$(awk '/^e2e/{print $5}' <<< "$LIST")
APP_PID1=$($SSH 'cat /tmp/sessions.pid 2>/dev/null' | tr -d '\r' || true)
[ -n "$OWNER1" ] || fail "owner pid missing"
[ -n "$APP_PID1" ] || fail "app pid missing"
SOCK=$($SSH 'find /tmp /run/user -name e2e.sock 2>/dev/null | head -1' | tr -d '\r')
[ -n "$SOCK" ] || fail "session socket missing"
DIR=$(dirname "$SOCK")
PERM_DIR=$($SSH "stat -c '%a' '$DIR'" | tr -d '\r')
PERM_SOCK=$($SSH "stat -c '%a' '$SOCK'" | tr -d '\r')
[ "$PERM_DIR" = "700" ] || fail "socket directory mode $PERM_DIR (want 700)"
[ "$PERM_SOCK" = "600" ] || fail "socket mode $PERM_SOCK (want 600)"

echo "--- second client while attached ---"
DISPLAY=$DISP timeout 15 "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --session e2e --exec "$APP" --seconds 3 \
    > "$OUT/client2.log" 2>&1 || true
grep -q 'session busy' "$OUT/client2.log" || fail "second client was not rejected as busy"
if grep -q 'window .* opened' "$OUT/client2.log"; then
    fail "busy client still opened a window"
fi
kill -0 "$CPID" 2>/dev/null || fail "the first client died when the second was rejected"

echo "--- client sessions wrapper ---"
"$XCOM" sessions --ssh xcom@127.0.0.1 --ssh-args "$SSH_ARGS" \
    > "$OUT/sessions.out" 2>&1 || fail "xcom sessions failed"
grep -q 'e2e' "$OUT/sessions.out" || fail "xcom sessions does not list e2e"

echo "--- idle timeout ---"
DISPLAY=$DISP timeout 20 "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --session idle --exec "$APP2" \
    --idle-timeout 2 --seconds 2 > "$OUT/idle.log" 2>&1 || true
IDLE_PID=$($SSH 'cat /tmp/idle.pid 2>/dev/null' | tr -d '\r' || true)
sleep 4
if $SSH 'xcomd sessions' | grep -q '^idle'; then
    fail "idle session still exists"
fi
if [ -n "$IDLE_PID" ] && $SSH "kill -0 $IDLE_PID" 2>/dev/null; then
    fail "idle app is still alive"
fi

echo "--- replace (while attached) ---"
DISPLAY=$DISP timeout 25 "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --session e2e --exec "$APP" \
    --replace --seconds 4 > "$OUT/client3.log" 2>&1 &
REPLACE_PID=$!
for _ in $(seq 1 100); do
    kill -0 "$CPID" 2>/dev/null || break
    sleep 0.1
done
wait "$CPID" 2>/dev/null || true
CPID=
grep -q 'session ended' "$OUT/client1.log" || fail "old client did not end cleanly on replace"
wait_window "$OUT/client3.log" || true
wait "$REPLACE_PID" 2>/dev/null || true
LIST=$($SSH 'xcomd sessions' | tr -d '\r')
OWNER2=$(awk '/^e2e/{print $5}' <<< "$LIST")
APP_PID2=$($SSH 'cat /tmp/sessions.pid 2>/dev/null' | tr -d '\r' || true)
[ -n "$OWNER2" ] || fail "replacement session not listed"
[ "$OWNER1" != "$OWNER2" ] || fail "owner did not change on --replace"
[ -n "$APP_PID2" ] && [ "$APP_PID1" != "$APP_PID2" ] || fail "app did not restart on --replace"
if $SSH "kill -0 $APP_PID1" 2>/dev/null; then
    fail "old app survived --replace"
fi

echo "--- stale socket after owner SIGKILL ---"
$SSH "kill -9 $OWNER2" 2>/dev/null || fail "could not kill the owner"
sleep 0.5
$SSH "test -S '$SOCK'" || fail "stale socket disappeared"
if $SSH "kill -0 $APP_PID2" 2>/dev/null; then
    fail "app survived its owner (PDEATHSIG broken)"
fi
DISPLAY=$DISP timeout 20 "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --session e2e --exec "$APP" --seconds 2 \
    > "$OUT/client4.log" 2>&1 || true
wait_window "$OUT/client4.log" || true
LIST=$($SSH 'xcomd sessions' | tr -d '\r')
OWNER3=$(awk '/^e2e/{print $5}' <<< "$LIST")
[ -n "$OWNER3" ] || fail "recreated session not listed"
[ "$OWNER3" != "$OWNER2" ] || fail "stale socket was not replaced by a new owner"

echo "--- kill while detached ---"
APP_PID3=$($SSH 'cat /tmp/sessions.pid 2>/dev/null' | tr -d '\r' || true)
"$XCOM" kill --ssh xcom@127.0.0.1 --ssh-args "$SSH_ARGS" --session e2e \
    >/dev/null 2>&1 || fail "xcom kill failed"
sleep 1
if $SSH 'xcomd sessions' | grep -q '^e2e'; then
    fail "session still listed after kill"
fi
if [ -n "$APP_PID3" ] && $SSH "kill -0 $APP_PID3" 2>/dev/null; then
    fail "app still alive after kill"
fi
if $SSH 'pgrep -x Xvfb' >/dev/null 2>&1; then
    fail "Xvfb left behind after kill"
fi

echo "--- name validation ---"
if $SSH 'xcomd attach --name ../evil --json' > /dev/null 2> "$OUT/name.err"; then
    fail "invalid session name was accepted"
fi
grep -q 'session name' "$OUT/name.err" || fail "invalid-name error is unclear"

echo "--- client 1 ---"; tail -3 "$OUT/client1.log"
echo "--- client 3 ---"; tail -3 "$OUT/client3.log"
if [ "$PASS" = 1 ]; then
    echo "=== e2e-sessions: PASS ==="
else
    echo "=== e2e-sessions: FAIL ==="
    exit 1
fi
