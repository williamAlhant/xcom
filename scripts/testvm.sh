#!/usr/bin/env bash
# Manage the xcom test VM container (Debian + sshd + Xvfb + test apps).
#
#   scripts/testvm.sh up        ensure it is current and running (idempotent)
#   scripts/testvm.sh down      stop + remove
#   scripts/testvm.sh status    show state
#
# `up` is cheap when nothing changed: a build-state label skips `podman build`
# entirely, and the container is only recreated when its image ID differs or
# the mounted xcomd binary was rebuilt. Readiness is polled, never slept for.
#
# The container runs with host networking on purpose: the dedicated channel
# port is chosen dynamically by the daemon and must be reachable from the
# client, exactly the NAT case `xcom connect --connect` exists for.
#
# Do not hold a flock across `podman run`: conmon inherits open FDs and keeps
# the lock for the container's lifetime, so the next `up` would hang.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
NAME=xcom-testvm
KEY=/tmp/opencode/e2e/id_ed25519
BUILDCTX=/tmp/opencode/e2e/testvm
DEVBIN="$REPO/dist/xcomd-dev-linux-x86_64"

# The container needs the *dev* artifact: the e2e scripts run
# `xcomd m0 anim|wm-demo|selftest`, which the shipping build drops.
ensure_dev_bin() {
    # Rebuild when sources changed: the container runs the static artifact and
    # a stale one silently invalidates the test. Set DEV_REBUILT so the
    # container is restarted to refresh the bind mount.
    local newer
    newer=$(find "$REPO/crates" "$REPO/Cargo.toml" "$REPO/Cargo.lock" \
        -newer "$DEVBIN" \( -name '*.rs' -o -name 'Cargo.*' \) 2>/dev/null | head -1)
    if [ ! -x "$DEVBIN" ] || [ -n "$newer" ]; then
        "$REPO/scripts/build-static.sh" --dev xcomd
        DEV_REBUILT=1
    else
        DEV_REBUILT=0
        echo "static dev xcomd up to date"
    fi
}

# Poll the real requirement (sshd up and xcomd mounted) instead of sleeping a
# guessed startup time. TCP is probed first so a not-yet-listening port does
# not spend a full ssh ConnectTimeout on every attempt.
wait_ready() {
    local deadline=$((SECONDS + ${READY_TIMEOUT:-15}))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if timeout 0.2 bash -c 'exec 3<>/dev/tcp/127.0.0.1/2222' 2>/dev/null; then
            if ssh -o BatchMode=yes -o StrictHostKeyChecking=no \
                -o UserKnownHostsFile=/dev/null -o ConnectTimeout=1 \
                -p 2222 -i "$KEY" xcom@127.0.0.1 \
                'test -x /usr/local/bin/xcomd' >/dev/null 2>&1; then
                echo "$NAME is up (ssh -p 2222 -i $KEY xcom@127.0.0.1)"
                return 0
            fi
        fi
        sleep 0.05
    done
    echo "error: sshd/xcomd did not become ready within ${READY_TIMEOUT:-15}s" >&2
    podman logs "$NAME" 2>&1 | tail -10 >&2 || true
    return 1
}

up() {
    mkdir -p "$BUILDCTX"
    if [ ! -f "$KEY" ]; then
        ssh-keygen -t ed25519 -N "" -f "$KEY" -q
    fi
    # Build context outside the repo so the test key never lands in git.
    # Preserve mtimes so an unchanged context stays byte- and time-identical
    # for podman's COPY cache.
    cp -p "$KEY.pub" "$BUILDCTX/authorized_keys"
    cp -p "$REPO/deploy/testvm/Dockerfile" "$BUILDCTX/Dockerfile"

    ensure_dev_bin

    # Build only when the image inputs changed. Podman would reuse layers, but
    # it still commits a new image (and a new created timestamp) on every
    # invocation; a label lets us skip the build entirely and keeps the image
    # date meaningful. Do not pin timestamps with --source-date-epoch: it
    # produces 1970-dated images and defeats the cache. Bump the v1 prefix if
    # the build context format changes.
    local image_state image_label build_log
    image_state="v1:$(cat "$BUILDCTX/Dockerfile" "$BUILDCTX/authorized_keys" | sha256sum | cut -d' ' -f1)"
    image_label=$(podman image inspect -f '{{index .Config.Labels "xcom.build-state"}}' "$NAME" 2>/dev/null || true)
    if [ "$image_label" != "$image_state" ]; then
        echo "building $NAME (Dockerfile or authorized_keys changed)..."
        build_log=$(mktemp)
        if ! podman build --label "xcom.build-state=$image_state" \
            -t "$NAME" "$BUILDCTX" >"$build_log" 2>&1; then
            cat "$build_log" >&2
            rm -f "$build_log"
            exit 1
        fi
        rm -f "$build_log"
    fi

    local image_id container_image running
    image_id=$(podman image inspect -f '{{.Id}}' "$NAME")
    container_image=$(podman inspect -f '{{.Image}}' "$NAME" 2>/dev/null || true)
    running=$(podman inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null || true)

    if [ -z "$container_image" ] || [ "$container_image" != "$image_id" ]; then
        # Missing, or built from a different image: recreate.
        podman rm -f "$NAME" >/dev/null 2>&1 || true
        podman run -d --name "$NAME" --network host \
            -v "$DEVBIN:/usr/local/bin/xcomd:ro" \
            "$NAME" >/dev/null
    elif [ "$running" != "true" ]; then
        podman start "$NAME" >/dev/null
    elif [ "$DEV_REBUILT" = 1 ]; then
        # The read-only bind mount would otherwise still show the old inode.
        podman restart "$NAME" >/dev/null
    fi

    wait_ready
}

case "${1:-up}" in
    up)
        up
        ;;
    down)
        podman rm -f "$NAME"
        ;;
    status)
        podman ps -a --filter "name=$NAME" --format '{{.Names}} {{.Status}}'
        if podman container exists "$NAME" 2>/dev/null; then
            echo "container image: $(podman inspect -f '{{.Image}}' "$NAME" 2>/dev/null || echo '?')"
        fi
        echo "current image:   $(podman image inspect -f '{{.Id}}' "$NAME" 2>/dev/null || echo 'missing')"
        ;;
    *)
        echo "usage: $0 up|down|status" >&2
        exit 2
        ;;
esac
