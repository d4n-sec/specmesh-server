#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
cd "$repo_root"

bash ci/check-toolchain.sh >&2
python3 ci/engine_compat.py verify >&2

output_dir=${1:-target/release-bundles}
mkdir -p -- "$output_dir"
output_dir=$(CDPATH= cd -- "$output_dir" && pwd -P)

allow_dirty=${SPECMESH_ALLOW_DIRTY:-0}
working_tree=clean
if [[ -n $(git status --porcelain --untracked-files=normal) ]]; then
    working_tree=dirty
    if [[ "$allow_dirty" != 1 ]]; then
        echo "release bundle requires a clean Server working tree" >&2
        exit 1
    fi
fi

engine_dir=${SPECMESH_ENGINE_DIR:-$repo_root/../specmesh-engine}
engine_dir=$(CDPATH= cd -- "$engine_dir" && pwd -P)
version=$(python3 -c 'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("Cargo.toml").read_text())["package"]["version"])')
engine_version=$(python3 -c 'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("compatibility/engine.toml").read_text())["version"])')
engine_revision=$(python3 ci/engine_compat.py revision)
commit=$(git rev-parse HEAD)
source_epoch=$(git show -s --format=%ct HEAD)
host_target=$(rustc -vV | sed -n 's/^host: //p')
bundle_root="specmesh-server-v${version}-${host_target}"
archive_name="${bundle_root}.tar.gz"

build_target_dir=""
bundle_tmp=""
archive_tmp=""
cleanup() {
    if [[ -n "$archive_tmp" && -f "$archive_tmp" ]]; then
        rm -f -- "$archive_tmp"
    fi
    if [[ -n "$bundle_tmp" && -d "$bundle_tmp" ]]; then
        rm -rf -- "$bundle_tmp"
    fi
    if [[ -n "$build_target_dir" && -d "$build_target_dir" ]]; then
        rm -rf -- "$build_target_dir"
    fi
}
trap cleanup EXIT

build_target_dir=$(mktemp -d \
    "${TMPDIR:-/tmp}/specmesh-server-release-bundle-target.XXXXXX")

SOURCE_DATE_EPOCH="$source_epoch" CARGO_TARGET_DIR="$build_target_dir" cargo build \
    --release \
    --locked \
    --all-features \
    --bin specmesh-server

bundle_tmp=$(mktemp -d "${TMPDIR:-/tmp}/specmesh-server-bundle.XXXXXX")
stage="$bundle_tmp/$bundle_root"
install -d -- "$stage/bin" "$stage/compatibility" "$stage/metadata"
install -m 0755 -- "$build_target_dir/release/specmesh-server" \
    "$stage/bin/specmesh-server"
install -m 0644 -- README.md "$stage/README.md"
install -m 0644 -- LICENSE "$stage/LICENSE"
install -m 0644 -- Cargo.lock "$stage/Cargo.lock"
install -m 0644 -- compatibility/engine.toml "$stage/compatibility/engine.toml"

cargo_lock_sha=$(sha256sum Cargo.lock | awk '{print $1}')
engine_lock_sha=$(sha256sum "$engine_dir/Cargo.lock" | awk '{print $1}')
binary_sha=$(sha256sum "$stage/bin/specmesh-server" | awk '{print $1}')
{
    printf 'package=specmesh-server\n'
    printf 'version=%s\n' "$version"
    printf 'commit=%s\n' "$commit"
    printf 'working_tree=%s\n' "$working_tree"
    printf 'engine_version=%s\n' "$engine_version"
    printf 'engine_revision=%s\n' "$engine_revision"
    printf 'source_date_epoch=%s\n' "$source_epoch"
    printf 'target=%s\n' "$host_target"
    printf 'rustc=%s\n' "$(rustc --version)"
    printf 'cargo=%s\n' "$(cargo --version)"
    printf 'cargo_lock_sha256=%s\n' "$cargo_lock_sha"
    printf 'engine_cargo_lock_sha256=%s\n' "$engine_lock_sha"
    printf 'binary_sha256=%s\n' "$binary_sha"
} > "$stage/metadata/build.txt"

archive_tmp=$(mktemp "$output_dir/.${archive_name}.tmp.XXXXXX")
tar \
    --sort=name \
    --mtime="@$source_epoch" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    --format=ustar \
    -C "$bundle_tmp" \
    -cf - \
    "$bundle_root" | gzip -n -9 > "$archive_tmp"
mv -f -- "$archive_tmp" "$output_dir/$archive_name"
archive_tmp=""

archive_sha=$(sha256sum "$output_dir/$archive_name" | awk '{print $1}')
printf '%s  %s\n' "$archive_sha" "$archive_name" > "$output_dir/$archive_name.sha256"
printf '%s\n' "$output_dir/$archive_name"
