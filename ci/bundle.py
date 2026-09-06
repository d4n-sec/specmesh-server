#!/usr/bin/env python3
"""Portable deterministic binary-bundle primitives; Python 3.11+ standard library."""
from __future__ import annotations

import gzip
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tarfile


def sha256(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def source_sha256(root: Path) -> str:
    names = subprocess.check_output(
        ["git", "-C", str(root), "ls-files", "-z", "--cached", "--others", "--exclude-standard"]
    ).split(b"\0")
    digest = hashlib.sha256()
    for name in sorted(set(filter(None, names))):
        path = root / os.fsdecode(name)
        digest.update(name + b"\0")
        if path.is_symlink():
            digest.update(b"symlink\0" + os.fsencode(os.readlink(path)))
        elif path.is_file():
            digest.update(b"executable\0" if path.stat().st_mode & 0o111 else b"file\0")
            digest.update(bytes.fromhex(sha256(path)))
        elif not path.exists():
            digest.update(b"deleted\0")
        else:
            raise ValueError(f"unsupported source entry: {path}")
        digest.update(b"\0")
    return digest.hexdigest()


def archive(stage: Path, destination: Path, epoch: int) -> None:
    if epoch < 0:
        raise ValueError("source epoch must be nonnegative")
    paths = [stage, *sorted(stage.rglob("*"), key=lambda item: item.relative_to(stage).as_posix())]
    with destination.open("wb") as output:
        # An empty gzip filename and mtime=0 avoid host paths and wall-clock time.
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0, compresslevel=9) as zipped:
            with tarfile.open(fileobj=zipped, mode="w|", format=tarfile.USTAR_FORMAT) as tar:
                for path in paths:
                    if not (path.is_symlink() or path.is_file() or path.is_dir()):
                        raise ValueError(f"unsupported bundle entry: {path}")
                    info = tar.gettarinfo(str(path), arcname=path.relative_to(stage.parent).as_posix())
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    info.mtime = epoch
                    info.mode = 0o777 if path.is_symlink() else (0o755 if path.is_dir() or path.stat().st_mode & 0o111 else 0o644)
                    if info.isfile():
                        with path.open("rb") as source:
                            tar.addfile(info, source)
                    else:
                        tar.addfile(info)


def main() -> None:
    command, *args = sys.argv[1:]
    if command == "sha256" and len(args) == 1:
        print(sha256(Path(args[0])))
    elif command == "source-sha256" and len(args) == 1:
        print(source_sha256(Path(args[0])))
    elif command == "archive" and len(args) == 3:
        archive(Path(args[0]), Path(args[1]), int(args[2]))
    else:
        raise SystemExit("usage: bundle.py sha256 FILE | source-sha256 REPO | archive STAGE OUTPUT EPOCH")


if __name__ == "__main__":
    main()
