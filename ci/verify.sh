#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
cd "$repo_root"

bash ci/check-toolchain.sh
python3 ci/engine_compat.py verify
git diff --check
git diff --cached --check

verification_target=""
stdio_engine_target=""
release_target=""
bundle_check=""
cleanup() {
    for temporary_dir in \
        "$verification_target" \
        "$stdio_engine_target" \
        "$release_target" \
        "$bundle_check"
    do
        if [[ -n "$temporary_dir" && -d "$temporary_dir" ]]; then
            rm -rf -- "$temporary_dir"
        fi
    done
}
trap cleanup EXIT

verification_target=$(mktemp -d \
    "${TMPDIR:-/tmp}/specmesh-server-verification-target.XXXXXX")
stdio_engine_target=$(mktemp -d \
    "${TMPDIR:-/tmp}/specmesh-server-stdio-engine-target.XXXXXX")
cargo fmt --package specmesh-server -- --check
SPECMESH_STDIO_ENGINE_TARGET_DIR="$stdio_engine_target" \
    CARGO_TARGET_DIR="$verification_target" cargo test \
    --locked \
    --all-targets \
    --all-features \
    -- \
    --test-threads=1
CARGO_TARGET_DIR="$verification_target" cargo clippy \
    --locked \
    --all-targets \
    --all-features \
    -- \
    -D warnings
RUSTDOCFLAGS="-D warnings" CARGO_TARGET_DIR="$verification_target" cargo doc \
    --locked \
    --all-features \
    --no-deps
bash ci/verify-package.sh

release_target=$(mktemp -d "${TMPDIR:-/tmp}/specmesh-server-release-target.XXXXXX")
CARGO_TARGET_DIR="$release_target" cargo build \
    --release \
    --locked \
    --all-features \
    --bin specmesh-server

bundle_check=$(mktemp -d "${TMPDIR:-/tmp}/specmesh-server-bundle-check.XXXXXX")
first=$(bash ci/release-bundle.sh "$bundle_check/first")
second=$(bash ci/release-bundle.sh "$bundle_check/second")
first_sha=$(sha256sum "$first" | awk '{print $1}')
second_sha=$(sha256sum "$second" | awk '{print $1}')
if [[ "$first_sha" != "$second_sha" ]]; then
    echo "Server release bundle is not reproducible" >&2
    exit 1
fi
printf 'Server release bundle SHA-256: %s\n' "$first_sha"
