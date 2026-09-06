# SpecMesh Server

`specmesh-server` is the authenticated, loopback-only Streamable HTTP MCP
adapter for SpecMesh. Domain behavior, Workspace storage, tool schemas,
`ActionResult`, cancellation, progress, and output coordination come from the
`specmesh-engine` library; this repository owns only the HTTP process and
transport boundary.

## Run

Create a 32-byte random bearer token as unpadded base64url in an absolute file
outside the Workspace:

```sh
umask 077
openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n' > /absolute/path/specmesh-token
```

Then start one foreground process for one Workspace:

```sh
cargo run -- \
  --workspace /absolute/path/workspace \
  --port 8765 \
  --token-file /absolute/path/specmesh-token
```

Omit `--workspace` to bind the process startup directory. The binding is
canonicalized once and never changes for that process.

The only MCP endpoint is `http://127.0.0.1:8765/mcp`. The host is fixed and
there is no daemon mode, TLS, public-network listener, session recovery, CLI
Action surface, or stdio MCP surface.

Host, Origin, and Bearer checks precede method handling: a missing or invalid
Host receives 403, an otherwise valid unauthenticated GET receives 401, and an
authenticated GET receives 405. At most 32 tool calls are active at once; the
next call receives HTTP 200 with JSON-RPC error `-32000`.

## Develop

```sh
bash ci/verify.sh
```

The integration suite builds the sibling Engine's real `specmesh mcp` binary
under this repository's ignored `target/stdio-engine` directory. It compares
the complete tool list and typed ActionResult wire across direct and complete
file output, including confirmation, failure, cancellation, nested Impact, and
RuleReview data, without using a direct service call as a transport substitute.

During sibling-repository development, `Cargo.toml` uses an exact `=0.0.0`
Engine version plus `../specmesh-engine`. The exact Engine commit and tree are
recorded in [`compatibility/engine.toml`](compatibility/engine.toml), and
`ci/verify.sh` rejects a sibling Engine whose package version, Git revision or tree
does not match that file. `Cargo.lock` fixes the resolved dependency graph, but
does not hash or pin the contents of a path dependency.

For local binary delivery, the build exports the recorded Engine commit and the
current Server commit into independent sibling directories. It verifies Cargo's
actual Engine path, compiles those snapshots, and checks that their files remain
unchanged. Both original repositories must be clean; `SPECMESH_ALLOW_DIRTY` cannot
bypass local delivery checks. No remote repository or package upload is required.
Online publication remains a separate release task requiring an exact package
version or immutable revision.

To create a local deterministic binary bundle without publishing it:

```sh
bash ci/release-bundle.sh target/release-bundles
```

The bundle contains the Server binary, README, Apache-2.0 license, lockfile,
Engine compatibility record, build metadata, and `metadata/source-lock.json`
with commit/tree, Git export hashes and lockfile hashes. `ci/verify.sh` creates it
twice and requires identical SHA-256 values.

This binary bundle does not include all third-party sources and is not an offline
source distribution. Building it requires the dependencies to be available in the
local Cargo cache; `cargo fetch --locked` can prepare them when needed.

Release builds and verification require Python 3.11+ in addition to the pinned Rust
toolchain. The portable bundle format, macOS LLVM loader handling, interpreter
selection, and retained verification artifacts are documented in [ci/README.md](ci/README.md).
