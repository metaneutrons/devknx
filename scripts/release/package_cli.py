#!/usr/bin/env python3
"""Build one deterministic, self-contained devknx CLI archive.

The release workflow supplies the already-built native executable. This script
never compiles, signs, uploads, or changes a GitHub release.
"""

from __future__ import annotations

import argparse
import gzip
import io
import pathlib
import re
import sys
import tarfile
import tomllib
import zipfile

TARGETS = {
    "aarch64-apple-darwin": "tar.gz",
    "x86_64-unknown-linux-gnu": "tar.gz",
    "aarch64-unknown-linux-gnu": "tar.gz",
    "x86_64-unknown-linux-musl": "tar.gz",
    "aarch64-unknown-linux-musl": "tar.gz",
    "x86_64-pc-windows-msvc": "zip",
    "aarch64-pc-windows-msvc": "zip",
}
VERSION = re.compile(r"^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?$")


def files_for(root: pathlib.Path, binary: pathlib.Path, target: str) -> list[tuple[str, bytes, int]]:
    if not binary.is_file():
        raise ValueError(f"missing executable: {binary}")
    content = binary.read_bytes()
    if not valid_executable(content, target):
        raise ValueError(f"executable format does not match {target}")
    if not content:
        raise ValueError("empty executable")
    name = "devknx.exe" if "windows" in target else "devknx"
    result = [(name, content, 0o755)]
    for relative in ("LICENSE", "THIRD-PARTY-NOTICES.md"):
        source = root / relative
        if not source.is_file() or not source.stat().st_size:
            raise ValueError(f"missing licence notice: {source}")
        result.append((relative, source.read_bytes(), 0o644))
    licences = sorted((root / "licenses").glob("*"))
    if not licences:
        raise ValueError("missing embedded-font licences")
    for source in licences:
        if not source.is_file() or not source.stat().st_size:
            raise ValueError(f"invalid embedded-font licence: {source}")
        result.append((f"licenses/{source.name}", source.read_bytes(), 0o644))
    return result


def valid_executable(content: bytes, target: str) -> bool:
    if target.endswith("apple-darwin"):
        return (
            content.startswith(b"\xcf\xfa\xed\xfe")
            and len(content) >= 8
            and int.from_bytes(content[4:8], "little") == 0x0100000C
        )
    if "linux" in target:
        machine = 62 if target.startswith("x86_64") else 183
        return (
            content.startswith(b"\x7fELF\x02\x01")
            and len(content) >= 20
            and int.from_bytes(content[18:20], "little") == machine
        )
    if "windows" in target:
        if not content.startswith(b"MZ") or len(content) < 64:
            return False
        offset = int.from_bytes(content[60:64], "little")
        machine = 0x8664 if target.startswith("x86_64") else 0xAA64
        return (
            offset + 6 <= len(content)
            and content[offset : offset + 4] == b"PE\0\0"
            and int.from_bytes(content[offset + 4 : offset + 6], "little") == machine
        )
    return False


def write_tar(path: pathlib.Path, prefix: str, files: list[tuple[str, bytes, int]], epoch: int) -> None:
    with path.open("wb") as output:
        with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=epoch) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for name, content, mode in sorted(files):
                    info = tarfile.TarInfo(f"{prefix}/{name}")
                    info.size = len(content)
                    info.mode = mode
                    info.mtime = epoch
                    info.uid = 0
                    info.gid = 0
                    info.uname = ""
                    info.gname = ""
                    archive.addfile(info, io.BytesIO(content))


def write_zip(path: pathlib.Path, prefix: str, files: list[tuple[str, bytes, int]], epoch: int) -> None:
    import datetime

    date = datetime.datetime.fromtimestamp(max(epoch, 315532800), tz=datetime.timezone.utc)
    stamp = (date.year, date.month, date.day, date.hour, date.minute, date.second)
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for name, content, mode in sorted(files):
            info = zipfile.ZipInfo(f"{prefix}/{name}", date_time=stamp)
            info.create_system = 3
            info.external_attr = (0o100000 | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, content, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)


def package(root: pathlib.Path, tag: str, target: str, binary: pathlib.Path, output: pathlib.Path, epoch: int) -> pathlib.Path:
    if target not in TARGETS:
        raise ValueError(f"unsupported target: {target}")
    match = VERSION.fullmatch(tag)
    if not match:
        raise ValueError(f"invalid release tag: {tag}")
    version = ".".join(match.groups()[:3])
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    if manifest["package"]["version"] != version:
        raise ValueError(f"tag {tag} does not match Cargo.toml version {manifest['package']['version']}")
    if epoch < 315532800:
        raise ValueError("SOURCE_DATE_EPOCH must be no earlier than 1980")
    entries = files_for(root, binary, target)
    extension = TARGETS[target]
    stem = f"devknx-{tag}-{target}"
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"{stem}.{extension}"
    if destination.exists():
        raise ValueError(f"refusing to overwrite existing archive: {destination}")
    if extension == "zip":
        write_zip(destination, stem, entries, epoch)
    else:
        write_tar(destination, stem, entries, epoch)
    return destination


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--output-dir", type=pathlib.Path, required=True)
    parser.add_argument("--source-date-epoch", type=int, required=True)
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parents[2]
    try:
        archive = package(root, args.tag, args.target, args.binary, args.output_dir, args.source_date_epoch)
    except (OSError, KeyError, ValueError, tomllib.TOMLDecodeError) as error:
        print(f"package_cli: {error}", file=sys.stderr)
        return 1
    print(archive)
    return 0


if __name__ == "__main__":
    sys.exit(main())
