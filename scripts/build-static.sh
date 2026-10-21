#!/usr/bin/env bash
# Build fully static Linux binaries (musl) inside an Alpine Rust container.
#
# This is the shipping artifact: it runs on any distro without dependencies,
# which the Debian test container and target VMs both rely on. The host build
# (glibc) stays the fast dev loop; ring's C/asm needs a musl toolchain, which
# Alpine provides natively.
#
# Usage: scripts/build-static.sh [bin ...]     (default: xcomd)
# Output: dist/<bin>-linux-x86_64
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=${XCOM_BUILD_IMAGE:-docker.io/library/rust:alpine}
BINS=${*:-xcomd}

mkdir -p "$REPO/dist"

BIN_FLAGS=""
for b in $BINS; do
    BIN_FLAGS="$BIN_FLAGS --bin $b"
done

podman run --rm \
    -v "$REPO":/src \
    -v xcom-cargo-registry:/usr/local/cargo/registry \
    -v xcom-cargo-target:/build/target \
    -w /src \
    -e CARGO_TARGET_DIR=/build/target \
    -e CARGO_PROFILE_RELEASE_DEBUG=false \
    -e CARGO_PROFILE_RELEASE_STRIP=debuginfo \
    "$IMAGE" \
    sh -c "
        set -e
        apk add --no-cache musl-dev >/dev/null
        cargo build --release $BIN_FLAGS
        for b in $BINS; do
            cp /build/target/release/\$b /src/dist/\$b-linux-x86_64
        done
    "

for b in $BINS; do
    echo "--- dist/$b-linux-x86_64 ($(du -h "$REPO/dist/$b-linux-x86_64" | cut -f1))"
    file "$REPO/dist/$b-linux-x86_64" | cut -c1-110
done
