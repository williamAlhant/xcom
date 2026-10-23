#!/usr/bin/env bash
# Cross-build the Windows client (x86_64-pc-windows-msvc) with cargo-xwin.
#
#   scripts/build-windows.sh
#
# Caches are bind-mounted so nothing is re-downloaded or rebuilt from
# scratch on later runs: the MS CRT/SDK (XWIN_CACHE_DIR), the cargo
# registry, and the repo's target/ directory all live on the host.
# Rootless podman maps container root to the invoking user, so files
# created in the mounts stay owned by you (no --userns needed).
#
# Output: target/x86_64-pc-windows-msvc/release/xcom.exe
#         (statically linked CRT: no vcruntime140/ucrt dependency)
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)

# Pinned by digest: the image the first successful build used, so later
# builds are reproducible.
IMAGE=docker.io/messense/cargo-xwin@sha256:10fa1f350addf9c59345954f9d6eb10e353ba481c6130757a5c95d8daad37191

podman run --rm \
  -v "$REPO":/work -w /work \
  -v "$HOME/.cargo":/cargo -e CARGO_HOME=/cargo \
  -v "$HOME/.cache/xwin":/cache/xwin -e XWIN_CACHE_DIR=/cache/xwin \
  -e RUSTFLAGS="-C target-feature=+crt-static" \
  -e XCOM_BUILD="${XCOM_BUILD:-$(git -C "$REPO" describe --always --dirty --tags 2>/dev/null || echo unknown)}" \
  -e CARGO_PROFILE_RELEASE_DEBUG=${XCOM_WIN_DEBUG:-false} \
  "$IMAGE" \
  cargo xwin build --release --target x86_64-pc-windows-msvc -p xcom-client

echo "built: $REPO/target/x86_64-pc-windows-msvc/release/xcom.exe"
