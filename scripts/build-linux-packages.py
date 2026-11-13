"""Build deterministic Linux packages from an already verified release binary."""

from __future__ import annotations

import argparse
import hashlib
import io
import lzma
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tarfile
from typing import Iterable


PACKAGE = "token-monitor-lite"
ARCHITECTURE = "amd64"
DEFAULT_EPOCH = 1_794_268_800  # 2026-11-10T00:00:00Z


def _regular_file(raw: str, description: str) -> Path:
    path = Path(raw)
    if not path.is_absolute() or ".." in path.parts or path.is_symlink() or not path.is_file():
        raise ValueError(f"{description} must be an absolute regular file")
    return path


def _new_directory(raw: str) -> Path:
    path = Path(raw)
    if not path.is_absolute() or ".." in path.parts or path.is_symlink():
        raise ValueError("output directory must be absolute and traversal-free")
    if path.exists() and any(path.iterdir()):
        raise ValueError("output directory must be new or empty")
    path.mkdir(parents=True, exist_ok=True)
    return path.resolve(strict=True)


def _entry(path: str, source: Path, mode: int) -> tuple[str, bytes, int]:
    relative = PurePosixPath(path)
    if relative.is_absolute() or ".." in relative.parts:
        raise ValueError("package path is invalid")
    return str(relative), source.read_bytes(), mode


def _tar_bytes(entries: Iterable[tuple[str, bytes, int]], epoch: int) -> bytes:
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data, mode in sorted(entries):
            info = tarfile.TarInfo(name=name)
            info.size = len(data)
            info.mode = mode
            info.mtime = epoch
            info.uid = 0
            info.gid = 0
            info.uname = "root"
            info.gname = "root"
            archive.addfile(info, io.BytesIO(data))
    return output.getvalue()


def _ar_member(name: str, data: bytes, epoch: int) -> bytes:
    encoded_name = f"{name}/".encode("ascii")
    if len(encoded_name) > 16:
        raise ValueError("ar member name is too long")
    header = b"".join(
        (
            encoded_name.ljust(16, b" "),
            str(epoch).encode("ascii").ljust(12, b" "),
            b"0".ljust(6, b" "),
            b"0".ljust(6, b" "),
            b"100644".ljust(8, b" "),
            str(len(data)).encode("ascii").ljust(10, b" "),
            b"`\n",
        )
    )
    return header + data + (b"\n" if len(data) % 2 else b"")


def _write_new(path: Path, data: bytes) -> None:
    with path.open("xb") as handle:
        handle.write(data)
        handle.flush()
        os.fsync(handle.fileno())


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def build(arguments: argparse.Namespace) -> list[Path]:
    binary = _regular_file(arguments.binary, "binary")
    desktop = _regular_file(arguments.desktop, "desktop metadata")
    metainfo = _regular_file(arguments.metainfo, "AppStream metadata")
    icon = _regular_file(arguments.icon, "application icon")
    license_file = _regular_file(arguments.license, "license")
    output = _new_directory(arguments.output)
    if arguments.epoch < 0:
        raise ValueError("source epoch must be non-negative")
    version = arguments.version
    if not version or any(character not in "0123456789abcdefghijklmnopqrstuvwxyz.+~-" for character in version):
        raise ValueError("version contains unsupported package characters")

    data_entries = [
        _entry("usr/bin/token-monitor-lite", binary, 0o755),
        _entry("usr/share/applications/token-monitor-lite.desktop", desktop, 0o644),
        _entry(
            "usr/share/icons/hicolor/scalable/apps/token-monitor-lite.svg",
            icon,
            0o644,
        ),
        _entry(
            "usr/share/metainfo/io.github.sandeep2062.TokenMonitorLite.metainfo.xml",
            metainfo,
            0o644,
        ),
        _entry("usr/share/doc/token-monitor-lite/copyright", license_file, 0o644),
    ]
    installed_size = max(1, (sum(len(data) for _, data, _ in data_entries) + 1023) // 1024)
    control = (
        f"Package: {PACKAGE}\n"
        f"Version: {version}\n"
        "Section: utils\n"
        "Priority: optional\n"
        f"Architecture: {ARCHITECTURE}\n"
        "Maintainer: Token Monitor Lite maintainers\n"
        f"Installed-Size: {installed_size}\n"
        "Depends: libc6 (>= 2.38), libdbus-1-3, libfontconfig1, libgcc-s1, "
        "libxkbcommon0, libxkbcommon-x11-0, libxcb1, libxcb-xkb1\n"
        "Description: Native token usage monitor\n"
        " Monitor supported local AI coding-session token usage with a native desktop application.\n"
    ).encode("utf-8")
    md5sums = "".join(
        f"{hashlib.md5(data, usedforsecurity=False).hexdigest()}  {name}\n"
        for name, data, _ in sorted(data_entries)
    ).encode("ascii")
    control_tar = _tar_bytes(
        [("control", control, 0o644), ("md5sums", md5sums, 0o644)],
        arguments.epoch,
    )
    data_tar = _tar_bytes(data_entries, arguments.epoch)
    xz_filters = [{"id": lzma.FILTER_LZMA2, "preset": 9 | lzma.PRESET_EXTREME}]
    deb = b"!<arch>\n" + b"".join(
        (
            _ar_member("debian-binary", b"2.0\n", arguments.epoch),
            _ar_member(
                "control.tar.xz",
                lzma.compress(control_tar, format=lzma.FORMAT_XZ, filters=xz_filters),
                arguments.epoch,
            ),
            _ar_member(
                "data.tar.xz",
                lzma.compress(data_tar, format=lzma.FORMAT_XZ, filters=xz_filters),
                arguments.epoch,
            ),
        )
    )
    deb_path = output / f"{PACKAGE}_{version}_{ARCHITECTURE}.deb"
    _write_new(deb_path, deb)

    archive_prefix = f"{PACKAGE}-{version}"
    archive_entries = [
        (f"{archive_prefix}/{name}", data, mode) for name, data, mode in data_entries
    ]
    raw_tar = output / f"{PACKAGE}-{version}-linux-x86_64.tar"
    _write_new(raw_tar, _tar_bytes(archive_entries, arguments.epoch))
    tar_zst = raw_tar.with_suffix(".tar.zst")
    result = subprocess.run(
        [
            "zstd",
            "--compress",
            "--ultra",
            "-19",
            "--threads=1",
            "--no-progress",
            "--force",
            str(raw_tar),
            "--output",
            str(tar_zst),
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    raw_tar.unlink()
    if result.returncode or not tar_zst.is_file():
        raise ValueError("zstd package compression failed")

    artifacts = [deb_path, tar_zst]
    manifest = "".join(f"{_sha256(path)}  {path.name}\n" for path in sorted(artifacts)).encode("ascii")
    manifest_path = output / "SHA256SUMS"
    _write_new(manifest_path, manifest)
    return artifacts + [manifest_path]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--desktop", required=True)
    parser.add_argument("--metainfo", required=True)
    parser.add_argument("--icon", required=True)
    parser.add_argument("--license", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--version", default="0.1.0")
    parser.add_argument("--epoch", type=int, default=DEFAULT_EPOCH)
    arguments = parser.parse_args()
    try:
        artifacts = build(arguments)
    except (OSError, ValueError) as error:
        print(f"package error: {error}", file=sys.stderr)
        return 1
    for artifact in artifacts:
        print(f"{_sha256(artifact)}  {artifact.name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
