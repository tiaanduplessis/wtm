"""Exercise the release installer's transport, integrity, and replacement behavior."""

import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parent.parent


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="wtm-install-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.tools = self.root / "tools"
        self.assets = self.root / "assets"
        self.destination = self.root / "install with spaces"
        self.tools.mkdir()
        self.assets.mkdir()
        self.destination.mkdir()
        self.existing = self.destination / "wtm"
        self.existing.write_text("previous installed binary")
        self.env = {**os.environ, "PATH": str(self.tools) + os.pathsep + os.environ["PATH"],
                    "FIXTURE_ASSETS": str(self.assets), "FIXTURE_GIT": shutil.which("git"),
                    "FIXTURE_SYSTEM": "Darwin", "FIXTURE_ARCH": "arm64",
                    "FIXTURE_TAG": "v0.1.0", "TMPDIR": str(self.root),
                    "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
        self.script("uname", '''#!/bin/sh
            case "$1" in -s) echo "$FIXTURE_SYSTEM";; -m) echo "$FIXTURE_ARCH";; esac
        ''')
        self.script("git", '''#!/bin/sh
            if [ "$1" = --version ] && [ -n "${FIXTURE_GIT_VERSION:-}" ]; then
                printf 'git version %s\\n' "$FIXTURE_GIT_VERSION"
            else
                exec "$FIXTURE_GIT" "$@"
            fi
        ''')
        self.script("curl", '''#!/usr/bin/env python3
            import os, pathlib, shutil, sys
            args = sys.argv[1:]
            url = args[-1]
            if os.environ.get("FIXTURE_DOWNLOAD_FAILURE"):
                sys.exit(22)
            if "--write-out" in args:
                print("https://github.com/tiaanduplessis/wtm/releases/tag/" + os.environ["FIXTURE_TAG"], end="")
            else:
                destination = args[args.index("--output") + 1]
                shutil.copyfile(pathlib.Path(os.environ["FIXTURE_ASSETS"]) / url.rsplit("/", 1)[1], destination)
        ''')
        self.prepare("aarch64-apple-darwin")

    def script(self, name, content):
        file = self.tools / name
        first, body = content.split("\n", 1)
        file.write_text(first + "\n" + textwrap.dedent(body).strip() + "\n")
        file.chmod(0o755)

    def prepare(self, target, binary=None, extras=()):
        asset = self.assets / f"wtm-v0.1.0-{target}.tar.gz"
        content = binary or b"#!/bin/sh\nprintf 'worktree-manager 0.1.0\\n'\n"
        with tarfile.open(asset, "w:gz") as tar:
            member = tarfile.TarInfo("wtm")
            member.mode = 0o755
            member.size = len(content)
            tar.addfile(member, io.BytesIO(content))
            for name, content in extras:
                member = tarfile.TarInfo(name)
                member.size = len(content)
                tar.addfile(member, io.BytesIO(content))
        digest = hashlib.sha256(asset.read_bytes()).hexdigest()
        (self.assets / "SHA256SUMS").write_text(f"{digest}  {asset.name}\n")
        return asset

    def install(self, *args):
        return subprocess.run(["sh", str(ROOT / "install.sh"), "--dir", str(self.destination), *args],
                              env=self.env, text=True, capture_output=True)

    def unchanged(self):
        self.assertEqual(self.existing.read_text(), "previous installed binary")
        self.assertEqual(list(self.root.glob("wtm-install.*")), [])
        self.assertEqual(list(self.destination.glob(".wtm-install.*")), [])

    def test_latest_and_pinned_versions_install_without_changing_shell_files(self):
        for args in [(), ("--version", "v0.1.0")]:
            with self.subTest(args=args):
                output = self.install(*args)
                self.assertEqual(output.returncode, 0, output.stderr)
                self.assertTrue(os.access(self.existing, os.X_OK))
                self.assertEqual(subprocess.check_output([str(self.existing), "--version"], text=True).strip(),
                                 "worktree-manager 0.1.0")
                self.assertEqual(list(self.root.glob("wtm-install.*")), [])
                self.assertFalse((self.root / ".zshrc").exists())

    def test_all_supported_platforms_resolve_the_right_archive(self):
        for system, architecture, target in [
            ("Darwin", "arm64", "aarch64-apple-darwin"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
            ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
            ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
        ]:
            with self.subTest(system=system, architecture=architecture):
                self.env.update(FIXTURE_SYSTEM=system, FIXTURE_ARCH=architecture)
                self.prepare(target)
                output = self.install("--version", "v0.1.0")
                self.assertEqual(output.returncode, 0, output.stderr)

    def test_corrupt_archive_and_ambiguous_or_malformed_checksums_preserve_existing_install(self):
        asset = self.assets / "wtm-v0.1.0-aarch64-apple-darwin.tar.gz"
        original = asset.read_bytes()
        asset.write_bytes(original + b"corruption")
        output = self.install()
        self.assertIn("checksum mismatch", output.stderr)
        self.unchanged()
        for checksum in ["", "bad  " + asset.name + "\n",
                         (hashlib.sha256(original).hexdigest() + "  " + asset.name + "\n") * 2]:
            with self.subTest(checksum=checksum):
                asset.write_bytes(original)
                (self.assets / "SHA256SUMS").write_text(checksum)
                self.assertNotEqual(self.install().returncode, 0)
                self.unchanged()

    def test_wrong_version_and_failed_download_preserve_existing_install(self):
        self.prepare("aarch64-apple-darwin", b"#!/bin/sh\necho 'worktree-manager 0.2.0'\n")
        output = self.install()
        self.assertIn("version does not match", output.stderr)
        self.unchanged()
        self.env["FIXTURE_DOWNLOAD_FAILURE"] = "1"
        self.assertNotEqual(self.install().returncode, 0)
        self.unchanged()

    def test_archive_paths_cannot_write_outside_the_binary(self):
        escaped = self.root / "escaped"
        self.prepare("aarch64-apple-darwin", extras=[("../../escaped", b"must not be extracted")])
        output = self.install()
        self.assertEqual(output.returncode, 0, output.stderr)
        self.assertFalse(escaped.exists())

    def test_old_git_unknown_platform_and_invalid_tags_fail_before_install(self):
        for key, value, expected in [("FIXTURE_GIT_VERSION", "2.35.9", "Git 2.36"),
                                     ("FIXTURE_ARCH", "armv7l", "install from source"),
                                     ("FIXTURE_TAG", "v0.1.0;evil", "stable tag")]:
            with self.subTest(key=key):
                self.env[key] = value
                self.assertIn(expected, self.install().stderr)
                self.unchanged()
                self.env.pop(key)
                self.env.update(FIXTURE_SYSTEM="Darwin", FIXTURE_ARCH="arm64", FIXTURE_TAG="v0.1.0")

    def test_native_binary_install_and_quickstart(self):
        target_directory = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
        binary = target_directory / "debug" / "wtm"
        if not binary.is_file():
            self.skipTest("run cargo test first to build the native CLI")
        self.prepare("aarch64-apple-darwin", binary.read_bytes())
        output = self.install()
        self.assertEqual(output.returncode, 0, output.stderr)
        repo = self.root / "example"
        subprocess.run([self.env["FIXTURE_GIT"], "init", "-b", "main", str(repo)],
                       check=True, env=self.env, capture_output=True)
        listing = subprocess.run([str(self.existing), "list", str(repo), "--json"], env=self.env,
                                 capture_output=True, text=True)
        self.assertEqual(listing.returncode, 0, listing.stderr)
        self.assertIn('"discovery_complete": true', listing.stdout)


if __name__ == "__main__":
    unittest.main()
