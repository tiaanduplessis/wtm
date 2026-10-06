"""Verify release completeness, native architecture checks, and reproducible archives."""

import hashlib
import importlib.util
import io
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("release", ROOT / "scripts/release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def native_header(target):
    if target.endswith("apple-darwin"):
        cpu = 0x0100000C if target.startswith("aarch64") else 0x01000007
        return b"\xcf\xfa\xed\xfe" + struct.pack("<I", cpu) + bytes(24)
    header = bytearray(32)
    header[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", header, 18, 183 if target.startswith("aarch64") else 62)
    return bytes(header)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="wtm-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
        (self.root / "CHANGELOG.md").write_text("# Changelog\n## Unreleased\n## 0.1.0\n- First release.\n")
        for name in ["README.md", "LICENSE"]:
            (self.root / name).write_text(name)
        self.output = self.root / "dist"
        self.output.mkdir()

    def archive(self, target, bad_member=None, architecture=None):
        file = self.output / release.archive_name("v0.1.0", target)
        with tarfile.open(file, "w:gz") as tar:
            for name in ["wtm", "LICENSE", "README.md", "CHANGELOG.md", "THIRD_PARTY_NOTICES.txt"]:
                content = native_header(architecture or target) if name == "wtm" else name.encode()
                member = tarfile.TarInfo(name)
                member.size = len(content)
                if name == bad_member:
                    member.type = tarfile.SYMTYPE
                    member.linkname = "/outside"
                    member.size = 0
                tar.addfile(member, io.BytesIO(content) if member.isfile() else None)
        return file

    def test_tags_and_changelog_require_matching_stable_versions(self):
        self.assertEqual(release.validate_tag("v0.1.0", self.root), "0.1.0")
        self.assertIn("First release", release.release_notes("v0.1.0", self.root))
        for tag in ["v0.2.0", "v0.1.0-rc1", "0.1.0", "v00.1.0", "v0.1.0; echo bad"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.validate_tag(tag, self.root)
        (self.root / "CHANGELOG.md").write_text("# Changelog\n## Unreleased\n- Not versioned.\n")
        with self.assertRaisesRegex(ValueError, "versioned changelog"):
            release.release_notes("v0.1.0", self.root)

    def test_missing_extra_and_wrong_architecture_archives_cannot_get_checksums(self):
        for target in release.TARGETS[:-1]:
            self.archive(target)
        with self.assertRaisesRegex(ValueError, "exactly one archive"):
            release.checksums("v0.1.0", self.output, self.root)
        self.archive(release.TARGETS[-1], architecture=release.TARGETS[0])
        with self.assertRaisesRegex(ValueError, "wrong binary architecture"):
            release.checksums("v0.1.0", self.output, self.root)
        self.archive(release.TARGETS[-1])
        (self.output / "extra.tar.gz").write_bytes(b"unexpected")
        with self.assertRaisesRegex(ValueError, "exactly one archive"):
            release.checksums("v0.1.0", self.output, self.root)

    def test_symlinks_are_rejected_and_complete_set_hashes_actual_bytes(self):
        for target in release.TARGETS:
            self.archive(target)
        self.archive(release.TARGETS[0], bad_member="wtm")
        with self.assertRaisesRegex(ValueError, "regular files"):
            release.checksums("v0.1.0", self.output, self.root)
        self.archive(release.TARGETS[0])
        file = release.checksums("v0.1.0", self.output, self.root)
        entries = file.read_text().splitlines()
        self.assertEqual(len(entries), 4)
        for entry in entries:
            digest, name = entry.split()
            self.assertEqual(digest, hashlib.sha256((self.output / name).read_bytes()).hexdigest())

    def test_packaging_refuses_wrong_target_or_version_and_is_reproducible(self):
        binary = self.root / "wtm"
        binary.write_bytes(native_header(release.TARGETS[0]))
        with self.assertRaisesRegex(ValueError, "architecture"):
            release.package("v0.1.0", release.TARGETS[1], binary, self.output, self.root)
        with patch.object(release.subprocess, "check_output", return_value="worktree-manager 0.2.0"):
            with self.assertRaisesRegex(ValueError, "version"):
                release.package("v0.1.0", release.TARGETS[0], binary, self.output, self.root)
        with patch.object(release.subprocess, "check_output", return_value="worktree-manager 0.1.0"), \
                patch.object(release, "dependency_notices", return_value=b"source notices"):
            file = release.package("v0.1.0", release.TARGETS[0], binary, self.output, self.root)
            first = file.read_bytes()
            release.package("v0.1.0", release.TARGETS[0], binary, self.output, self.root)
            self.assertEqual(first, file.read_bytes())
            with tarfile.open(file) as tar:
                self.assertEqual(tar.getmember("wtm").mode, 0o755)
                self.assertEqual(tar.extractfile("THIRD_PARTY_NOTICES.txt").read(), b"source notices")


if __name__ == "__main__":
    unittest.main()
