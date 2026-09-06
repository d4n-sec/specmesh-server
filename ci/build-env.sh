#!/usr/bin/env bash
# Source in each build entry point; never changes the user's global environment.
SPECMESH_PYTHON=${SPECMESH_PYTHON:-python3}
if ! "$SPECMESH_PYTHON" -c 'import sys, tomllib; assert sys.version_info >= (3, 11)' >/dev/null 2>&1; then
    echo "Python 3.11+ is required; set SPECMESH_PYTHON to its executable (found: $SPECMESH_PYTHON)" >&2
    return 1
fi
export SPECMESH_PYTHON
export PYTHONDONTWRITEBYTECODE=1

# The pinned macOS toolchain ships libLLVM in sysroot/lib, while rust-objcopy's
# loader searches a nested lib directory. Keep the repair local to build children.
case "$(rustc -vV | sed -n 's/^host: //p')" in
    *-apple-darwin)
        SPECMESH_RUST_SYSROOT=$(rustc --print sysroot)
        if [[ ! -f "$SPECMESH_RUST_SYSROOT/lib/libLLVM.dylib" ]]; then
            echo "Rust toolchain is missing $SPECMESH_RUST_SYSROOT/lib/libLLVM.dylib" >&2
            return 1
        fi
        export DYLD_LIBRARY_PATH="$SPECMESH_RUST_SYSROOT/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
        SPECMESH_RUST_HOST=$(rustc -vV | sed -n 's/^host: //p')
        "$SPECMESH_RUST_SYSROOT/lib/rustlib/$SPECMESH_RUST_HOST/bin/rust-objcopy" --version >&2 || return 1
        ;;
esac
