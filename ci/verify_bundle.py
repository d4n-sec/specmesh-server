#!/usr/bin/env python3
"""Inspect and smoke-test the exact binary bundle produced by this source tree."""
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import struct
import subprocess
import sys
import tarfile
import tempfile
import tomllib

from bundle import source_sha256
from local_snapshot import BUILD_WORKSPACE, git


def verify(archive: Path, repo: Path) -> None:
    with archive.open("rb") as source:
        header = source.read(10)
    assert header[:4] == b"\x1f\x8b\x08\x00"
    assert struct.unpack("<I", header[4:8])[0] == 0
    assert header[9] == 255
    with tarfile.open(archive) as tar:
        members = tar.getmembers()
        names = [member.name for member in members]
        assert names and len(names) == len(set(names))
        root = names[0]
        assert PurePosixPath(root).name == root and root not in (".", "..")
        assert names == [root, *sorted(names[1:])]
        metadata_file = tar.extractfile(f"{root}/metadata/build.txt")
        metadata = dict(line.split("=", 1) for line in metadata_file.read().decode().splitlines())
        package = metadata["package"]
        assert package in ("specmesh-engine", "specmesh-server")
        assert root == f'{package}-v{metadata["version"]}-{metadata["target"]}'
        assert metadata["archive_format"] == "ustar+gzip-v1"
        assert metadata["source_sha256"] == source_sha256(repo)
        for member in members:
            path = PurePosixPath(member.name)
            assert not path.is_absolute() and ".." not in path.parts
            assert path.parts[0] == root
            assert (member.uid, member.gid, member.uname, member.gname) == (0, 0, "", "")
            assert member.mtime == int(metadata["source_date_epoch"])
            assert not member.pax_headers
            assert member.isdir() or member.isfile() or member.issym()
            if member.issym():
                target = path.parent / member.linkname
                assert not PurePosixPath(member.linkname).is_absolute() and ".." not in target.parts
                assert member.mode == 0o777
            elif member.isdir():
                assert member.mode == 0o755
            else:
                assert member.mode in (0o644, 0o755)
        required = ["README.md", "LICENSE", "Cargo.lock"]
        if package == "specmesh-engine":
            required.append("docs/schema-2-implementation-plan.md")
            required.extend(path.relative_to(repo).as_posix()
                            for path in sorted((repo / "skills/specmesh").rglob("*"))
                            if path.is_file())
        else:
            required.append("compatibility/engine.toml")
            engine = Path(os.environ.get("SPECMESH_ENGINE_DIR", repo.parent / "specmesh-engine")).resolve()
            compatibility = tomllib.loads((repo / "compatibility/engine.toml").read_text())
            source_lock_bytes = tar.extractfile(f"{root}/metadata/source-lock.json").read()
            source_lock = json.loads(source_lock_bytes)
            assert hashlib.sha256(source_lock_bytes).hexdigest() == metadata["source_lock_sha256"]
            assert source_lock["kind"] == metadata["dependency_source"] == "local-git-snapshot"
            assert source_lock["engine_revision"] == compatibility["revision"] == metadata["engine_revision"]
            assert source_lock["engine_tree"] == compatibility["tree"] == metadata["engine_tree"]
            assert source_lock["server_revision"] == metadata["commit"] == git(repo, "rev-parse", "HEAD")
            assert source_lock["server_tree"] == metadata["commit_tree"] == git(repo, "rev-parse", "HEAD^{tree}")
            assert source_lock["cargo_lock_sha256"] == metadata["cargo_lock_sha256"]
            assert source_lock["engine_cargo_lock_sha256"] == metadata["engine_cargo_lock_sha256"]
            assert source_lock["cargo_engine_manifest"] == "specmesh-engine/Cargo.toml"
            assert source_lock["cargo_workspace_manifest_sha256"] == hashlib.sha256(BUILD_WORKSPACE.encode()).hexdigest()
            assert metadata["working_tree"] == metadata["engine_working_tree"] == "clean"
            assert metadata["engine_source_sha256"] == source_sha256(engine)
            assert metadata["engine_cargo_lock_sha256"] == hashlib.sha256((engine / "Cargo.lock").read_bytes()).hexdigest()
            for name, source in (("engine", engine), ("server", repo)):
                assert git(source, "rev-parse", "HEAD") == source_lock[name + "_revision"]
                assert git(source, "rev-parse", "HEAD^{tree}") == source_lock[name + "_tree"]
                assert not git(source, "status", "--porcelain", "--untracked-files=normal")
                payload = subprocess.check_output(["git", "-C", str(source), "archive", "--format=tar",
                                                   source_lock[name + "_revision"]])
                assert hashlib.sha256(payload).hexdigest() == source_lock[name + "_archive_sha256"]
        for relative in required:
            assert tar.extractfile(f"{root}/{relative}").read() == (repo / relative).read_bytes(), relative
        binary = "specmesh" if package == "specmesh-engine" else "specmesh-server"
        payload = tar.extractfile(f"{root}/bin/{binary}").read()
        assert hashlib.sha256(payload).hexdigest() == metadata["binary_sha256"]
        assert tar.getmember(f"{root}/bin/{binary}").mode == 0o755
        assert hashlib.sha256((repo / "Cargo.lock").read_bytes()).hexdigest() == metadata["cargo_lock_sha256"]
        # All names and member types were checked above; retain no extracted artifact.
        with tempfile.TemporaryDirectory(prefix="specmesh-bundle-smoke-") as temporary:
            tar.extractall(temporary, **({"filter": "data"} if hasattr(tarfile, "data_filter") else {}))
            executable = Path(temporary) / root / "bin" / binary
            result = subprocess.run([str(executable), "--version"], check=True,
                                    text=True, stdout=subprocess.PIPE)
            assert result.stdout.strip() == f'{binary} {metadata["version"]}'
            subprocess.run([str(executable), "--help"], check=True, stdout=subprocess.DEVNULL)
            if package == "specmesh-engine":
                subprocess.run([str(executable), "doctor"], check=True, stdout=subprocess.DEVNULL)
    print(f"verified bundle content, metadata, permissions and executable: {archive}")


if __name__ == "__main__":
    verify(Path(sys.argv[1]), Path(sys.argv[2]).resolve())
