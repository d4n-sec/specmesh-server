#!/usr/bin/env python3
"""Prepare an immutable local Engine/Server build; no commits or network operations."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile
import tomllib


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()


def require_source(root: Path, revision: str, tree: str) -> None:
    if git(root, "rev-parse", "HEAD") != revision:
        raise ValueError(f"{root.name}: HEAD does not match pinned revision")
    if git(root, "rev-parse", revision + "^{tree}") != tree:
        raise ValueError(f"{root.name}: tree does not match pinned tree")
    if git(root, "status", "--porcelain", "--untracked-files=normal"):
        raise ValueError(f"{root.name}: local delivery requires a clean working tree")


def export(root: Path, revision: str, destination: Path) -> str:
    destination.mkdir()
    archive = destination.parent / (destination.name + ".tar")
    subprocess.run(["git", "-C", str(root), "archive", "--format=tar",
                    "--output", str(archive), revision], check=True)
    with archive.open("rb") as source:
        archived_revision = subprocess.check_output(
            ["git", "get-tar-commit-id"], stdin=source, text=True
        ).strip()
    if archived_revision != revision:
        raise ValueError("Git archive does not identify the pinned commit")
    with tarfile.open(archive) as tar:
        for item in tar.getmembers():
            path = Path(item.name)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("unexpected path in Git export")
            if item.issym() or item.islnk():
                target = Path(item.linkname)
                if target.is_absolute() or ".." in target.parts:
                    raise ValueError("Git export link escapes the local snapshot")
        tar.extractall(destination, **({"filter": "data"} if hasattr(tarfile, "data_filter") else {}))
    return hashlib.sha256(archive.read_bytes()).hexdigest()


def verify_cargo_path(server: Path, engine: Path) -> None:
    host = next(line.split(": ", 1)[1] for line in subprocess.check_output(
        ["rustc", "-vV"], text=True, cwd=server).splitlines() if line.startswith("host: "))
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
         "--filter-platform", host, "--manifest-path", str(server / "Cargo.toml")],
        text=True, cwd=server,
    ))
    packages = {item["id"]: item for item in metadata["packages"]}
    server_id = next(key for key, item in packages.items()
                     if Path(item["manifest_path"]).resolve() == (server / "Cargo.toml").resolve())
    server_node = next(node for node in metadata["resolve"]["nodes"] if node["id"] == server_id)
    engines = [packages[dep["pkg"]] for dep in server_node["deps"]
               if packages[dep["pkg"]]["name"] == "specmesh-engine"]
    if len(engines) != 1 or Path(engines[0]["manifest_path"]).resolve() != (engine / "Cargo.toml").resolve():
        raise ValueError("Cargo did not resolve Server's Engine to the pinned independent snapshot")
    expected = tomllib.loads((engine / "Cargo.toml").read_text())["package"]["version"]
    if engines[0]["version"] != expected or engines[0]["source"] is not None:
        raise ValueError("Cargo Engine identity differs from the local snapshot")


def prepare(server_root: Path, engine_root: Path, revision: str, tree: str, output: Path) -> dict:
    server_root, engine_root, output = server_root.resolve(), engine_root.resolve(), output.resolve()
    server_revision = git(server_root, "rev-parse", "HEAD")
    server_tree = git(server_root, "rev-parse", "HEAD^{tree}")
    require_source(server_root, server_revision, server_tree)
    require_source(engine_root, revision, tree)
    output.mkdir()  # Never reuse or overwrite another build's snapshots.
    engine = output / "specmesh-engine"
    server = output / "specmesh-server"
    engine_archive = export(engine_root, revision, engine)
    server_archive = export(server_root, server_revision, server)
    verify_cargo_path(server, engine)
    # Reject source changes during preparation, even though exports are immutable.
    require_source(server_root, server_revision, server_tree)
    require_source(engine_root, revision, tree)
    record = {
        "kind": "local-git-snapshot",
        "engine_revision": revision,
        "engine_tree": tree,
        "engine_archive_sha256": engine_archive,
        "server_revision": server_revision,
        "server_tree": server_tree,
        "server_archive_sha256": server_archive,
        "cargo_engine_manifest": "specmesh-engine/Cargo.toml",
        "cargo_lock_sha256": hashlib.sha256((server / "Cargo.lock").read_bytes()).hexdigest(),
        "engine_cargo_lock_sha256": hashlib.sha256((engine / "Cargo.lock").read_bytes()).hexdigest(),
    }
    (output / "source-lock.json").write_text(json.dumps(record, indent=2) + "\n")
    return record


def verify_prepared(output: Path, server_root: Path | None = None,
                    engine_root: Path | None = None) -> None:
    """Reject altered exports or Cargo resolution before/after the actual build."""
    output = output.resolve()
    record = json.loads((output / "source-lock.json").read_text())
    for name, origin in (("server", server_root), ("engine", engine_root)):
        if origin is not None:
            require_source(origin, record[name + "_revision"], record[name + "_tree"])
    for name in ("engine", "server"):
        directory = output / ("specmesh-" + name)
        archive = output / (directory.name + ".tar")
        if hashlib.sha256(archive.read_bytes()).hexdigest() != record[name + "_archive_sha256"]:
            raise ValueError("source archive changed after preparation")
        with tarfile.open(archive) as tar:
            expected = set()
            for item in tar.getmembers():
                path = directory / item.name
                expected.add(path.relative_to(directory).as_posix())
                if item.isdir():
                    valid = path.is_dir() and not path.is_symlink()
                elif item.issym():
                    valid = path.is_symlink() and str(path.readlink()) == item.linkname
                else:
                    valid = (item.isfile() and path.is_file() and not path.is_symlink()
                             and path.read_bytes() == tar.extractfile(item).read()
                             and (path.stat().st_mode & 0o111) == (item.mode & 0o111))
                if not valid:
                    raise ValueError(f"source snapshot changed: {name}/{item.name}")
            if {path.relative_to(directory).as_posix() for path in directory.rglob("*")} != expected:
                raise ValueError("source snapshot contains added or missing files")
    verify_cargo_path(output / "specmesh-server", output / "specmesh-engine")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    preparation = commands.add_parser("prepare")
    preparation.add_argument("--server", type=Path, required=True)
    preparation.add_argument("--engine", type=Path, required=True)
    preparation.add_argument("--revision", required=True)
    preparation.add_argument("--tree", required=True)
    preparation.add_argument("--output", type=Path, required=True)
    verification = commands.add_parser("verify")
    verification.add_argument("--output", type=Path, required=True)
    verification.add_argument("--server", type=Path, required=True)
    verification.add_argument("--engine", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "prepare":
        print(json.dumps(prepare(args.server, args.engine, args.revision, args.tree, args.output), indent=2))
    else:
        verify_prepared(args.output, args.server, args.engine)
