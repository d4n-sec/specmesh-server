#!/usr/bin/env python3
"""Regression tests for the archive format, independent of GNU tar and Cargo."""
import os
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest

from bundle import archive, sha256


class BundleTests(unittest.TestCase):
    def test_archive_normalizes_metadata_and_preserves_contents_and_links(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outputs = []
            for index in (1, 2):
                stage = root / str(index) / "bundle"
                (stage / "bin").mkdir(parents=True)
                (stage / "docs").mkdir()
                (stage / "README.md").write_bytes(b"bundle readme\n")
                (stage / "docs" / "guide.md").write_bytes(b"guide\n")
                binary = stage / "bin" / "tool"
                binary.write_bytes(b"#!/bin/sh\nexit 0\n")
                binary.chmod(0o755)
                (stage / "guide").symlink_to("docs/guide.md")
                # Preserve a link, never copy its target into an archive member.
                (stage / "outside").symlink_to("../outside")
                for path in (stage, *stage.rglob("*")):
                    os.utime(path, (100 + index, 100 + index), follow_symlinks=False)
                destination = root / f"bundle-{index}.tar.gz"
                archive(stage, destination, 1234567890)
                outputs.append(destination)
            self.assertEqual(sha256(outputs[0]), sha256(outputs[1]))
            header = outputs[0].read_bytes()[:10]
            self.assertEqual(header[:4], b"\x1f\x8b\x08\x00")
            self.assertEqual(struct.unpack("<I", header[4:8])[0], 0)
            self.assertEqual(header[9], 255)
            with tarfile.open(outputs[0]) as tar:
                members = tar.getmembers()
                self.assertEqual(
                    [member.name for member in members],
                    ["bundle", "bundle/README.md", "bundle/bin", "bundle/bin/tool",
                     "bundle/docs", "bundle/docs/guide.md", "bundle/guide", "bundle/outside"],
                )
                for member in members:
                    self.assertEqual((member.uid, member.gid, member.uname, member.gname),
                                     (0, 0, "", ""))
                    self.assertEqual(member.mtime, 1234567890)
                    self.assertEqual(member.pax_headers, {})
                self.assertEqual(tar.getmember("bundle/bin/tool").mode, 0o755)
                self.assertEqual(tar.getmember("bundle/docs/guide.md").mode, 0o644)
                self.assertEqual(tar.extractfile("bundle/README.md").read(), b"bundle readme\n")
                self.assertEqual(tar.extractfile("bundle/bin/tool").read(), b"#!/bin/sh\nexit 0\n")
                self.assertEqual(tar.getmember("bundle/guide").linkname, "docs/guide.md")
                self.assertTrue(tar.getmember("bundle/outside").issym())
                self.assertEqual(tar.getmember("bundle/outside").linkname, "../outside")

    def test_invalid_epoch_does_not_create_archive(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "invalid.tar.gz"
            with self.assertRaises(ValueError):
                archive(root, output, -1)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
