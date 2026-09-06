# Server verification and packaging

`verify.sh` is the provider-neutral verification entry point used locally and
by CI. It first checks `compatibility/engine.toml` against the sibling Engine's
Cargo package, exact Git revision and tree, then runs the complete Server gates and
real stdio/HTTP integration suite. `rust-toolchain.toml` pins the local toolchain
to `1.98.1`, which must exactly match `ci/rust-version`; hosted CI installs and
selects that version explicitly.

Verification, package checking, package extraction builds, and every release
build use separate temporary Cargo target directories. A packaged crate can
therefore never replace a sibling working-tree artifact with the same package
name and development version.

The Server crate has only a binary target, so its gate runs rustdoc with
warnings denied but has no library doctest target.

The GitHub adapter requires the repository variable
`SPECMESH_ENGINE_REPOSITORY`. It checks that repository out beside the Server
at the exact revision read from the compatibility record; no repository URL or
mutable Engine ref is embedded in this repository.

The development manifest retains its relative sibling path. Local delivery uses
`local_snapshot.py` to export the recorded Engine commit and current Server commit
from local Git into independent sibling directories. It verifies the Engine tree,
Git archive commit identity, and the dependency path returned by locked offline
Cargo metadata. Release builds run from the exported Server directory and compile
the exported Engine. After building, the export contents and Cargo path are checked
again. `metadata/source-lock.json` records both commits/trees, export SHA-256 values,
the relative Engine manifest path and both lockfile hashes. The Engine identity is
fixed by the snapshot checks; Cargo.lock fixes the resolved third-party graph, not
the path dependency's revision.

`verify-package.sh` also prepares these snapshots. It creates a crate with
`cargo package --no-verify`, extracts that exact crate, patches only the temporary
manifest to the fixed Engine snapshot, verifies Cargo's actual resolution, and
performs a locked offline release build. This checks the local package contents;
it does not upload or claim a registry-ready package.

Local bundle and package verification strictly require clean Engine and Server
trees, including untracked files. `SPECMESH_ALLOW_DIRTY=1` may still support
development compatibility checks but never bypasses snapshot preparation. Before
committing infrastructure changes, run the focused Python tests and development
Rust checks; the final full gate follows the Server commit. The snapshot tests use
existing local commits only and never create commits.

No remote address is required for local delivery. Dependencies must already be
available locally for the offline steps (`cargo fetch --locked` can prepare them).
The binary bundle does not promise portable offline source rebuilding; online
publication and a complete offline source distribution are separate scopes.

## Release build prerequisites and retained evidence

The scripts support Linux and macOS hosts with the pinned Rust toolchain, Bash, Git,
and Python 3.11 or newer. No GNU tar, GNU sha256sum, or Python packages are required.
GitHub CI selects Python 3.12. Locally, `python3` is the default; when it is older,
set `SPECMESH_PYTHON=/absolute/path/to/python3.12` for the invocation. An unsuitable
interpreter fails before any build, with an actionable message.

On macOS, `build-env.sh` derives the selected compiler's sysroot and prepends its
`lib` directory to the build process's `DYLD_LIBRARY_PATH`, preserving any existing
value. It runs the selected toolchain's `rust-objcopy --version` and fails if that
probe fails. This repairs the pinned toolchain's libLLVM loader search without
changing Rust installations, shell profiles, stripping settings, or hiding warnings.

Bundles use the Python standard library to write sorted USTAR members with fixed
owner/group, permissions and Git-commit timestamps, then gzip with no filename or
wall-clock timestamp. Symlinks are archived as links rather than followed. The
regression checks inspect member contents, paths, permissions, links and gzip/tar
metadata; full verification also extracts both actual bundles and runs their binaries.
Reproducibility means two builds of the same source with the same host/toolchain;
it is not a promise that binaries for different targets or toolchains have equal hashes.

Build metadata contains the Git commit and tree, source SHA-256, clean state, Python
version, binary and lockfile hashes, and the source-lock hash. Bundle verification
also compares the recorded export hashes against fresh Git exports. Earlier dirty
preflight evidence remains historical; it is never relabeled as a clean delivery.

Set `SPECMESH_BUNDLE_OUTPUT_DIR` to an existing directory outside the repository to
retain the two verified bundles in a unique child directory; otherwise verification
removes its temporary bundles as before. Record the command, environment selection,
exit status and complete log alongside those artifacts. Final release verification
requires committed, clean sources and no `SPECMESH_ALLOW_DIRTY` override.
