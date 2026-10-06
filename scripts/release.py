#!/usr/bin/env python3
"""Validate tags and assemble GitHub release archives using Python's standard library."""

import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import struct
import subprocess
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
)


def validate_tag(tag, root=ROOT):
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    if not re.fullmatch(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", tag):
        raise ValueError("release tag must be a stable version such as v0.1.0")
    if tag != "v" + manifest["package"]["version"]:
        raise ValueError("release tag does not match Cargo.toml")
    return manifest["package"]["version"]


def binary_target(data):
    if data[:4] == b"\xcf\xfa\xed\xfe" and len(data) >= 8:
        cpu = struct.unpack_from("<I", data, 4)[0]
        return {0x0100000C: TARGETS[0], 0x01000007: TARGETS[1]}.get(cpu)
    if data[:5] == b"\x7fELF\x02" and len(data) >= 20:
        endian = {1: "<", 2: ">"}.get(data[5])
        if endian:
            cpu = struct.unpack_from(endian + "H", data, 18)[0]
            return {183: TARGETS[2], 62: TARGETS[3]}.get(cpu)
    return None


def dependency_notices(target, root=ROOT):
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
        cwd=root, text=True,
    ))
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    seen = set()

    def visit(identifier):
        if identifier in seen:
            return
        seen.add(identifier)
        for dependency in nodes[identifier]["deps"]:
            if any(kind["kind"] in (None, "build") for kind in dependency["dep_kinds"]):
                visit(dependency["pkg"])

    visit(metadata["resolve"]["root"])
    sections = ["Third-party dependency notices\n\n"
                "These are package declarations and source notices from the locked dependency graph.\n"
                "Dependencies retain their own licenses; the project's MIT license does not replace them.\n"]
    dependencies = sorted((packages[identifier] for identifier in seen if packages[identifier]["source"]),
                          key=lambda package: (package["name"], package["version"]))
    for package in dependencies:
        directory = Path(package["manifest_path"]).parent
        sections.append(f"\n{'=' * 72}\n{package['name']} {package['version']}\n"
                        f"Declared license: {package['license']}\n"
                        f"Repository: {package['repository'] or 'not declared'}\n"
                        f"Authors: {', '.join(package['authors']) or 'not declared'}\n")
        files = sorted(file for file in directory.iterdir()
                       if file.is_file() and re.match(r"^(LICENSE|LICENCE|COPYING|COPYRIGHT|NOTICE)([._-]|$)",
                                                     file.name, re.IGNORECASE)
                       and file.suffix != ".rs")
        if package["license_file"]:
            declared = directory / package["license_file"]
            if declared not in files:
                files.append(declared)
        if not files:
            vcs = directory / ".cargo_vcs_info.json"
            commit = json.loads(vcs.read_text())["git"]["sha1"] if vcs.exists() else ""
            bundled = root / "licenses" / f"objc2-{commit}.md"
            if package["repository"] == "https://github.com/madsmtm/objc2" and bundled.is_file():
                files = [bundled, root / "licenses" / "MIT-reference.txt"]
            else:
                raise ValueError(f"source license notice missing for {package['name']} {package['version']}")
        for file in files:
            sections.append(f"\n--- {file.name} ---\n{file.read_text(errors='replace')}\n")
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    copyright_file = sysroot / "share/doc/rust/COPYRIGHT-library.html"
    if not copyright_file.is_file():
        raise ValueError("Rust standard-library copyright information is unavailable")
    sections.append("\nRust standard-library copyright information\n" + copyright_file.read_text())
    license_directory = sysroot / "share/doc/rust/licenses"
    for file in sorted(license_directory.glob("*.txt")):
        sections.append(f"\n--- Rust standard library: {file.name} ---\n{file.read_text()}\n")
    if "linux-musl" in target:
        sections.append("\n--- musl libc ---\n" + (root / "licenses/musl-COPYRIGHT").read_text())
    return "\n".join(sections).encode()


def archive_name(tag, target):
    if target not in TARGETS:
        raise ValueError("unsupported release target")
    return f"wtm-{tag}-{target}.tar.gz"


def package(tag, target, binary, output, root=ROOT):
    version = validate_tag(tag, root)
    binary = binary.resolve()
    data = binary.read_bytes()
    if binary_target(data) != target:
        raise ValueError("binary architecture does not match the release target")
    reported = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if reported != "worktree-manager " + version:
        raise ValueError("binary version does not match the release tag")
    members = {"wtm": data, "THIRD_PARTY_NOTICES.txt": dependency_notices(target, root)}
    for name in ("LICENSE", "README.md", "CHANGELOG.md"):
        members[name] = (root / name).read_bytes()
    output.mkdir(parents=True, exist_ok=True)
    archive = output / archive_name(tag, target)
    # Stable archive metadata makes repeated packaging of the same files reproducible.
    with archive.open("wb") as stream, gzip.GzipFile(fileobj=stream, mode="wb", filename="", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
            for name, content in members.items():
                info = tarfile.TarInfo(name)
                info.size = len(content)
                info.mode = 0o755 if name == "wtm" else 0o644
                tar.addfile(info, io.BytesIO(content))
    return archive


def checksums(tag, output, root=ROOT):
    validate_tag(tag, root)
    expected = {archive_name(tag, target) for target in TARGETS}
    found = {file.name for file in output.glob("*.tar.gz")}
    if found != expected:
        raise ValueError("release must contain exactly one archive for each supported target")
    for target in TARGETS:
        with tarfile.open(output / archive_name(tag, target)) as tar:
            members = tar.getmembers()
            if {member.name for member in members} != {"wtm", "LICENSE", "README.md", "CHANGELOG.md", "THIRD_PARTY_NOTICES.txt"} or len(members) != 5:
                raise ValueError("release archive contents are invalid")
            if any(not member.isfile() for member in members):
                raise ValueError("release archives must contain only regular files")
            if binary_target(tar.extractfile("wtm").read(32)) != target:
                raise ValueError("release archive contains the wrong binary architecture")
    text = "".join(f"{hashlib.sha256((output / name).read_bytes()).hexdigest()}  {name}\n" for name in sorted(expected))
    (output / "SHA256SUMS").write_text(text)
    return output / "SHA256SUMS"


def release_notes(tag, root=ROOT):
    validate_tag(tag, root)
    text = (root / "CHANGELOG.md").read_text()
    heading = re.search(r"^## (?:\[)?" + re.escape(tag[1:]) + r"(?:\])?(?: .*)?$", text, re.MULTILINE)
    if not heading:
        raise ValueError("add a versioned changelog section before tagging a release")
    body = text[heading.end():].split("\n## ", 1)[0].strip()
    if not body:
        raise ValueError("release changelog section is empty")
    return (f"# wtm {tag[1:]}\n\n{body}\n\n"
            "## Install\n\nDownload the archive for your OS and architecture, or run:\n\n"
            f"```sh\ncurl -fsSL https://raw.githubusercontent.com/tiaanduplessis/wtm/{tag}/install.sh | sh -s -- --version {tag}\n```\n\n"
            "The installer checks SHA-256 and requires Git 2.36 or newer. Rust is not required.\n"
            "Archives include the binary, project license, documentation, and dependency notices.\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    for command in ("check", "package", "checksums", "notes"):
        subparser = subparsers.add_parser(command)
        subparser.add_argument("--tag", required=True)
        if command in ("package", "checksums"):
            subparser.add_argument("--output", type=Path, default=ROOT / "dist")
        if command == "package":
            subparser.add_argument("--target", choices=TARGETS, required=True)
            subparser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "check":
            print(validate_tag(args.tag))
            release_notes(args.tag)
        elif args.command == "package":
            print(package(args.tag, args.target, args.binary, args.output))
        elif args.command == "checksums":
            print(checksums(args.tag, args.output))
        else:
            print(release_notes(args.tag))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"release: {error}\n")


if __name__ == "__main__":
    main()
