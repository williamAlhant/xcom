#!/usr/bin/env bash
# Fast idle full-window-frame test with kitty.
#
# kitty is OpenGL-rendered: on some drivers a swap reports the whole window as
# damaged even when only the cursor cell changed. The client faithfully sends
# that frame, which is ~375 KB per blink for a 4K window (roughly 6 Mbps at
# kitty's default 0.5 s blink).
#
# Automated (default): starts a private Xvfb unless CLIENT_DISPLAY is set,
# waits for kitty's first frame and then for a gap with no new frames, and
# samples a short idle window. Pointer/keyboard isolation matters: input makes
# kitty redraw and pollutes the measurement.
#
# Interactive: `--interactive` keeps the client on your desktop (CLIENT_DISPLAY,
# default :0) with the same clean kitty config and forced blinking. No frame
# assertion is run; type, click and resize as usual. It exits after the given
# seconds, or when you close the window (0 = until closed).
#
# The signal is frame geometry, not the 5 s metrics line: a post-startup frame
# containing a rect that covers most of the window is the regression. The tail
# is checked by the shared scripts/check-frames.py helper.
#
# Opt-in: not part of scripts/ci.sh (it needs the test VM's kitty + llvmpipe).
#
# Usage:
#   scripts/e2e-kitty.sh [seconds]           automated check (default 10 s)
#   scripts/e2e-kitty.sh --interactive [s]   visible session (default until close)
#
# Environment:
#   CLIENT_DISPLAY=:0       display to use. Automated mode starts a private
#                           Xvfb when this is unset; interactive defaults to :0.
#   SPAWN=1200x900          session display size
#   QUIET_MS=400            automated: wait for this gap with no new frames
#   MEASURE=1.5             automated idle sample window
#   MAX_RECT_FRACTION=0.5   fail if a post-startup rect covers more of the window
#   KITTY_OPTS=...          extra kitty -o options (default: force blinking)
#   EXPECT_FRAMES=1         require post-startup frames (0 for a quiet baseline)
#   SKIP_TESTVM_UP=1        assume xcom-testvm is already current and running
#   KEEP_SESSION=1          interactive: leave the session running on exit
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
XCOM=${XCOM:-$REPO/target/release/xcom}
INTERACTIVE=${INTERACTIVE:-0}
while [ $# -gt 0 ]; do
    case "$1" in
        -i|--interactive) INTERACTIVE=1; shift ;;
        *) break ;;
    esac
done
if [ $# -gt 0 ]; then
    SECS=$1
elif [ "$INTERACTIVE" = 1 ]; then
    SECS=0
else
    SECS=10
fi
SPAWN=${SPAWN:-1200x900}
QUIET_MS=${QUIET_MS:-400}
MEASURE=${MEASURE:-1.5}
MAX_RECT_FRACTION=${MAX_RECT_FRACTION:-0.5}
KITTY_OPTS=${KITTY_OPTS:-"-o cursor_blink_interval=0.5 -o cursor_stop_blinking_after=0 -o background_opacity=1"}
EXPECT_FRAMES=${EXPECT_FRAMES:-1}
KEEP_SESSION=${KEEP_SESSION:-0}
NAME="kitty-idle-$$"
KEY=/tmp/opencode/e2e/id_ed25519
OUT=/tmp/opencode/e2e-kitty/$$
mkdir -p "$OUT"   # per-run: parallel runs must not share logs or sessions

SSH_ARGS="-p 2222 -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
CONFIG=/tmp/xcom-kitty-clean-$$
RUNTIME=/tmp/xcom-kitty-runtime-$$
XVFB_PID=""

cleanup() {
    kill "${CLIENT_PID:-}" 2>/dev/null || true
    if [ "$KEEP_SESSION" != 1 ]; then
        "$XCOM" kill --ssh xcom@127.0.0.1 --ssh-args "$SSH_ARGS" --session "$NAME" \
            >/dev/null 2>&1 || true
        podman exec xcom-testvm sh -c "rm -rf '$CONFIG' '$RUNTIME'" \
            >/dev/null 2>&1 || true
    fi
    if [ -n "$XVFB_PID" ]; then
        kill "$XVFB_PID" 2>/dev/null || true
        wait "$XVFB_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

if [ ! -x "$XCOM" ]; then
    echo "error: $XCOM is missing; build the client first" >&2
    echo "       cargo build --release -p xcom-client" >&2
    exit 2
fi

# Automated runs must not receive desktop input: a click or key makes kitty
# redraw and swamps the idle blink. Start our own display unless the caller
# explicitly selected one (for watching an automated run on the desktop).
start_private_xvfb() {
    local xvfb display
    xvfb=${XCOM_XVFB:-$(command -v Xvfb || echo "$HOME/.local/xvfb/usr/bin/Xvfb")}
    if [ ! -x "$xvfb" ]; then
        echo "error: Xvfb not found; set XCOM_XVFB to its path" >&2
        exit 2
    fi
    for display in $(seq 99 109); do
        [ -e "/tmp/.X11-unix/X$display" ] && continue
        "$xvfb" ":$display" -screen 0 1600x1000x24 -nolisten tcp \
            > "$OUT/xvfb.log" 2>&1 &
        XVFB_PID=$!
        for _ in $(seq 1 40); do
            [ -S "/tmp/.X11-unix/X$display" ] && break
            kill -0 "$XVFB_PID" 2>/dev/null || break
            sleep 0.05
        done
        if [ -S "/tmp/.X11-unix/X$display" ]; then
            sleep 0.1   # X server finished its post-socket setup
            CLIENT_DISPLAY=":$display"
            return 0
        fi
        kill "$XVFB_PID" 2>/dev/null || true
        wait "$XVFB_PID" 2>/dev/null || true
        XVFB_PID=""
    done
    echo "error: could not start a private Xvfb (tried :99-:109)" >&2
    exit 2
}

SETUP0=$(date +%s%N)
if [ "$INTERACTIVE" = 1 ]; then
    CLIENT_DISPLAY=${CLIENT_DISPLAY:-:0}
else
    if [ -z "${CLIENT_DISPLAY:-}" ]; then
        start_private_xvfb
    fi
fi

if [ "${SKIP_TESTVM_UP:-0}" != 1 ]; then
    "$REPO/scripts/testvm.sh" up
fi
TEST0=$(date +%s%N)

if ! podman exec xcom-testvm sh -c 'command -v kitty' >/dev/null 2>&1; then
    echo "error: kitty is not installed in xcom-testvm; rebuild it:" >&2
    echo "       scripts/testvm.sh up" >&2
    exit 2
fi
if ! podman exec xcom-testvm sh -c 'command -v glxinfo' >/dev/null 2>&1; then
    echo "warning: mesa-utils/glxinfo missing in xcom-testvm; kitty GL failures will be harder to diagnose" >&2
fi

# A clean config keeps the user's kitty config (shaders, background images,
# cursor settings) out of the measurement. cursor_stop_blinking_after=0 forces
# the blink forever, so neither mode depends on kitty's 15 s idle stop.
podman exec xcom-testvm sh -c "rm -rf '$CONFIG' '$RUNTIME' && \
    mkdir -p '$CONFIG' '$RUNTIME' && chmod 700 '$RUNTIME'"

APP="env -u WAYLAND_DISPLAY LIBGL_ALWAYS_SOFTWARE=1 \
    XDG_RUNTIME_DIR=$RUNTIME KITTY_CONFIG_DIRECTORY=$CONFIG kitty $KITTY_OPTS"

run_client() {
    DISPLAY="$CLIENT_DISPLAY" "$XCOM" connect --ssh xcom@127.0.0.1 \
        --ssh-args "$SSH_ARGS" --session "$NAME" --replace --spawn "$SPAWN" \
        --exec "$APP" --seconds "$SECS" --no-clipboard-sync "$@"
}

if [ "$INTERACTIVE" = 1 ]; then
    echo "=== e2e-kitty interactive: session $NAME, display $CLIENT_DISPLAY ==="
    echo "type/click the window; $SECS seconds (0 = until closed)"
    run_client
    if [ "$KEEP_SESSION" = 1 ]; then
        echo "session $NAME left running; stop it with:"
        echo "  $XCOM kill --ssh xcom@127.0.0.1 --ssh-args \"$SSH_ARGS\" --session $NAME"
    fi
    exit 0
fi

echo "=== e2e-kitty: session $NAME, display $CLIENT_DISPLAY, sample ${MEASURE}s ==="
DISPLAY="$CLIENT_DISPLAY" XCOM_DEBUG=1 run_client > "$OUT/client.log" 2>&1 &
CLIENT_PID=$!

# Wait for kitty's window map/first frame. This is the real readiness signal:
# no fixed startup sleep.
for _ in $(seq 1 80); do
    grep -q '\[ui\] frame' "$OUT/client.log" 2>/dev/null && break
    if ! kill -0 "$CLIENT_PID" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if ! grep -q '\[ui\] frame' "$OUT/client.log" 2>/dev/null; then
    echo "error: the session did not produce a frame (did kitty start?)" >&2
    echo "--- client ---" >&2
    tail -40 "$OUT/client.log" >&2 || true
    echo "--- software GL check ---" >&2
    podman exec xcom-testvm sh -c \
        "LIBGL_ALWAYS_SOFTWARE=1 xvfb-run -a glxinfo -B 2>&1 | head -20" >&2 || true
    exit 1
fi

# Wait for a quiet gap instead of a fixed settle. Startup redraws and the
# server's 400/800 ms re-syncs arrive in bursts; kitty's 2 Hz blink leaves
# ~500 ms gaps. Measuring after a gap starts between blinks, not mid-burst.
last=$(grep -c '\[ui\] frame' "$OUT/client.log")
quiet=0
for _ in $(seq 1 120); do   # up to 6 s for a continuously animating app
    sleep 0.05
    count=$(grep -c '\[ui\] frame' "$OUT/client.log")
    if [ "$count" -gt "$last" ]; then
        quiet=0
        last=$count
    else
        quiet=$((quiet + 50))
    fi
    [ "$quiet" -ge "$QUIET_MS" ] && break
done
if [ "$quiet" -lt "$QUIET_MS" ]; then
    echo "error: frames never settled (continuous animation, not an idle blink)" >&2
    exit 1
fi

# Sample only the settled tail; startup full frames cannot trip the assertion.
OFFSET=$(wc -c < "$OUT/client.log")
sleep "$MEASURE"
tail -c +$((OFFSET + 1)) "$OUT/client.log" > "$OUT/sample.log" || true

kill "$CLIENT_PID" 2>/dev/null || true
wait "$CLIENT_PID" 2>/dev/null || true
CLIENT_PID=
TEST1=$(date +%s%N)

status=0
python3 "$REPO/scripts/check-frames.py" "$OUT/sample.log" \
    --max-rect-fraction "$MAX_RECT_FRACTION" \
    --expect-frames "$EXPECT_FRAMES" || status=$?

echo "--- sample frames (last 5) ---"
grep '\[ui\] frame' "$OUT/sample.log" | tail -5 || true
printf 'timing: setup=%dms test=%dms\n' \
    $(((TEST0 - SETUP0) / 1000000)) $(((TEST1 - TEST0) / 1000000))
echo "--- full logs in $OUT ---"
exit "$status"
