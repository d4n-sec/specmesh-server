#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
cd "$repo_root"

source "$script_dir/build-env.sh"

bash ci/check-toolchain.sh
"$SPECMESH_PYTHON" ci/engine_compat.py verify
git diff --check
git diff --cached --check
"$SPECMESH_PYTHON" ci/test_bundle.py
"$SPECMESH_PYTHON" ci/test_local_snapshot.py

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
        if [[ "$temporary_dir" == "$bundle_check" && -n "${SPECMESH_BUNDLE_OUTPUT_DIR:-}" ]]; then
            continue
        fi
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

bundle_check=$(mktemp -d "${SPECMESH_BUNDLE_OUTPUT_DIR:-${TMPDIR:-/tmp}}/specmesh-server-bundle-check.XXXXXX")
first=$(bash ci/release-bundle.sh "$bundle_check/first")
second=$(bash ci/release-bundle.sh "$bundle_check/second")
first_sha=$("$SPECMESH_PYTHON" "$script_dir/bundle.py" sha256 "$first")
second_sha=$("$SPECMESH_PYTHON" "$script_dir/bundle.py" sha256 "$second")
"$SPECMESH_PYTHON" ci/verify_bundle.py "$first" "$repo_root"
"$SPECMESH_PYTHON" ci/verify_bundle.py "$second" "$repo_root"
if [[ "$first_sha" != "$second_sha" ]]; then
    echo "Server release bundle is not reproducible" >&2
    exit 1
fi
printf 'Server release bundle SHA-256: %s\n' "$first_sha"
printf 'Verified bundle paths: %s %s\n' "$first" "$second"
