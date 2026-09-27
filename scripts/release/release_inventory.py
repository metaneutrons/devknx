#!/usr/bin/env python3
"""Enforce the exact signed release-asset set and write/check SHA256SUMS."""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import re
import sys

from package_cli import TARGETS

TAG = re.compile(r"^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?$")
DEFINITIONS = (
    "devknx.rb",
    "devknx-app.rb",
    "devknx.PKGBUILD",
    "devknx-bin.PKGBUILD",
)


def payloads(tag: str) -> set[str]:
    match = TAG.fullmatch(tag)
    if not match:
        raise ValueError(f"invalid release tag: {tag}")
    core = ".".join(match.groups())
    names = {
        f"devknx-{tag}-{target}.{extension}"
        for target, extension in TARGETS.items()
    }
    names.add(f"devknx-{tag}-aarch64-apple-darwin.app.zip")
    names.add(f"devknx-{tag}-source.tar.gz")
    names.update(f"devknx_{core}-1_{arch}.deb" for arch in ("amd64", "arm64"))
    return names


def expected(tag: str, *, complete: bool) -> set[str]:
    names = payloads(tag)
    result = set(names)
    for name in names:
        result.update((f"{name}.spdx.json", f"{name}.sigstore.json"))
    if complete:
        for name in DEFINITIONS:
            result.update((name, f"{name}.sigstore.json"))
        result.add("SHA256SUMS")
    return result


def file_names(directory: pathlib.Path) -> set[str]:
    if not directory.is_dir():
        raise ValueError(f"not a directory: {directory}")
    paths = tuple(directory.iterdir())
    for path in paths:
        if path.is_symlink() or not path.is_file() or not path.stat().st_size:
            raise ValueError(f"unsafe or empty release asset: {path}")
    return {path.name for path in paths}


def check_set(directory: pathlib.Path, names: set[str]) -> None:
    present = file_names(directory)
    if present != names:
        raise ValueError(
            f"release asset mismatch; missing={sorted(names - present)}, "
            f"unexpected={sorted(present - names)}"
        )


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def write_sums(directory: pathlib.Path, tag: str) -> None:
    wanted = expected(tag, complete=True)
    wanted.remove("SHA256SUMS")
    check_set(directory, wanted)
    sums = directory / "SHA256SUMS"
    with sums.open("x", encoding="ascii") as output:
        for name in sorted(wanted):
            output.write(f"{sha256(directory / name)}  {name}\n")


def verify(directory: pathlib.Path, tag: str) -> None:
    wanted = expected(tag, complete=True)
    check_set(directory, wanted)
    raw = (directory / "SHA256SUMS").read_text(encoding="ascii")
    lines = raw.splitlines()
    names: set[str] = set()
    ordered_names: list[str] = []
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.-]+)", line)
        if not match:
            raise ValueError("malformed SHA256SUMS")
        digest, name = match.groups()
        if name in names or name == "SHA256SUMS" or sha256(directory / name) != digest:
            raise ValueError(f"checksum mismatch or duplicate: {name}")
        names.add(name)
        ordered_names.append(name)
    if names != wanted - {"SHA256SUMS"} or ordered_names != sorted(ordered_names):
        raise ValueError("SHA256SUMS does not match the candidate inventory")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("raw", "payloads", "write", "verify"))
    parser.add_argument("--tag", required=True)
    parser.add_argument("--directory", type=pathlib.Path, required=True)
    args = parser.parse_args()
    try:
        if args.operation == "raw":
            check_set(args.directory, payloads(args.tag))
        elif args.operation == "payloads":
            check_set(args.directory, expected(args.tag, complete=False))
        elif args.operation == "write":
            write_sums(args.directory, args.tag)
        else:
            verify(args.directory, args.tag)
    except (OSError, ValueError) as error:
        print(f"release_inventory: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
