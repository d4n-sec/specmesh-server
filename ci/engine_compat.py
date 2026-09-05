#!/usr/bin/env python3
"""Read and verify the pinned sibling Engine development snapshot."""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path


REVISION_PATTERN = re.compile(r"^[0-9a-f]{40}$")


def fail(message: str) -> None:
    raise SystemExit(f"Engine compatibility check failed: {message}")


def read_toml(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def git(engine_dir: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(engine_dir), *arguments],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    )
    return result.stdout.strip()


def load_compatibility(server_root: Path) -> dict:
    compatibility = read_toml(server_root / "compatibility/engine.toml")
    if compatibility.get("schema") != 1:
        fail("compatibility schema must be 1")
    if compatibility.get("package") != "specmesh-engine":
        fail("compatibility package must be specmesh-engine")
    if not REVISION_PATTERN.fullmatch(str(compatibility.get("revision", ""))):
        fail("compatibility revision must be a full lowercase Git SHA")
    return compatibility


def verify(server_root: Path, compatibility: dict) -> None:
    server_manifest = read_toml(server_root / "Cargo.toml")
    dependency = server_manifest.get("dependencies", {}).get("specmesh-engine")
    if not isinstance(dependency, dict):
        fail("Server Cargo.toml must use a structured specmesh-engine dependency")

    engine_dir = Path(
        os.environ.get("SPECMESH_ENGINE_DIR", server_root.parent / "specmesh-engine")
    ).resolve()
    dependency_path = (server_root / str(dependency.get("path", ""))).resolve()
    if dependency_path != engine_dir:
        fail(f"Cargo path {dependency_path} does not match Engine directory {engine_dir}")

    engine_manifest = read_toml(engine_dir / "Cargo.toml")
    engine_package = engine_manifest.get("package", {})
    if engine_package.get("name") != compatibility["package"]:
        fail("Engine Cargo package name does not match compatibility record")
    if engine_package.get("version") != compatibility["version"]:
        fail("Engine Cargo version does not match compatibility record")
    if dependency.get("version") != f'={compatibility["version"]}':
        fail("Server Engine dependency must use the compatibility record's exact version")

    actual_revision = git(engine_dir, "rev-parse", "HEAD")
    if actual_revision != compatibility["revision"]:
        fail(
            f"Engine HEAD is {actual_revision}; expected {compatibility['revision']}"
        )
    if git(engine_dir, "status", "--porcelain", "--untracked-files=normal"):
        if os.environ.get("SPECMESH_ALLOW_DIRTY") != "1":
            fail("Engine working tree is dirty")

    print(
        f"verified {compatibility['package']} {compatibility['version']} "
        f"at {compatibility['revision']}"
    )


def main() -> None:
    server_root = Path(__file__).resolve().parent.parent
    compatibility = load_compatibility(server_root)
    command = sys.argv[1] if len(sys.argv) > 1 else "verify"
    if command == "revision":
        print(compatibility["revision"])
    elif command == "verify":
        verify(server_root, compatibility)
    else:
        fail(f"unknown command {command!r}")


if __name__ == "__main__":
    main()
