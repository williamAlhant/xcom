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
KEY=/tmp/opencode/e2e/id_ed25519
OUT=/tmp/opencode/e2e
mkdir -p "$OUT"

"$REPO/scripts/testvm.sh" up

SSH_ARGS="-p 2222 -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

DISPLAY=${CLIENT_DISPLAY:-:0} "$XCOM" connect --ssh xcom@127.0.0.1 \
    --ssh-args "$SSH_ARGS" --exec "$APP" \
    --seconds "$SECS" --dump-ppm "$OUT/frame-ssh.ppm" \
    > "$OUT/client-ssh.log" 2>&1 || true

echo "--- client (app: $APP) ---"
cat "$OUT/client-ssh.log"

PASS=1
grep -qE '[1-9][0-9]* frames blitted' "$OUT/client-ssh.log" || PASS=0
grep -q 'window .* opened' "$OUT/client-ssh.log" || PASS=0
if [ "${CHECK_BOX:-0}" = 1 ]; then
    python3 "$REPO/scripts/check-ppm.py" "$OUT/frame-ssh.ppm" box || PASS=0
fi
if [ "$PASS" = 1 ]; then
    echo "=== e2e-ssh: PASS ==="
else
    echo "=== e2e-ssh: FAIL ==="
    exit 1
fi
