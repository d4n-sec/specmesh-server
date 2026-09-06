#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
cd "$repo_root"

source "$script_dir/build-env.sh"

bash ci/check-toolchain.sh >&2
"$SPECMESH_PYTHON" ci/engine_compat.py verify >&2

output_dir=${1:-target/release-bundles}
mkdir -p -- "$output_dir"
output_dir=$(CDPATH= cd -- "$output_dir" && pwd -P)

if [[ -n $(git status --porcelain --untracked-files=normal) ]]; then
    echo "local delivery requires a clean Server working tree; ALLOW_DIRTY does not apply" >&2
    exit 1
fi

engine_dir=${SPECMESH_ENGINE_DIR:-$repo_root/../specmesh-engine}
engine_dir=$(CDPATH= cd -- "$engine_dir" && pwd -P)
engine_source_sha=$("$SPECMESH_PYTHON" "$script_dir/bundle.py" source-sha256 "$engine_dir")
version=$("$SPECMESH_PYTHON" -c 'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("Cargo.toml").read_text())["package"]["version"])')
engine_version=$("$SPECMESH_PYTHON" -c 'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("compatibility/engine.toml").read_text())["version"])')
engine_revision=$("$SPECMESH_PYTHON" ci/engine_compat.py revision)
engine_tree=$("$SPECMESH_PYTHON" ci/engine_compat.py tree)
commit=$(git rev-parse HEAD)
commit_tree=$(git rev-parse HEAD^{tree})
source_sha=$("$SPECMESH_PYTHON" "$script_dir/bundle.py" source-sha256 "$repo_root")
source_epoch=$(git show -s --format=%ct HEAD)
host_target=$(rustc -vV | sed -n 's/^host: //p')
bundle_root="specmesh-server-v${version}-${host_target}"
archive_name="${bundle_root}.tar.gz"

build_target_dir=""
bundle_tmp=""
archive_tmp=""
source_tmp=""
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
    if [[ -n "$source_tmp" && -d "$source_tmp" ]]; then
        rm -rf -- "$source_tmp"
    fi
}
trap cleanup EXIT

build_target_dir=$(mktemp -d \
    "${TMPDIR:-/tmp}/specmesh-server-release-bundle-target.XXXXXX")
source_tmp=$(mktemp -d "${TMPDIR:-/tmp}/specmesh-server-release-source.XXXXXX")
"$SPECMESH_PYTHON" "$script_dir/local_snapshot.py" prepare \
    --server "$repo_root" --engine "$engine_dir" \
    --revision "$engine_revision" --tree "$engine_tree" \
    --output "$source_tmp/snapshot" >&2
source_root="$source_tmp/snapshot/specmesh-server"
encoded_rustflags=$("$SPECMESH_PYTHON" "$script_dir/local_snapshot.py" rustflags \
    --output "$source_tmp/snapshot")
cd "$source_root"

SOURCE_DATE_EPOCH="$source_epoch" CARGO_TARGET_DIR="$build_target_dir" \
    CARGO_ENCODED_RUSTFLAGS="$encoded_rustflags" cargo build \
    --release \
    --locked \
    --offline \
    --all-features \
    --manifest-path "$source_root/Cargo.toml" \
    --bin specmesh-server
"$SPECMESH_PYTHON" "$script_dir/local_snapshot.py" verify --output "$source_tmp/snapshot" \
    --server "$repo_root" --engine "$engine_dir"

bundle_tmp=$(mktemp -d "${TMPDIR:-/tmp}/specmesh-server-bundle.XXXXXX")
stage="$bundle_tmp/$bundle_root"
install -d -- "$stage/bin" "$stage/compatibility" "$stage/metadata"
install -m 0755 -- "$build_target_dir/release/specmesh-server" \
    "$stage/bin/specmesh-server"
install -m 0644 -- README.md "$stage/README.md"
install -m 0644 -- LICENSE "$stage/LICENSE"
install -m 0644 -- Cargo.lock "$stage/Cargo.lock"
install -m 0644 -- compatibility/engine.toml "$stage/compatibility/engine.toml"
install -m 0644 -- "$source_tmp/snapshot/source-lock.json" "$stage/metadata/source-lock.json"

cargo_lock_sha=$("$SPECMESH_PYTHON" "$script_dir/bundle.py" sha256 Cargo.lock)
engine_lock_sha=$("$SPECMESH_PYTHON" "$script_dir/bundle.py" sha256 "$source_tmp/snapshot/specmesh-engine/Cargo.lock")
source_lock_sha=$("$SPECMESH_PYTHON" "$script_dir/bundle.py" sha256 "$stage/metadata/source-lock.json")
binary_sha=$("$SPECMESH_PYTHON" "$script_dir/bundle.py" sha256 "$stage/bin/specmesh-server")
{
    printf 'package=specmesh-server\n'
    printf 'version=%s\n' "$version"
    printf 'commit=%s\n' "$commit"
    printf 'commit_tree=%s\n' "$commit_tree"
    printf 'source_sha256=%s\n' "$source_sha"
    printf 'python=%s\n' "$("$SPECMESH_PYTHON" --version)"
    printf 'archive_format=ustar+gzip-v1\n'
    printf 'working_tree=clean\n'
    printf 'dependency_source=local-git-snapshot\n'
    printf 'rust_path_remap=specmesh-source\n'
    printf 'source_lock_sha256=%s\n' "$source_lock_sha"
    printf 'engine_version=%s\n' "$engine_version"
    printf 'engine_revision=%s\n' "$engine_revision"
    printf 'engine_tree=%s\n' "$engine_tree"
    printf 'engine_source_sha256=%s\n' "$engine_source_sha"
    printf 'engine_working_tree=clean\n'
    printf 'source_date_epoch=%s\n' "$source_epoch"
    printf 'target=%s\n' "$host_target"
    printf 'rustc=%s\n' "$(rustc --version)"
    printf 'cargo=%s\n' "$(cargo --version)"
    printf 'cargo_lock_sha256=%s\n' "$cargo_lock_sha"
    printf 'engine_cargo_lock_sha256=%s\n' "$engine_lock_sha"
    printf 'binary_sha256=%s\n' "$binary_sha"
} > "$stage/metadata/build.txt"

archive_tmp=$(mktemp "$output_dir/.${archive_name}.tmp.XXXXXX")
"$SPECMESH_PYTHON" "$script_dir/bundle.py" archive "$stage" "$archive_tmp" "$source_epoch"
mv -f -- "$archive_tmp" "$output_dir/$archive_name"
archive_tmp=""

archive_sha=$("$SPECMESH_PYTHON" "$script_dir/bundle.py" sha256 "$output_dir/$archive_name")
printf '%s  %s\n' "$archive_sha" "$archive_name" > "$output_dir/$archive_name.sha256"
printf '%s\n' "$output_dir/$archive_name"
