#!/usr/bin/env bash
# Bump the workspace version, commit it, and create the annotated release tag.
#
# Usage:
#   scripts/release.sh patch|minor|major|X.Y.Z [--dry-run] [--verify] [--force]
#
# The release workflow is triggered by pushing the tag. This script never
# pushes; it prints the two commands to run after reviewing the commit/tag.
#
# --verify   run scripts/ci.sh before committing/tagging
# --force    allow a branch other than main (still requires a clean tree)
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
cd "$REPO"

DRY_RUN=0
VERIFY=0
FORCE=0
BUMP=""
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        --verify) VERIFY=1 ;;
        --force) FORCE=1 ;;
        -*) echo "error: unknown option $arg" >&2; exit 2 ;;
        *)
            if [ -n "$BUMP" ]; then
                echo "error: only one version argument is accepted" >&2
                exit 2
            fi
            BUMP=$arg
            ;;
    esac
done
if [ -z "$BUMP" ]; then
    echo "usage: $0 patch|minor|major|X.Y.Z [--dry-run] [--verify] [--force]" >&2
    exit 2
fi

current_version() {
    awk '
        /^\[workspace\.package\]/ { section = 1; next }
        /^\[/ { section = 0 }
        section && /^version[[:space:]]*=/ {
            gsub(/["[:space:]]/, "", $3)
            print $3
            exit
        }
    ' Cargo.toml
}

version_line() {
    awk '
        /^\[workspace\.package\]/ { section = 1; next }
        /^\[/ { section = 0 }
        section && /^version[[:space:]]*=/ { print NR; exit }
    ' Cargo.toml
}

is_semver() {
    [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

current=$(current_version)
if ! is_semver "$current"; then
    echo "error: cannot read the workspace version from Cargo.toml (got ${current:-empty})" >&2
    exit 1
fi

case "$BUMP" in
    patch|minor|major)
        IFS=. read -r major minor patch <<< "$current"
        case "$BUMP" in
            patch) patch=$((patch + 1)) ;;
            minor) minor=$((minor + 1)); patch=0 ;;
            major) major=$((major + 1)); minor=0; patch=0 ;;
        esac
        new="$major.$minor.$patch"
        ;;
    *)
        new=${BUMP#v}
        if ! is_semver "$new"; then
            echo "error: expected patch, minor, major or X.Y.Z (got $BUMP)" >&2
            exit 2
        fi
        ;;
esac

if [ "$new" = "$current" ]; then
    echo "error: version is already $current" >&2
    exit 1
fi
if git rev-parse -q --verify "refs/tags/v$new" >/dev/null; then
    echo "error: tag v$new already exists" >&2
    exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree is not clean; commit or stash first" >&2
    git status --short >&2
    exit 1
fi
branch=$(git symbolic-ref --short -q HEAD || true)
if [ -z "$branch" ]; then
    echo "error: detached HEAD; check out a branch first" >&2
    exit 1
fi
if [ "$branch" != "main" ] && [ "$FORCE" != 1 ]; then
    echo "error: releases are made from main (use --force to override; on $branch)" >&2
    exit 1
fi

line=$(version_line)
if [ -z "$line" ]; then
    echo "error: no [workspace.package] version line in Cargo.toml" >&2
    exit 1
fi

echo "release: $current -> $new (branch $branch)"
if [ "$DRY_RUN" = 1 ]; then
    echo "would update Cargo.toml:$line, refresh Cargo.lock, commit, and tag v$new"
    echo "push afterwards:"
    echo "  git push origin $branch"
    echo "  git push origin v$new"
    exit 0
fi

sed -i "${line}s/.*/version = \"$new\"/" Cargo.toml
if [ "$(current_version)" != "$new" ]; then
    echo "error: Cargo.toml version update did not take effect" >&2
    git checkout -- Cargo.toml
    exit 1
fi

# `cargo check` refreshes the lockfile entries for workspace crates and gives
# an early compile check. It does not build release artifacts.
if ! cargo check --workspace; then
    echo "error: cargo check failed; restoring Cargo.toml/Cargo.lock" >&2
    git checkout -- Cargo.toml Cargo.lock
    exit 1
fi
if git diff --quiet -- Cargo.lock; then
    echo "error: Cargo.lock was not updated; refusing to tag" >&2
    git checkout -- Cargo.toml
    exit 1
fi

if [ "$VERIFY" = 1 ]; then
    if ! "$REPO/scripts/ci.sh"; then
        echo "error: verification failed; restoring Cargo.toml/Cargo.lock" >&2
        git checkout -- Cargo.toml Cargo.lock
        exit 1
    fi
fi

git add Cargo.toml Cargo.lock
git commit -m "Bump version to $new" -m "Release v$new."
git tag -a "v$new" -m "xcom $new"

echo
echo "created commit $(git rev-parse --short HEAD) and tag v$new"
echo "review, then push:"
echo "  git push origin $branch"
echo "  git push origin v$new"
