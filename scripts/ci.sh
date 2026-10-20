#!/usr/bin/env bash
# Local quality gate (there is no CI service configured for this repo yet).
#
#   scripts/ci.sh [e2e-seconds]
#
# Runs the same checks the review used, in the same order.
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
cd "$REPO"

echo "== clippy =="
cargo clippy --workspace --all-targets -- -D warnings

echo "== tests =="
cargo test --workspace --release

echo "== windows compile check (pure-Rust crates) =="
# ring (via rustls) needs a mingw C compiler to build for the Windows target;
# this gate covers the crates where the original break was. The full client
# check belongs in a CI runner with the Windows toolchain installed.
if rustup target list --installed 2>/dev/null | grep -q x86_64-pc-windows-gnu; then
    cargo check -p xcom-proto -p xcom-codec --target x86_64-pc-windows-gnu
else
    echo "skipped: rustup target add x86_64-pc-windows-gnu"
fi

echo "== e2e (direct) =="
scripts/e2e-direct.sh "${1:-3}"
