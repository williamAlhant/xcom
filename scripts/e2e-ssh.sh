#!/usr/bin/env bash
# End-to-end test through a real ssh session: the client bootstraps via ssh
# against the test VM container, then streams over the dedicated TLS channel.
#
# This exercises the real security model: the handshake line (token +
# certificate fingerprint) travels over ssh, everything after is direct.
#
# Usage: scripts/e2e-ssh.sh [seconds]
#
# Environment:
#   APP          remote command (default: xterm)
#   TYPE_TEXT    text typed into the session after TYPE_DELAY seconds
#   RESIZE_TO    WxH: resize the window after RESIZE_AFTER seconds
#   CHECK_BOX=1  assert the frame is the anim's box pattern
#   CHECK_KEYS=1 type into the anim and assert the app received the keysyms
#   CHECK_RESIZE=1  assert one user resize does not start a resize ping-pong
#   CHECK_CLOSE=1   close one of two windows; the session must keep the other
#   CHECK_UNICODE=1 type non-ASCII text and compare the exact UTF-8 bytes
#   CHECK_WM=1      normal window + transient dialog + override-redirect menu
#   CHECK_TEARDOWN=0  skip the no-zombie check after the client exits
#   CLOSE_AFTER=N   seconds after start to close one window (test aid)
#   XCOM_DEBUG=1 protocol/input tracing (client and daemon)
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
XCOM=${XCOM:-$REPO/target/release/xcom}
SECS=${1:-3}
APP=${APP:-"xterm -geometry 60x20"}
TYPE_TEXT=${TYPE_TEXT:-}
TYPE_DELAY=${TYPE_DELAY:-1}   # keep well inside the session
KEY=/tmp/opencode/e2e/id_ed25519
OUT=/tmp/opencode/e2e/$$
mkdir -p "$OUT"   # per-run: parallel runs must not share handshake files

# CHECK_KEYS=1: the anim logs received keysyms, so typed text can be verified
# at the application end rather than just at the transport.
if [ "${CHECK_KEYS:-0}" = 1 ]; then
    APP=${APP_ANIM:-"xcomd m0 anim --mode box --fps 60 --seconds 60 --input-log /tmp/xcom-keys.log"}
    TYPE_TEXT=${TYPE_TEXT:-hello}
    podman exec xcom-testvm rm -f /tmp/xcom-keys.log 2>/dev/null || true
fi

if [ "${CHECK_UNICODE:-0}" = 1 ]; then
    APP="xterm -T xterm -geometry 80x24 -e sh -c 'stty raw -echo; cat > /tmp/unicode.txt'"
    TYPE_TEXT=${UNICODE_TEXT:-éàéàéà}
    podman exec xcom-testvm rm -f /tmp/unicode.txt 2>/dev/null || true
fi

if [ "${CHECK_WM:-0}" = 1 ]; then
    APP="xcomd m0 wm-demo --seconds 20"
fi

"$REPO/scripts/testvm.sh" up

SSH_ARGS="-p 2222 -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

TYPE_ARGS=""
if [ -n "$TYPE_TEXT" ]; then
    TYPE_ARGS="--type-text $TYPE_TEXT --type-delay $TYPE_DELAY"
fi
RESIZE_ARGS=""
if [ -n "${RESIZE_TO:-}" ]; then
    RESIZE_ARGS="--resize-to $RESIZE_TO --resize-after ${RESIZE_AFTER:-2}"
fi
if [ -n "${DUMP_AFTER:-}" ] && [ "${DUMP_AFTER}" != 0 ]; then
    RESIZE_ARGS="$RESIZE_ARGS --dump-after $DUMP_AFTER"
fi

DISPLAY=${CLIENT_DISPLAY:-:0} XCOM_DEBUG=${XCOM_DEBUG:-0} "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --exec "$APP" \
    --seconds "$SECS" --dump-ppm "$OUT/frame-ssh.ppm" $TYPE_ARGS $RESIZE_ARGS \
    ${CLOSE_AFTER:+--close-after "$CLOSE_AFTER"} \
    > "$OUT/client-ssh.log" 2>&1 || true

# The session outlives the client by design; end it before the teardown check.
# This also exercises the client's `kill` subcommand over ssh.
"$XCOM" kill --ssh xcom@127.0.0.1 --ssh-args "$SSH_ARGS" --session default \
    >/dev/null 2>&1 || true
sleep 1

echo "--- client (app: $APP) ---"
grep -vE '^\[serve\] (waiting|client)' "$OUT/client-ssh.log" | tail -12

PASS=1
grep -qE '[1-9][0-9]* frames blitted' "$OUT/client-ssh.log" || PASS=0
grep -q 'window .* opened' "$OUT/client-ssh.log" || PASS=0
# Display sizing: the client requests a size and the server reports the result.
grep -qE '\[xcom\] requesting a [0-9]+x[0-9]+ session display' "$OUT/client-ssh.log" || PASS=0
grep -qE '\[xcom\] session display [0-9]+x[0-9]+' "$OUT/client-ssh.log" || PASS=0

if [ "${CHECK_BOX:-0}" = 1 ]; then
    python3 "$REPO/scripts/check-ppm.py" "$OUT/frame-ssh.ppm" box || PASS=0
fi

if [ "${CHECK_RESIZE:-0}" = 1 ]; then
    echo "--- resize dance check ---"
    RESIZES=$(grep -c 'recv resize' "$OUT/client-ssh.log" || true)
    GEOMS=$(grep -c '\[ui\] geometry' "$OUT/client-ssh.log" || true)
    NRESIZED=$(grep -c '\[ui\] resized' "$OUT/client-ssh.log" || true)
    echo "resized events: $NRESIZED, resize messages: $RESIZES, geometry acks: $GEOMS"
    if [ "$NRESIZED" -le 1 ]; then
        # The compositor ignored our programmatic resize (allowed on Wayland);
        # the dance check only bites when a real resize happened.
        echo "check-resize: SKIPPED (compositor ignored request_inner_size)"
    elif [ "$RESIZES" -gt 3 ]; then
        echo "check-resize: FAIL - ping-pong ($RESIZES resizes for one action)"
        PASS=0
    else
        echo "check-resize: OK (no ping-pong)"
    fi
    python3 "$REPO/scripts/check-ppm.py" "$OUT/frame-ssh.ppm" xterm || PASS=0
fi

if [ "${CHECK_CLOSE:-0}" = 1 ]; then
    echo "--- per-window close check ---"
    grep -q 'test aid: closing window' "$OUT/client-ssh.log" || PASS=0
    grep -q 'window .* closed' "$OUT/client-ssh.log" || PASS=0
    if grep -q '1 window(s) open' "$OUT/client-ssh.log"; then
        echo "check-close: OK (one window closed, the other stayed up)"
    else
        echo "check-close: FAIL (the session did not survive the close)"
        PASS=0
    fi
fi

if [ "${CHECK_WM:-0}" = 1 ]; then
    echo "--- mini-WM check ---"
    # Only the client's "opened" lines describe the local windows; with
    # XCOM_DEBUG the daemon also logs kind=/parent= for the same windows.
    OPENED=$(grep -c 'window .* opened' "$OUT/client-ssh.log" || true)
    KINDS=$(grep 'window .* opened' "$OUT/client-ssh.log" | grep -o 'kind=[0-9]*' | tr '\n' ' ')
    DIALOG=$(grep 'window .* opened' "$OUT/client-ssh.log" | grep 'kind=1' | head -1)
    MENU=$(grep 'window .* opened' "$OUT/client-ssh.log" | grep 'kind=2' | head -1)
    echo "opened=$OPENED kinds=[$KINDS]"
    echo "dialog: $DIALOG"
    echo "menu:   $MENU"
    MOVED=$(grep -c '\[ui\] moved' "$OUT/client-ssh.log" || true)
    echo "popup moves followed: $MOVED"
    if [ "$OPENED" -eq 3 ] \
        && echo "$DIALOG" | grep -q 'parent=0x[1-9a-f]' \
        && echo "$MENU" | grep -q 'override=true' \
        && [ "$MOVED" -ge 1 ]; then
        echo "check-wm: OK (3 windows, dialog parent, override menu, move followed)"
    else
        echo "check-wm: FAIL"
        PASS=0
    fi
fi

if [ "${CHECK_UNICODE:-0}" = 1 ]; then
    echo "--- unicode check ---"
    GOT=$(podman exec xcom-testvm od -An -tx1 -v /tmp/unicode.txt 2>/dev/null | tr -d ' \n')
    EXPECT=$(printf '%s' "$TYPE_TEXT" | od -An -tx1 -v | tr -d ' \n')
    echo "got:      $GOT"
    echo "expected: $EXPECT"
    if [ -n "$GOT" ] && [ "$GOT" = "$EXPECT" ]; then
        echo "check-unicode: OK ($TYPE_TEXT round-tripped)"
    else
        echo "check-unicode: FAIL"
        PASS=0
    fi
fi

if [ "${CHECK_TEARDOWN:-1}" = 1 ]; then
    echo "--- teardown check ---"
    sleep 1
    # Only the daemon counts: the exec'd app is also named xcomd.
    LEFT_DAEMON=$(podman exec xcom-testvm sh -c "ps -eo args | grep -E '^xcomd (serve|session)' || true" | tr -d '\r')
    LEFT_XVFB=$(podman exec xcom-testvm sh -c "pgrep -x Xvfb || true" | tr -d '\r')
    if [ -n "$LEFT_DAEMON$LEFT_XVFB" ]; then
        echo "check-teardown: FAIL (daemon='$LEFT_DAEMON' xvfb='$LEFT_XVFB' left behind)"
        podman exec xcom-testvm sh -c "ps -o pid,etime,stat,args -p $LEFT_DAEMON 2>/dev/null || true"
        PASS=0
    else
        echo "check-teardown: OK (daemon and Xvfb exited with the session)"
    fi
fi

if [ "${CHECK_KEYS:-0}" = 1 ]; then
    echo "--- keys received by the remote app ---"
    PODLOG=$(podman exec xcom-testvm cat /tmp/xcom-keys.log 2>/dev/null || true)
    echo "$PODLOG" | tail -6
    SYMS=$(python3 -c "import sys; print(' '.join(hex(ord(c)) for c in sys.argv[1]))" "$TYPE_TEXT")
    MISSING=0
    for sym in $SYMS; do
        if ! grep -q "sym=$sym " <<< "$PODLOG"; then
            echo "check-keys: MISSING $sym (from $TYPE_TEXT)"
            MISSING=1
            PASS=0
        fi
    done
    if [ "$MISSING" = 0 ] && grep -q "sym=" <<< "$PODLOG"; then
        echo "check-keys: OK ($TYPE_TEXT arrived)"
    else
        PASS=0
    fi
    # Latency: keys must not sit in a user-space buffer until unrelated
    # traffic flushes them (that made typing feel late and bursty).
    TT=$(grep '\[xcom\] typing' "$OUT/client-ssh.log" | grep -o 't_ns=[0-9]*' | head -1 | cut -d= -f2 || true)
    FIRST=$(grep -o 't_ns=[0-9]*' <<< "$PODLOG" | head -1 | cut -d= -f2 || true)
    LAST=$(grep -o 't_ns=[0-9]*' <<< "$PODLOG" | tail -1 | cut -d= -f2 || true)
    if [ -n "$TT" ] && [ -n "$FIRST" ] && [ -n "$LAST" ]; then
        LAT_MS=$(( (FIRST - TT) / 1000000 ))
        SPREAD_MS=$(( (LAST - FIRST) / 1000000 ))
        echo "check-keys: first key after ${LAT_MS}ms, key spread ${SPREAD_MS}ms"
        if [ "$LAT_MS" -lt 0 ] || [ "$LAT_MS" -gt 300 ]; then
            echo "check-keys: FAIL - input latency ${LAT_MS}ms (buffer not flushed?)"
            PASS=0
        fi
        if [ "$SPREAD_MS" -gt 500 ]; then
            echo "check-keys: FAIL - bursty delivery (${SPREAD_MS}ms spread for 5 keys 30ms apart)"
            PASS=0
        fi
    fi
fi

if [ "$PASS" = 1 ]; then
    echo "=== e2e-ssh: PASS ==="
else
    echo "=== e2e-ssh: FAIL ==="
    exit 1
fi
