# Server verification and packaging

`verify.sh` is the provider-neutral verification entry point used locally and
by CI. It first checks `compatibility/engine.toml` against the sibling Engine's
Cargo package and exact Git revision, then runs the complete Server gates and
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

The unreleased development manifest intentionally uses a sibling path
dependency. Consequently `verify-package.sh` asks `cargo package` to create the
crate without its normal registry-based verification, extracts that exact
crate, patches only the temporary copy back to the verified sibling Engine,
and performs a locked offline release build. A future published package must
instead use the exact package version or immutable revision required by the
accepted Server boundary.

The default requires clean Engine and Server trees. `SPECMESH_ALLOW_DIRTY=1`
exists only to validate infrastructure changes before commit; any bundle made
in that mode records `working_tree=dirty` and is not a release artifact.
