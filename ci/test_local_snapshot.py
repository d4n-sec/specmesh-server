#!/usr/bin/env python3
"""Exercise local delivery against existing local commits; never create commits."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import engine_compat
import local_snapshot


class LocalSnapshotTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="specmesh-source-test-")
        cls.root = Path(cls.temporary.name)
        cls.server_source = Path(__file__).resolve().parent.parent
        cls.engine_source = Path(os.environ.get(
            "SPECMESH_ENGINE_DIR", cls.server_source.parent / "specmesh-engine")).resolve()
        for name, source in (("server", cls.server_source), ("engine", cls.engine_source)):
            destination = cls.root / name
            subprocess.run(["git", "clone", "--quiet", "--no-checkout", "--shared",
                            str(source), str(destination)], check=True)
            subprocess.run(["git", "-C", str(destination), "checkout", "--quiet",
                            "--detach", "HEAD"], check=True)
        cls.server, cls.engine = cls.root / "server", cls.root / "engine"
        cls.revision = local_snapshot.git(cls.engine, "rev-parse", "HEAD")
        cls.tree = local_snapshot.git(cls.engine, "rev-parse", "HEAD^{tree}")

    @classmethod
    def tearDownClass(cls):
        cls.temporary.cleanup()

    def prepare(self):
        output = self.root / self.id().rsplit(".", 1)[1]
        record = local_snapshot.prepare(self.server, self.engine, self.revision, self.tree, output)
        return output, record

    def test_export_and_changed_source(self):
        output, record = self.prepare()
        self.assertEqual(record["engine_revision"], self.revision)
        self.assertEqual(record["engine_tree"], self.tree)
        subprocess.run([sys.executable, str(Path(local_snapshot.__file__)), "verify",
                        "--output", str(output), "--server", str(self.server),
                        "--engine", str(self.engine)], check=True)
        source = output / "specmesh-engine/src/lib.rs"
        source.write_bytes(source.read_bytes() + b"\n// changed fixture\n")
        with self.assertRaisesRegex(ValueError, "snapshot changed"):
            local_snapshot.verify_prepared(output)

    def test_wrong_pin_and_dirty_sources(self):
        for revision, tree in (("0" * 40, self.tree), (self.revision, "0" * 40)):
            with self.assertRaises(ValueError):
                local_snapshot.prepare(self.server, self.engine, revision, tree, self.root / "rejected")
        with patch.dict(os.environ, {"SPECMESH_ALLOW_DIRTY": "1"}):
            for source in (self.engine, self.server):
                marker = source / "untracked-test-file"
                marker.write_text("fixture")
                try:
                    with self.assertRaisesRegex(ValueError, "clean working tree"):
                        local_snapshot.prepare(self.server, self.engine, self.revision,
                                               self.tree, self.root / "rejected")
                finally:
                    marker.unlink()

    def test_cargo_resolves_wrong_source(self):
        output, _ = self.prepare()
        server, engine = output / "specmesh-server", output / "specmesh-engine"
        manifest = server / "Cargo.toml"
        manifest.write_text(manifest.read_text().replace(
            'path = "../specmesh-engine"', "path = " + json.dumps(str(self.engine))))
        with self.assertRaisesRegex(ValueError, "pinned independent snapshot"):
            local_snapshot.verify_cargo_path(server, engine)

    def test_compatibility_commit_and_tree(self):
        compatibility = engine_compat.load_compatibility(self.server_source)
        engine_compat.verify(self.server_source, compatibility)
        for key in ("revision", "tree"):
            with self.subTest(key=key), self.assertRaises(SystemExit):
                engine_compat.verify(self.server_source, {**compatibility, key: "0" * 40})


if __name__ == "__main__":
    unittest.main()
