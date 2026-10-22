#!/usr/bin/env bash
# Clipboard end-to-end: the selection is mirrored in both directions with no
# client shortcut. The client runs on a private Xvfb (X11 backend); the daemon
# runs in the test container with gtk3-widget-factory. A small helper owns and
# reads selections on both sides.
#
# Usage: scripts/e2e-clipboard.sh [seconds]
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
SECS=${1:-20}
DISP=${CLIP_DISPLAY:-:98}
XCOM=${XCOM:-$REPO/target/release/xcom}
X11I=$REPO/target/debug/examples/x11_input
OUT=/tmp/opencode/e2e/clip-$$
mkdir -p "$OUT"

STARTED_XVFB=0
cleanup() {
    set +e
    podman exec xcom-testvm pkill -x gtk3-widget-factory 2>/dev/null
    podman exec xcom-testvm pkill -x xcomd 2>/dev/null
    [ -n "${CPID:-}" ] && kill "$CPID" 2>/dev/null
    [ -n "${DPID:-}" ] && kill "$DPID" 2>/dev/null
    [ -n "${RPID:-}" ] && kill "$RPID" 2>/dev/null
    [ -n "${LPID:-}" ] && kill "$LPID" 2>/dev/null
    if [ "$STARTED_XVFB" = 1 ] && [ -n "${XPID:-}" ]; then kill "$XPID" 2>/dev/null; fi
}
trap cleanup EXIT

echo "== build helper and test VM =="
cargo build -q -p xcom-x11 --example x11_input
"$REPO/scripts/testvm.sh" up >/dev/null
podman cp "$X11I" xcom-testvm:/usr/local/bin/x11_input

if ! DISPLAY=$DISP "$X11I" pointer >/dev/null 2>&1; then
    XVFB=${XCOM_XVFB:-$(command -v Xvfb || echo $HOME/.local/xvfb/usr/bin/Xvfb)}
    "$XVFB" "$DISP" -screen 0 1920x1080x24 -nolisten tcp +extension XTEST \
        > "$OUT/xvfb.log" 2>&1 &
    XPID=$!
    STARTED_XVFB=1
    sleep 1
fi

echo "== start daemon + client =="
podman exec -e XCOM_DEBUG=1 xcom-testvm xcomd serve --json --listen 127.0.0.1:0 \
    --exec 'gtk3-widget-factory' > "$OUT/hs.json" 2> "$OUT/daemon.log" &
DPID=$!
for _ in $(seq 1 50); do [ -s "$OUT/hs.json" ] && break; sleep 0.1; done
H="$OUT/hs.json"
RDISPLAY=$(jq -r .display "$H")
AUTH=$(podman exec xcom-testvm sh -c 'ls -t /tmp/xcom-xauth-* 2>/dev/null | head -1')
[ -n "$AUTH" ] || { echo "no Xauthority for the daemon display" >&2; exit 1; }

env -u WAYLAND_DISPLAY DISPLAY=$DISP XCOM_DEBUG=1 "$XCOM" connect \
    --direct "$(jq -r .connect "$H")" \
    --token-control "$(jq -r .token_control "$H")" \
    --token-bulk "$(jq -r .token_bulk "$H")" \
    --fingerprint "$(jq -r .cert_sha256 "$H")" --seconds "$SECS" \
    > "$OUT/client.log" 2>&1 &
CPID=$!

local_get() {
    DISPLAY=$DISP "$X11I" clipboard-get 2>/dev/null || true
}
remote_get() {
    podman exec -e XAUTHORITY="$AUTH" xcom-testvm sh -c \
        "DISPLAY=$RDISPLAY /usr/local/bin/x11_input clipboard-get" 2>/dev/null || true
}

# Poll a reader until it prints the expected value (the mirror is event driven,
# but allow for helper startup and the round trip).
wait_for() {
    local reader=$1 expect=$2
    for _ in $(seq 1 40); do
        local got
        got=$($reader)
        if [ "$got" = "$expect" ]; then
            echo "$got"
            return 0
        fi
        sleep 0.1
    done
    echo "${got:-<empty>}"
    return 1
}

echo "== remote -> local (no shortcut) =="
(podman exec -e XAUTHORITY="$AUTH" xcom-testvm sh -c \
    "DISPLAY=$RDISPLAY /usr/local/bin/x11_input clipboard-set REMOTE-CLIP 12" \
    > "$OUT/remote-set.log" 2>&1) &
RPID=$!
# Give the client's initial seed Get time to settle before the push.
sleep 0.5
GOT_LOCAL=$(wait_for local_get REMOTE-CLIP) || true
echo "local clipboard: ${GOT_LOCAL:-<empty>}"

# Stay outside the client's echo-suppression window before writing locally.
sleep 0.7

echo "== local -> remote (no shortcut) =="
(DISPLAY=$DISP "$X11I" clipboard-set LOCAL-CLIP 12 > "$OUT/local-set.log" 2>&1) &
LPID=$!
GOT_REMOTE=$(wait_for remote_get LOCAL-CLIP) || true
echo "remote clipboard: ${GOT_REMOTE:-<empty>}"

PASS=1
[ "$GOT_LOCAL" = "REMOTE-CLIP" ] || PASS=0
[ "$GOT_REMOTE" = "LOCAL-CLIP" ] || PASS=0
if [ "$PASS" = 1 ]; then
    echo "=== e2e-clipboard: PASS ==="
else
    echo "=== e2e-clipboard: FAIL ==="
    echo "--- client ---"; grep -iE "clipboard" "$OUT/client.log" | tail -20
    echo "--- daemon ---"; grep -iE "clipboard" "$OUT/daemon.log" | tail -20
    echo "--- remote set ---"; cat "$OUT/remote-set.log" 2>/dev/null
fi
exit $((PASS == 1 ? 0 : 1))
