#!/usr/bin/env bash
# Manage the xcom test VM container (Debian + sshd + Xvfb + test apps).
#
#   scripts/testvm.sh up        build + start (installs the dev static xcomd)
#   scripts/testvm.sh down      stop + remove
#   scripts/testvm.sh status    show state
#
# The container runs with host networking on purpose: the dedicated channel
# port is chosen dynamically by the daemon and must be reachable from the
# client, exactly the NAT case `xcom connect --connect` exists for.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
NAME=xcom-testvm
KEY=/tmp/opencode/e2e/id_ed25519
BUILDCTX=/tmp/opencode/e2e/testvm

case "${1:-up}" in
    up)
        mkdir -p "$BUILDCTX"
        if [ ! -f "$KEY" ]; then
            ssh-keygen -t ed25519 -N "" -f "$KEY" -q
        fi
        # Build context outside the repo so the test key never lands in git.
        cp "$KEY.pub" "$BUILDCTX/authorized_keys"
        cp "$REPO/deploy/testvm/Dockerfile" "$BUILDCTX/Dockerfile"

        # The container needs the *dev* artifact: the e2e scripts run
        # `xcomd m0 anim|wm-demo|selftest`, which the shipping build drops.
        DEVBIN="$REPO/dist/xcomd-dev-linux-x86_64"
        # Rebuild when sources changed: the container runs the static artifact
        # and a stale one silently invalidates the test (that bug already bit
        # us once), but a rebuild per run is needless waiting.
        NEWER=$(find "$REPO/crates" "$REPO/Cargo.toml" "$REPO/Cargo.lock" -newer "$DEVBIN" -name '*.rs' -o -newer "$DEVBIN" -name 'Cargo.*' 2>/dev/null | head -1)
        if [ ! -x "$DEVBIN" ] || [ -n "$NEWER" ]; then
            "$REPO/scripts/build-static.sh" --dev xcomd
        else
            echo "static dev xcomd up to date"
        fi

        podman build -t xcom-testvm "$BUILDCTX"
        podman rm -f "$NAME" >/dev/null 2>&1 || true
        podman run -d --name "$NAME" --network host \
            -v "$DEVBIN:/usr/local/bin/xcomd:ro" \
            xcom-testvm
        echo "waiting for sshd..."
        for _ in $(seq 1 50); do
            if ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
                -o ConnectTimeout=1 -p 2222 -i "$KEY" xcom@127.0.0.1 true 2>/dev/null; then
                echo "$NAME is up (ssh -p 2222 -i $KEY xcom@127.0.0.1)"
                exit 0
            fi
            sleep 0.2
        done
        echo "error: sshd did not come up" >&2
        podman logs "$NAME" | tail -5 >&2
        exit 1
        ;;
    down)
        podman rm -f "$NAME"
        ;;
    status)
        podman ps -a --filter "name=$NAME" --format '{{.Names}} {{.Status}}'
        ;;
    *)
        echo "usage: $0 up|down|status" >&2
        exit 2
        ;;
esac
