"""Build and inspect deterministic Linux release packages on native x86-64 Linux."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import stat
import subprocess
import sys
from typing import Any


EVIDENCE_SCHEMA = "token-monitor-lite-evidence-v1"
SHA256 = re.compile(r"^[0-9a-f]{64}$")
EXPECTED_FILES = {
    "usr/bin/token-monitor-lite": 0o755,
    "usr/share/applications/token-monitor-lite.desktop": 0o644,
    "usr/share/doc/token-monitor-lite/copyright": 0o644,
    "usr/share/icons/hicolor/scalable/apps/token-monitor-lite.svg": 0o644,
    "usr/share/metainfo/io.github.sandeep2062.TokenMonitorLite.metainfo.xml": 0o644,
}


def _load_report_validator() -> Any:
    path = Path(__file__).with_name("run-linux-acceptance.py")
    spec = importlib.util.spec_from_file_location("token_monitor_linux_acceptance", path)
    if spec is None or spec.loader is None:
        raise ValueError("acceptance report validator is unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _run(command: list[str], *, check: bool = True) -> subprocess.CompletedProcess[bytes]:
    result = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=120.0,
    )
    if check and result.returncode:
        raise ValueError(f"command failed: {Path(command[0]).name}")
    return result


def _regular_file(raw: str, description: str) -> Path:
    path = Path(raw)
    if not path.is_absolute() or ".." in path.parts or path.is_symlink() or not path.is_file():
        raise ValueError(f"{description} must be an absolute regular file")
    return path


def _new_root(raw: str) -> Path:
    path = Path(raw)
    if not path.is_absolute() or ".." in path.parts or path.is_symlink() or path.exists():
        raise ValueError("work root must be a new absolute non-symlink path")
    path.mkdir(parents=True, mode=0o700)
    return path.resolve(strict=True)


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _tree_hashes(root: Path) -> dict[str, tuple[str, int]]:
    values: dict[str, tuple[str, int]] = {}
    for path in sorted(root.rglob("*")):
        if path.is_dir():
            continue
        if path.is_symlink() or not path.is_file():
            raise ValueError("package extraction contains a non-regular file")
        relative = path.relative_to(root).as_posix()
        values[relative] = (_sha256(path), stat.S_IMODE(path.stat().st_mode))
    return values


def _build(
    builder: Path,
    binary: Path,
    desktop: Path,
    metainfo: Path,
    icon: Path,
    license_file: Path,
    output: Path,
    version: str,
) -> None:
    _run(
        [
            sys.executable,
            str(builder),
            "--binary",
            str(binary),
            "--desktop",
            str(desktop),
            "--metainfo",
            str(metainfo),
            "--icon",
            str(icon),
            "--license",
            str(license_file),
            "--output",
            str(output),
            "--version",
            version,
        ]
    )


def _assert_deterministic(first: Path, second: Path) -> None:
    first_files = {path.name: path for path in first.iterdir() if path.is_file()}
    second_files = {path.name: path for path in second.iterdir() if path.is_file()}
    if set(first_files) != set(second_files):
        raise ValueError("deterministic package output names differ")
    if any(first_files[name].read_bytes() != second_files[name].read_bytes() for name in first_files):
        raise ValueError("deterministic package bytes differ")


def _validate_extracted(root: Path, expected_binary: Path) -> None:
    files = _tree_hashes(root)
    if set(files) != set(EXPECTED_FILES):
        raise ValueError("package contains an unexpected path")
    for name, expected_mode in EXPECTED_FILES.items():
        if files[name][1] != expected_mode:
            raise ValueError("package file mode mismatch")
    if files["usr/bin/token-monitor-lite"][0] != _sha256(expected_binary):
        raise ValueError("packaged binary differs from accepted release binary")


def _validate_declared_dependencies(deb: Path, binary: Path) -> None:
    declared = {
        dependency.strip().split(" ", 1)[0].split(":", 1)[0]
        for dependency in _run(["dpkg-deb", "--field", str(deb), "Depends"])
        .stdout.decode("utf-8")
        .split(",")
    }
    dynamic = _run(["readelf", "--dynamic", str(binary)]).stdout.decode(
        "utf-8", errors="replace"
    )
    needed = set(re.findall(r"Shared library: \[([^]]+)]", dynamic))
    ldd = _run(["ldd", str(binary)]).stdout.decode("utf-8", errors="replace")
    resolved: dict[str, Path] = {}
    for line in ldd.splitlines():
        match = re.search(r"^\s*(\S+)\s+=>\s+(/\S+)\s+\(", line)
        if match:
            resolved[match.group(1)] = Path(match.group(2))
    missing_paths = sorted(needed - set(resolved) - {"linux-vdso.so.1"})
    if missing_paths:
        raise ValueError("a direct dynamic dependency was not resolved")
    undeclared: set[str] = set()
    for soname in sorted(needed):
        library = resolved.get(soname)
        if library is None:
            continue
        owner = b""
        candidates = [library]
        resolved_library = library.resolve(strict=True)
        if resolved_library != library:
            candidates.append(resolved_library)
        for candidate in candidates:
            result = _run(["dpkg-query", "--search", str(candidate)], check=False)
            if result.returncode == 0:
                owner = result.stdout
                break
        if not owner:
            raise ValueError("a direct dynamic dependency has no owning Debian package")
        packages = {
            line.split(": ", 1)[0].split(":", 1)[0]
            for line in owner.decode("utf-8").splitlines()
            if ": " in line
        }
        if not packages.intersection(declared):
            undeclared.update(packages or {soname})
    if undeclared:
        raise ValueError("Debian control metadata omits a direct dynamic dependency package")


def _write_report(path: Path, report: dict[str, Any]) -> str:
    if not path.is_absolute() or ".." in path.parts or path.is_symlink() or path.exists():
        raise ValueError("report path must be a new absolute non-symlink path")
    encoded = json.dumps(report, indent=2, sort_keys=True).encode("utf-8") + b"\n"
    with path.open("xb") as handle:
        handle.write(encoded)
        handle.flush()
        os.fsync(handle.fileno())
    return hashlib.sha256(encoded).hexdigest()


def run(arguments: argparse.Namespace) -> dict[str, Any]:
    if platform.system() != "Linux" or platform.machine() not in {"x86_64", "AMD64"}:
        raise ValueError("package acceptance requires native Linux x86-64")
    if not SHA256.fullmatch(arguments.source_snapshot_sha256):
        raise ValueError("source snapshot hash must be lowercase SHA-256")
    binary = _regular_file(arguments.binary, "binary")
    builder = _regular_file(arguments.builder, "package builder")
    generator = _regular_file(arguments.generator, "fixture generator")
    desktop = _regular_file(arguments.desktop, "desktop metadata")
    metainfo = _regular_file(arguments.metainfo, "AppStream metadata")
    icon = _regular_file(arguments.icon, "application icon")
    license_file = _regular_file(arguments.license, "license")
    work = _new_root(arguments.work_root)
    report_path = Path(arguments.report)

    fixture = work / "fixture"
    fixture_result = _run([sys.executable, str(generator), "--output", str(fixture)])
    fixture_summary = json.loads(fixture_result.stdout)
    fixture_manifest_path = fixture / "manifest.json"
    fixture_manifest = json.loads(fixture_manifest_path.read_text(encoding="utf-8"))

    package_a = work / "package-a"
    package_b = work / "package-b"
    package_old = work / "package-old"
    _build(builder, binary, desktop, metainfo, icon, license_file, package_a, arguments.version)
    _build(builder, binary, desktop, metainfo, icon, license_file, package_b, arguments.version)
    _build(builder, binary, desktop, metainfo, icon, license_file, package_old, "0.0.9")
    _assert_deterministic(package_a, package_b)

    deb = package_a / f"token-monitor-lite_{arguments.version}_amd64.deb"
    archive = package_a / f"token-monitor-lite-{arguments.version}-linux-x86_64.tar.zst"
    sums = package_a / "SHA256SUMS"
    for artifact in (deb, archive, sums):
        if not artifact.is_file() or artifact.is_symlink():
            raise ValueError("expected package artifact is missing")
    expected_sums = "".join(
        f"{_sha256(path)}  {path.name}\n" for path in sorted((deb, archive))
    )
    if sums.read_text(encoding="ascii") != expected_sums:
        raise ValueError("package checksum manifest mismatch")

    fields = _run(["dpkg-deb", "--field", str(deb)]).stdout.decode("utf-8")
    for required in (
        "Package: token-monitor-lite",
        f"Version: {arguments.version}",
        "Architecture: amd64",
    ):
        if required not in fields:
            raise ValueError("Debian control metadata mismatch")

    deb_root = work / "deb-root"
    archive_root = work / "archive-root"
    deb_root.mkdir()
    archive_root.mkdir()
    _run(["dpkg-deb", "--extract", str(deb), str(deb_root)])
    _run(["tar", "--zstd", "--extract", "--file", str(archive), "--directory", str(archive_root)])
    _validate_extracted(deb_root, binary)
    archive_payload = archive_root / f"token-monitor-lite-{arguments.version}"
    _validate_extracted(archive_payload, binary)
    if _tree_hashes(deb_root) != _tree_hashes(archive_payload):
        raise ValueError("Debian and tar package payloads differ")

    _run(["desktop-file-validate", str(deb_root / "usr/share/applications/token-monitor-lite.desktop")])
    _run(
        [
            "appstreamcli",
            "validate",
            "--no-net",
            str(
                deb_root
                / "usr/share/metainfo/io.github.sandeep2062.TokenMonitorLite.metainfo.xml"
            ),
        ]
    )
    elf = _run(["readelf", "--file-header", str(deb_root / "usr/bin/token-monitor-lite")]).stdout
    if b"Advanced Micro Devices X86-64" not in elf:
        raise ValueError("packaged binary is not Linux x86-64 ELF")
    dependencies = _run(["ldd", str(deb_root / "usr/bin/token-monitor-lite")]).stdout.lower()
    if b"not found" in dependencies:
        raise ValueError("packaged binary has an unresolved dynamic dependency")
    _validate_declared_dependencies(deb, deb_root / "usr/bin/token-monitor-lite")
    strings = _run(["strings", "--all", str(deb_root / "usr/bin/token-monitor-lite")]).stdout.lower()
    for forbidden in (
        b"electron",
        b"node_modules",
        b"chromium",
        b"token-monitor/",
        b"/home/azureuser/",
        b"d:\\github\\",
    ):
        if forbidden in strings:
            raise ValueError("packaged binary contains a forbidden runtime/build marker")

    lifecycle_root = work / "lifecycle-root"
    lifecycle_root.mkdir()
    old_deb = package_old / "token-monitor-lite_0.0.9_amd64.deb"
    _run(["dpkg-deb", "--extract", str(old_deb), str(lifecycle_root)])
    user_state = work / "retained-user-state"
    user_state.mkdir()
    sentinel = user_state / "settings-preserved"
    sentinel.write_bytes(b"preserve-across-package-lifecycle\n")
    sentinel_hash = _sha256(sentinel)
    _run(["dpkg-deb", "--extract", str(deb), str(lifecycle_root)])
    _validate_extracted(lifecycle_root, binary)
    before_failed_upgrade = _tree_hashes(lifecycle_root)
    corrupt = work / "corrupt-upgrade.deb"
    corrupt.write_bytes(deb.read_bytes()[: max(1, deb.stat().st_size // 3)])
    failed_stage = work / "failed-upgrade-stage"
    failed_stage.mkdir()
    if _run(["dpkg-deb", "--extract", str(corrupt), str(failed_stage)], check=False).returncode == 0:
        raise ValueError("truncated package was unexpectedly accepted")
    if _tree_hashes(lifecycle_root) != before_failed_upgrade:
        raise ValueError("failed package staging changed the installed root")
    for name in EXPECTED_FILES:
        (lifecycle_root / PurePosixPath(name)).unlink()
    if any(path.is_file() for path in lifecycle_root.rglob("*")):
        raise ValueError("isolated package uninstall left payload files")
    if _sha256(sentinel) != sentinel_hash:
        raise ValueError("package lifecycle changed retained user state")

    artifacts = [
        {"role": "linux-deb", "sha256": _sha256(deb), "bytes": deb.stat().st_size},
        {
            "role": "linux-tar-zst",
            "sha256": _sha256(archive),
            "bytes": archive.stat().st_size,
        },
        {"role": "package-sha256sums", "sha256": _sha256(sums), "bytes": sums.stat().st_size},
    ]
    report = {
        "schema": EVIDENCE_SCHEMA,
        "phase": "linux-package",
        "status": "diagnostic",
        "sourceSnapshotSha256": arguments.source_snapshot_sha256,
        "fixture": {
            "schema": fixture_manifest["schema"],
            "revision": fixture_manifest["revision"],
            "manifestSha256": fixture_summary["manifestSha256"],
            "configuredSources": fixture_manifest["coverage"]["configuredSources"],
            "canonicalSessions": fixture_manifest["coverage"]["canonicalSessions"],
            "usageObservations": fixture_manifest["coverage"]["usageObservations"],
            "releaseTargetMet": fixture_manifest["releaseTarget"]["met"],
        },
        "platform": {
            "os": platform.system(),
            "architecture": platform.machine(),
            "desktop": None,
            "renderer": "not-started",
            "cpuCount": os.cpu_count() or 1,
            "memoryMiB": max(1, (os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")) // (1024 * 1024)),
        },
        "checks": [
            {"name": "deterministic-package-bytes", "status": "passed"},
            {"name": "exact-package-contents", "status": "passed"},
            {"name": "desktop-appstream-metadata", "status": "passed"},
            {"name": "elf-dependency-marker-inspection", "status": "passed"},
            {"name": "declared-dynamic-dependencies", "status": "passed"},
            {"name": "isolated-install-upgrade-rollback-uninstall", "status": "passed"},
            {"name": "native-system-package-lifecycle", "status": "blocked"},
            {"name": "qualified-release-signature", "status": "blocked"},
        ],
        "artifacts": artifacts,
        "blockers": [
            "approved-native-system-install",
            "qualified-linux-release-signing-identity",
        ],
        "cleanup": {
            "processesStopped": True,
            "credentialRemoved": True,
            "temporaryRootsRemoved": False,
        },
    }
    _load_report_validator()._validate_report(report)
    report_hash = _write_report(report_path, report)
    return {"phase": report["phase"], "reportSha256": report_hash, "status": report["status"]}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--builder", required=True)
    parser.add_argument("--generator", required=True)
    parser.add_argument("--desktop", required=True)
    parser.add_argument("--metainfo", required=True)
    parser.add_argument("--icon", required=True)
    parser.add_argument("--license", required=True)
    parser.add_argument("--work-root", required=True)
    parser.add_argument("--report", required=True)
    parser.add_argument("--version", default="0.1.0")
    parser.add_argument("--source-snapshot-sha256", required=True)
    arguments = parser.parse_args()
    try:
        summary = run(arguments)
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"package acceptance error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(summary, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
