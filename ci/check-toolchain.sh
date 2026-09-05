#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
expected=$(tr -d '[:space:]' < "$script_dir/rust-version")
rustc_version=$(rustc --version | awk '{print $2}')
cargo_version=$(cargo --version | awk '{print $2}')

if [[ "$rustc_version" != "$expected" || "$cargo_version" != "$expected" ]]; then
    echo "expected rustc/cargo $expected, found rustc $rustc_version and cargo $cargo_version" >&2
    exit 1
fi

printf 'verified rustc/cargo %s\n' "$expected"
