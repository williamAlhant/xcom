#!/usr/bin/env bash
# End-to-end test through a real ssh session: the client bootstraps via ssh
# against the test VM container, then streams over the dedicated TLS channel.
#
# This exercises the real security model: the handshake line (token +
# certificate fingerprint) travels over ssh, everything after is direct.
#
# Usage: scripts/e2e-ssh.sh [seconds]
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
XCOM=${XCOM:-$REPO/target/release/xcom}
SECS=${1:-8}
APP=${APP:-"xterm -geometry 60x20"}
TYPE_TEXT=${TYPE_TEXT:-}
TYPE_DELAY=${TYPE_DELAY:-3}

# CHECK_KEYS=1: use the anim as the app (it logs received keysyms) and verify
# that text typed by the client actually arrives at the application.
if [ "${CHECK_KEYS:-0}" = 1 ]; then
    APP=${APP_ANIM:-"xcomd m0 anim --mode box --fps 60 --seconds 120 --input-log /tmp/xcom-keys.log"}
    TYPE_TEXT=${TYPE_TEXT:-hello}
    podman exec xcom-testvm rm -f /tmp/xcom-keys.log 2>/dev/null || true
fi
KEY=/tmp/opencode/e2e/id_ed25519
OUT=/tmp/opencode/e2e
mkdir -p "$OUT"

"$REPO/scripts/testvm.sh" up

SSH_ARGS="-p 2222 -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

TYPE_ARGS=""
if [ -n "$TYPE_TEXT" ]; then
    TYPE_ARGS="--type-text $TYPE_TEXT --type-delay $TYPE_DELAY"
fi

DISPLAY=${CLIENT_DISPLAY:-:0} "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --exec "$APP" \
    --seconds "$SECS" --dump-ppm "$OUT/frame-ssh.ppm" $TYPE_ARGS \
    > "$OUT/client-ssh.log" 2>&1 || true

echo "--- client (app: $APP) ---"
cat "$OUT/client-ssh.log"

PASS=1
grep -qE '[1-9][0-9]* frames blitted' "$OUT/client-ssh.log" || PASS=0
grep -q 'window .* opened' "$OUT/client-ssh.log" || PASS=0
if [ "${CHECK_BOX:-0}" = 1 ]; then
    python3 "$REPO/scripts/check-ppm.py" "$OUT/frame-ssh.ppm" box || PASS=0
fi
if [ "${CHECK_KEYS:-0}" = 1 ]; then
    echo "--- keys received by the remote app ---"
    PODLOG=$(podman exec xcom-testvm cat /tmp/xcom-keys.log 2>/dev/null || true)
    echo "$PODLOG" | tail -6
    SYMS=$(python3 -c "import sys; print(' '.join(hex(ord(c)) for c in sys.argv[1]))" "$TYPE_TEXT")
    for sym in $SYMS; do
        if ! grep -q "sym=$sym " <<< "$PODLOG"; then
            echo "check-keys: MISSING $sym (from $TYPE_TEXT)"
            PASS=0
        fi
    done
    grep -q "sym=" <<< "$PODLOG" && echo "check-keys: OK ($TYPE_TEXT arrived)" || PASS=0
fi
if [ "$PASS" = 1 ]; then
    echo "=== e2e-ssh: PASS ==="
else
    echo "=== e2e-ssh: FAIL ==="
    exit 1
fi
