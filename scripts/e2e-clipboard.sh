#!/usr/bin/env bash
# Clipboard end-to-end: remote selection -> local clipboard, and back.
#
# The client runs on a private Xvfb (X11 backend) so XTEST can drive the
# Ctrl+Shift+C/V shortcuts without desktop input permissions. The daemon runs
# in the test container; a small helper owns/reads selections on both sides.
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

# Wait until the client window is viewable before focusing it: an unmapped
# window accepts set_input_focus without error but never receives the keys.
for _ in $(seq 1 80); do
    DISPLAY=$DISP "$X11I" list 2>/dev/null | grep -q 'VIEWABLE.*xcom' && break
    sleep 0.25
done
DISPLAY=$DISP "$X11I" focus xcom >/dev/null 2>&1 || true
sleep 0.5
# A click gives winit its FocusIn; XTEST keys otherwise go to PointerRoot.
DISPLAY=$DISP "$X11I" clickat 80 80 1 >/dev/null 2>&1 || true
sleep 0.5
FOCUS=$(DISPLAY=$DISP "$X11I" focus-get 2>/dev/null || true)
case "$FOCUS" in
    *focus=0x1*) echo "warning: window never took focus ($FOCUS)" >&2 ;;
esac

shortcut() {
    DISPLAY=$DISP "$X11I" down 37
    DISPLAY=$DISP "$X11I" down 50
    DISPLAY=$DISP "$X11I" keys "$1"
    DISPLAY=$DISP "$X11I" up 50
    DISPLAY=$DISP "$X11I" up 37
}

echo "== remote -> local (Ctrl+Shift+C) =="
(podman exec -e XAUTHORITY="$AUTH" xcom-testvm sh -c \
    "DISPLAY=$RDISPLAY /usr/local/bin/x11_input clipboard-set REMOTE-CLIP 12" \
    > "$OUT/remote-set.log" 2>&1) &
RPID=$!
sleep 0.8
shortcut 54 # C
sleep 1
GOT_LOCAL=$(DISPLAY=$DISP "$X11I" clipboard-get 2>/dev/null || true)
echo "local clipboard: ${GOT_LOCAL:-<empty>}"

echo "== local -> remote (Ctrl+Shift+V) =="
(DISPLAY=$DISP "$X11I" clipboard-set LOCAL-CLIP 12 > "$OUT/local-set.log" 2>&1) &
LPID=$!
sleep 0.8
shortcut 55 # V
sleep 1
GOT_REMOTE=$(podman exec -e XAUTHORITY="$AUTH" xcom-testvm sh -c \
    "DISPLAY=$RDISPLAY /usr/local/bin/x11_input clipboard-get" 2>/dev/null || true)
echo "remote clipboard: ${GOT_REMOTE:-<empty>}"

PASS=1
[ "$GOT_LOCAL" = "REMOTE-CLIP" ] || PASS=0
[ "$GOT_REMOTE" = "LOCAL-CLIP" ] || PASS=0
if [ "$PASS" = 1 ]; then
    echo "=== e2e-clipboard: PASS ==="
else
    echo "=== e2e-clipboard: FAIL ==="
    echo "--- client ---"; grep -iE "clipboard|key" "$OUT/client.log" | tail -20
    echo "--- daemon ---"; tail -20 "$OUT/daemon.log"
    echo "--- remote set ---"; cat "$OUT/remote-set.log" 2>/dev/null
fi
exit $((PASS == 1 ? 0 : 1))
