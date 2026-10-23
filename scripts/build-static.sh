#!/usr/bin/env bash
# Build fully static Linux binaries (musl) inside an Alpine Rust container.
#
# This is the shipping artifact: it runs on any distro without dependencies,
# which the Debian test container and target VMs both rely on. The host build
# (glibc) stays the fast dev loop; ring's C/asm needs a musl toolchain, which
# Alpine provides natively.
#
# Usage: scripts/build-static.sh [--dev] [bin ...]     (default: xcomd)
#        CONTAINER=docker scripts/build-static.sh ...  (CI runners use docker)
# Output:
#   default   dist/<bin>-linux-x86_64       shipping: no M0 spike harness
#   --dev     dist/<bin>-dev-linux-x86_64   development: M0 harness enabled
#
# The M0 harness (anim/wm-demo/selftest/sink) is needed by the e2e scripts and
# the test container, so those use the --dev artifact. Shipping binaries stay
# lean.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=${XCOM_BUILD_IMAGE:-docker.io/library/rust:alpine}
# podman locally, docker on CI runners: same image, same named volumes.
CONTAINER=${CONTAINER:-podman}

DEV=0
if [ "${1:-}" = "--dev" ]; then
    DEV=1
    shift
fi
BINS=${*:-xcomd}

FEATURES="--no-default-features"
SUFFIX=""
if [ "$DEV" = 1 ]; then
    FEATURES=""
    SUFFIX="-dev"
fi

mkdir -p "$REPO/dist"

BIN_FLAGS=""
for b in $BINS; do
    BIN_FLAGS="$BIN_FLAGS --bin $b"
done

"$CONTAINER" run --rm \
    -v "$REPO":/src \
    -v xcom-cargo-registry:/usr/local/cargo/registry \
    -v xcom-cargo-target:/build/target \
    -w /src \
    -e CARGO_TARGET_DIR=/build/target \
    -e XCOM_BUILD="${XCOM_BUILD:-$(git -C "$REPO" describe --always --dirty --tags 2>/dev/null || echo unknown)}" \
    -e CARGO_PROFILE_RELEASE_DEBUG=false \
    -e CARGO_PROFILE_RELEASE_STRIP=debuginfo \
    "$IMAGE" \
    sh -c "
        set -e
        apk add --no-cache musl-dev >/dev/null
        cargo build --release $FEATURES $BIN_FLAGS
        for b in $BINS; do
            cp /build/target/release/\$b /src/dist/\$b$SUFFIX-linux-x86_64
        done
    "

for b in $BINS; do
    out="$REPO/dist/$b$SUFFIX-linux-x86_64"
    echo "--- dist/$b$SUFFIX-linux-x86_64 ($(du -h "$out" | cut -f1))"
    file "$out" | cut -c1-110
done
