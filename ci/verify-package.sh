#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
cd "$repo_root"

python3 ci/engine_compat.py verify
engine_dir=${SPECMESH_ENGINE_DIR:-$repo_root/../specmesh-engine}
engine_dir=$(CDPATH= cd -- "$engine_dir" && pwd -P)
version=$(python3 -c 'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("Cargo.toml").read_text())["package"]["version"])')
engine_path_toml=$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$engine_dir")

artifact_state() {
    local artifact=$1
    if [[ -f "$artifact" && ! -L "$artifact" ]]; then
        printf 'file %s ' "$artifact"
        sha256sum "$artifact" | awk '{print $1}'
    elif [[ -e "$artifact" || -L "$artifact" ]]; then
        printf 'non-regular %s\n' "$artifact"
    else
        printf 'absent %s\n' "$artifact"
    fi
}

default_target_state() {
    artifact_state "$repo_root/target/debug/specmesh-server"
    artifact_state "$repo_root/target/debug/specmesh-server.d"
    artifact_state "$engine_dir/target/debug/specmesh"
    artifact_state "$engine_dir/target/debug/specmesh.d"
}

default_target_before=$(default_target_state)

package_target=""
package_tmp=""
build_target=""
cleanup() {
    for temporary_dir in "$package_target" "$package_tmp" "$build_target"; do
        if [[ -n "$temporary_dir" && -d "$temporary_dir" ]]; then
            rm -rf -- "$temporary_dir"
        fi
    done
}
trap cleanup EXIT

package_args=(
    package
    --locked
    --offline
    --no-verify
    --config
    "patch.crates-io.specmesh-engine.path=$engine_path_toml"
)
if [[ ${SPECMESH_ALLOW_DIRTY:-0} == 1 ]]; then
    package_args+=(--allow-dirty)
fi
package_target=$(mktemp -d "${TMPDIR:-/tmp}/specmesh-server-package-target.XXXXXX")
CARGO_TARGET_DIR="$package_target" cargo "${package_args[@]}"

crate_archive="$package_target/package/specmesh-server-${version}.crate"
if [[ ! -f "$crate_archive" ]]; then
    echo "cargo package did not create $crate_archive" >&2
    exit 1
fi

package_tmp=$(mktemp -d "${TMPDIR:-/tmp}/specmesh-server-package.XXXXXX")
tar -xzf "$crate_archive" -C "$package_tmp"
package_root="$package_tmp/specmesh-server-${version}"
python3 - "$package_root/Cargo.toml" "$engine_dir" <<'PY'
import json
import pathlib
import sys

manifest = pathlib.Path(sys.argv[1])
engine_dir = pathlib.Path(sys.argv[2])
with manifest.open("a", encoding="utf-8") as handle:
    handle.write("\n[patch.crates-io]\n")
    handle.write(f"specmesh-engine = {{ path = {json.dumps(str(engine_dir))} }}\n")
PY
install -m 0644 -- Cargo.lock "$package_root/Cargo.lock"
build_target=$(mktemp -d \
    "${TMPDIR:-/tmp}/specmesh-server-package-build-target.XXXXXX")
CARGO_NET_OFFLINE=true CARGO_TARGET_DIR="$build_target" cargo build \
    --release \
    --locked \
    --all-features \
    --manifest-path "$package_root/Cargo.toml"

if [[ "$(default_target_state)" != "$default_target_before" ]]; then
    echo "Server package verification modified default target artifacts" >&2
    exit 1
fi
