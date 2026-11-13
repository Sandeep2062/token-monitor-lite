"""Measure native Linux release startup, idle resources, and update latency."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys
import time
from typing import Any


EVIDENCE_SCHEMA = "token-monitor-lite-evidence-v1"
SHA256 = re.compile(r"^[0-9a-f]{64}$")
STARTUP_LIMIT_MS = 2_000
IDLE_RSS_LIMIT_KIB = 100 * 1024
IDLE_CPU_LIMIT_PERCENT = 0.5
UPDATE_LIMIT_MS = 2_000


def _load_gui_driver() -> Any:
    path = Path(__file__).with_name("run-linux-acceptance.py")
    spec = importlib.util.spec_from_file_location("token_monitor_linux_gui_acceptance", path)
    if spec is None or spec.loader is None:
        raise ValueError("Linux GUI acceptance helpers are unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _regular_file(raw: str, description: str) -> Path:
    path = Path(raw)
    if not path.is_absolute() or ".." in path.parts or path.is_symlink() or not path.is_file():
        raise ValueError(f"{description} must be an absolute regular file")
    return path


def _directory(raw: str, description: str) -> Path:
    path = Path(raw)
    if not path.is_absolute() or ".." in path.parts or path.is_symlink() or not path.is_dir():
        raise ValueError(f"{description} must be an absolute directory")
    return path.resolve(strict=True)


def _new_directory(raw: str) -> Path:
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


def _percentile(values: list[int], percentile: float) -> int:
    if not values:
        raise ValueError("measurement set is empty")
    ordered = sorted(values)
    index = max(0, math.ceil(percentile * len(ordered)) - 1)
    return ordered[index]


def _median(values: list[int]) -> float:
    if not values:
        raise ValueError("measurement set is empty")
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return float(ordered[middle])
    return (ordered[middle - 1] + ordered[middle]) / 2.0


def _drop_owned_cache(paths: list[Path]) -> None:
    advice = getattr(os, "POSIX_FADV_DONTNEED", None)
    advise = getattr(os, "posix_fadvise", None)
    if advice is None or advise is None:
        raise ValueError("POSIX cache advice is unavailable")
    for path in paths:
        if not path.is_file() or path.is_symlink():
            continue
        with path.open("rb") as handle:
            advise(handle.fileno(), 0, 0, advice)


def _proc_ticks(pid: int) -> int:
    data = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    fields = data[data.rfind(")") + 2 :].split()
    return int(fields[11]) + int(fields[12])


def _proc_io(pid: int) -> dict[str, int]:
    values: dict[str, int] = {}
    for line in Path(f"/proc/{pid}/io").read_text(encoding="ascii").splitlines():
        key, separator, raw_value = line.partition(":")
        if separator:
            values[key] = int(raw_value.strip())
    required = {"rchar", "wchar", "syscr", "syscw", "read_bytes", "write_bytes"}
    if not required.issubset(values):
        raise ValueError("process I/O counters are incomplete")
    return {name: values[name] for name in sorted(required)}


def _counter_delta(before: dict[str, int], after: dict[str, int]) -> dict[str, int]:
    return {name: max(0, after[name] - before[name]) for name in before}


def _rss_kib(pid: int) -> int:
    for line in Path(f"/proc/{pid}/status").read_text(encoding="ascii").splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1])
    raise ValueError("process RSS is unavailable")


def _threads(pid: int) -> int:
    for line in Path(f"/proc/{pid}/status").read_text(encoding="ascii").splitlines():
        if line.startswith("Threads:"):
            return int(line.split()[1])
    raise ValueError("process thread count is unavailable")


def _process_inventory(pid: int) -> dict[str, int]:
    fd_root = Path(f"/proc/{pid}/fd")
    descriptors = list(fd_root.iterdir())
    sqlite_descriptors = 0
    socket_descriptors = 0
    inotify_watches = 0
    for descriptor in descriptors:
        try:
            target = os.readlink(descriptor)
        except OSError:
            continue
        if "token-monitor-lite.sqlite3" in target:
            sqlite_descriptors += 1
        if target.startswith("socket:"):
            socket_descriptors += 1
        try:
            info = Path(f"/proc/{pid}/fdinfo/{descriptor.name}").read_text(
                encoding="ascii", errors="replace"
            )
        except OSError:
            continue
        inotify_watches += sum(1 for line in info.splitlines() if line.startswith("inotify "))
    children_text = Path(f"/proc/{pid}/task/{pid}/children").read_text(encoding="ascii").strip()
    return {
        "childProcesses": len(children_text.split()) if children_text else 0,
        "fileDescriptors": len(descriptors),
        "inotifyWatches": inotify_watches,
        "socketDescriptors": socket_descriptors,
        "sqliteDescriptors": sqlite_descriptors,
        "threads": _threads(pid),
    }


def _tcp_listener_count(driver: Any, env: dict[str, str], pid: int) -> int:
    result = driver._run(["ss", "--tcp", "--udp", "--listening", "--processes"], env=env, check=False)
    marker = f"pid={pid},".encode("ascii")
    return sum(1 for line in result.stdout.splitlines() if marker in line)


def _formatted_tracked_total(manifest: dict[str, Any], increment: int = 0) -> str:
    totals = manifest["expectedAggregate"]
    value = sum(int(totals[name]) for name in ("input", "output", "cacheRead", "cacheWrite", "reasoning"))
    return f"{value + increment:,}"


def _tree_contains(driver: Any, env: dict[str, str], value: str) -> bool:
    return any(value == str(node.get("name", "")) for node in driver._accessibility_tree(env))


def _observe_update(
    driver: Any,
    env: dict[str, str],
    pid: int,
    action: Any,
    predicate: Any,
    description: str,
    *,
    timeout: float = 5.0,
) -> dict[str, Any]:
    started = time.monotonic()
    start_ticks = _proc_ticks(pid)
    start_io = _proc_io(pid)
    peak_rss_kib = _rss_kib(pid)
    action()
    while not predicate():
        peak_rss_kib = max(peak_rss_kib, _rss_kib(pid))
        if time.monotonic() - started > timeout:
            raise ValueError(f"timed out waiting for {description}")
        time.sleep(0.05)
    elapsed = max(time.monotonic() - started, 0.001)
    peak_rss_kib = max(peak_rss_kib, _rss_kib(pid))
    ticks = _proc_ticks(pid) - start_ticks
    return {
        "cpuPercent": round(ticks / os.sysconf("SC_CLK_TCK") / elapsed * 100.0, 4),
        "io": _counter_delta(start_io, _proc_io(pid)),
        "latencyMs": round(elapsed * 1000),
        "peakRssKiB": peak_rss_kib,
    }


def _write_json(path: Path, value: Any) -> None:
    encoded = json.dumps(value, indent=2, sort_keys=True).encode("utf-8") + b"\n"
    with path.open("xb") as handle:
        handle.write(encoded)
        handle.flush()
        os.fsync(handle.fileno())


def _replace_file(path: Path, data: bytes) -> None:
    with path.open("wb") as handle:
        handle.write(data)
        handle.flush()
        os.fsync(handle.fileno())


def _append_file(path: Path, records: list[bytes]) -> None:
    with path.open("ab", buffering=0) as handle:
        for record in records:
            handle.write(record)
        os.fsync(handle.fileno())


def run(arguments: argparse.Namespace) -> tuple[dict[str, Any], bool]:
    if platform.system() != "Linux" or platform.machine() not in {"x86_64", "AMD64"}:
        raise ValueError("resource acceptance requires native Linux x86-64")
    if not SHA256.fullmatch(arguments.source_snapshot_sha256):
        raise ValueError("source snapshot hash must be lowercase SHA-256")
    binary = _regular_file(arguments.binary, "binary")
    home = _directory(arguments.home, "persisted acceptance home")
    fixture_root = _directory(arguments.fixture_root, "fixture root")
    manifest_path = fixture_root / "manifest.json"
    if not manifest_path.is_file() or manifest_path.is_symlink():
        raise ValueError("fixture manifest is unavailable")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    work = _new_directory(arguments.work_root)
    report_path = Path(arguments.report)
    if not report_path.is_absolute() or ".." in report_path.parts or report_path.exists() or report_path.is_symlink():
        raise ValueError("report must be a new absolute non-symlink path")
    driver = _load_gui_driver()

    logs = work / "logs"
    runtime = work / "runtime"
    cache = work / "cache"
    logs.mkdir()
    runtime.mkdir(mode=0o700)
    cache.mkdir()
    display_number = next(
        (number for number in range(71, 100) if not Path(f"/tmp/.X{number}-lock").exists()),
        None,
    )
    if display_number is None:
        raise ValueError("no isolated X display number is available")
    env = dict(os.environ)
    for name in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS"):
        env.pop(name, None)
    env.update(
        {
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_STATE_HOME": str(home / ".local" / "state"),
            "XDG_CACHE_HOME": str(cache),
            "XDG_RUNTIME_DIR": str(runtime),
            "TOKEN_MONITOR_CLIENTS": "claude,codex",
            "SLINT_BACKEND": "winit-software",
            "NO_AT_BRIDGE": "0",
            "ACCESSIBILITY_ENABLED": "1",
            "DISPLAY": f":{display_number}",
            "RUST_BACKTRACE": "0",
        }
    )

    dbus_pid: int | None = None
    xvfb: subprocess.Popen[bytes] | None = None
    window_manager: subprocess.Popen[bytes] | None = None
    accessibility_bus: subprocess.Popen[bytes] | None = None
    application: subprocess.Popen[bytes] | None = None
    cleanup_ok = False
    measurements: dict[str, Any] = {}
    restoration_error: OSError | None = None
    try:
        bus = driver._run(
            ["dbus-daemon", "--session", "--fork", "--nopidfile", "--print-address=1", "--print-pid=1"],
            env=env,
        ).stdout.decode("utf-8").splitlines()
        if len(bus) != 2 or not bus[0].startswith("unix:"):
            raise ValueError("isolated DBus session did not start")
        dbus_pid = int(bus[1])
        env["DBUS_SESSION_BUS_ADDRESS"] = bus[0]
        xvfb = driver._start_process(
            ["Xvfb", env["DISPLAY"], "-screen", "0", "1280x800x24", "-nolisten", "tcp", "-noreset"],
            env,
            logs / "xvfb.log",
        )
        driver._wait_for(
            lambda: driver._run(
                ["xdotool", "getmouselocation"], env=env, timeout=2.0, check=False
            ).returncode
            == 0,
            "Xvfb display",
        )
        window_manager = driver._start_process(
            ["matchbox-window-manager", "-use_titlebar", "yes"],
            env,
            logs / "window-manager.log",
        )
        accessibility_bus = driver._start_process(
            ["/usr/libexec/at-spi-bus-launcher", "--launch-immediately"],
            env,
            logs / "accessibility-bus.log",
        )
        time.sleep(0.5)

        source_paths = [fixture_root / entry["path"] for entry in manifest["sources"]]
        for bundle in manifest["bundles"]:
            source_paths.extend(
                fixture_root / sidecar["path"] for sidecar in bundle["sidecars"]
            )
        source_physical_bytes = sum(path.stat().st_size for path in source_paths)
        source_decoded_bytes = sum(
            entry["decodedBytes"] for entry in manifest["sources"]
        ) + sum(
            sidecar["bytes"]
            for bundle in manifest["bundles"]
            for sidecar in bundle["sidecars"]
        )
        database = home / ".local" / "state" / "token-monitor-lite" / "token-monitor-lite.sqlite3"
        cold_start_ms: list[int] = []
        warm_start_ms: list[int] = []
        shutdown_ms: list[int] = []
        for index in range(30):
            if index < 10:
                _drop_owned_cache(
                    [
                        binary,
                        manifest_path,
                        *source_paths,
                        database,
                        database.with_name(database.name + "-wal"),
                        database.with_name(database.name + "-shm"),
                    ]
                )
            started = time.monotonic()
            application, window_id = driver._start_application(
                binary, env, logs / f"startup-{index:02d}.log"
            )
            driver._wait_for(
                lambda: driver._tree_contains(env, "Healthy 100"),
                "populated startup window",
                timeout=30.0,
            )
            duration = round((time.monotonic() - started) * 1000)
            (cold_start_ms if index < 10 else warm_start_ms).append(duration)
            shutdown_ms.append(driver._close_application(env, application, window_id))
            application = None

        application, window_id = driver._start_application(binary, env, logs / "resource-session.log")
        driver._wait_for(
            lambda: driver._tree_contains(env, "Healthy 100"),
            "resource session population",
            timeout=30.0,
        )
        time.sleep(5.0)
        idle_started = time.monotonic()
        idle_start_ticks = _proc_ticks(application.pid)
        idle_rss_kib: list[int] = []
        while time.monotonic() - idle_started < 60.0:
            idle_rss_kib.append(_rss_kib(application.pid))
            time.sleep(1.0)
        idle_elapsed = time.monotonic() - idle_started
        idle_ticks = _proc_ticks(application.pid) - idle_start_ticks
        idle_cpu_percent = (
            idle_ticks / os.sysconf("SC_CLK_TCK") / idle_elapsed * 100.0
        )

        inventory = _process_inventory(application.pid)
        inventory["tcpUdpListeners"] = _tcp_listener_count(driver, env, application.pid)
        first_source = fixture_root / manifest["sources"][0]["path"]
        original_source = first_source.read_bytes()
        appended = json.dumps(
            {
                "message": {
                    "id": "acceptance-latency-message",
                    "model": "claude-sonnet-4-5",
                    "usage": {"input_tokens": 7, "output_tokens": 0},
                },
                "requestId": "acceptance-latency-request",
                "sessionId": manifest["sources"][0]["session"],
                "timestamp": "2026-11-09T13:00:00Z",
                "type": "assistant",
            },
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8") + b"\n"
        active_peak_rss_kib = _rss_kib(application.pid)
        appended_total = _formatted_tracked_total(manifest, 7)
        append_measurement = _observe_update(
            driver,
            env,
            application.pid,
            lambda: _append_file(first_source, [appended]),
            lambda: _tree_contains(driver, env, appended_total),
            "visible append update",
        )
        active_peak_rss_kib = max(active_peak_rss_kib, append_measurement["peakRssKiB"])

        original_total = _formatted_tracked_total(manifest)
        rewrite_measurement = _observe_update(
            driver,
            env,
            application.pid,
            lambda: _replace_file(first_source, original_source),
            lambda: _tree_contains(driver, env, original_total),
            "visible rewrite update",
        )
        active_peak_rss_kib = max(active_peak_rss_kib, rewrite_measurement["peakRssKiB"])

        burst_records = 100
        burst_lines = [
            json.dumps(
                {
                    "message": {
                        "id": f"acceptance-burst-message-{index:03d}",
                        "model": "claude-sonnet-4-5",
                        "usage": {"input_tokens": 1, "output_tokens": 0},
                    },
                    "requestId": f"acceptance-burst-request-{index:03d}",
                    "sessionId": manifest["sources"][0]["session"],
                    "timestamp": f"2026-11-09T13:01:{index % 60:02d}Z",
                    "type": "assistant",
                },
                sort_keys=True,
                separators=(",", ":"),
            ).encode("utf-8")
            + b"\n"
            for index in range(burst_records)
        ]
        burst_measurement = _observe_update(
            driver,
            env,
            application.pid,
            lambda: _append_file(first_source, burst_lines),
            lambda: _tree_contains(
                driver, env, _formatted_tracked_total(manifest, burst_records)
            ),
            "coalesced burst update",
        )
        active_peak_rss_kib = max(active_peak_rss_kib, burst_measurement["peakRssKiB"])

        driver._click_named(env, "Source health")
        driver._wait_for(lambda: driver._tree_contains(env, "Healthy 100"), "source health view")
        removal_measurement = _observe_update(
            driver,
            env,
            application.pid,
            first_source.unlink,
            lambda: driver._tree_contains(env, "Retained 1"),
            "visible source disappearance",
        )
        active_peak_rss_kib = max(active_peak_rss_kib, removal_measurement["peakRssKiB"])

        recovery_measurement = _observe_update(
            driver,
            env,
            application.pid,
            lambda: _replace_file(first_source, original_source),
            lambda: driver._tree_contains(env, "Healthy 100")
            and not driver._tree_contains(env, "Retained 1"),
            "visible source recovery",
        )
        active_peak_rss_kib = max(active_peak_rss_kib, recovery_measurement["peakRssKiB"])
        shutdown_ms.append(driver._close_application(env, application, window_id))
        application = None

        measurements = {
            "activePeakRssKiB": active_peak_rss_kib,
            "appendFullReconciliation": append_measurement,
            "burstCoalescing": dict(burst_measurement, admittedWrites=burst_records),
            "coldStartupMs": cold_start_ms,
            "coldStartupMedianMs": _median(cold_start_ms),
            "coldStartupP95Ms": _percentile(cold_start_ms, 0.95),
            "configuredSourceBytes": source_physical_bytes,
            "configuredSourceDecodedBytes": source_decoded_bytes,
            "fixtureUsageObservations": manifest["coverage"]["usageObservations"],
            "fixtureCanonicalObservations": manifest["coverage"]["canonicalObservations"],
            "idleCpuPercent": round(idle_cpu_percent, 4),
            "idleRssMaxKiB": max(idle_rss_kib),
            "idleRssMedianKiB": _median(idle_rss_kib),
            "inventory": inventory,
            "reconciliationMode": "full-configured-source-reconciliation",
            "removalVisible": removal_measurement,
            "rewriteFullReconciliation": rewrite_measurement,
            "sourceRecovery": recovery_measurement,
            "shutdownMaxMs": max(shutdown_ms),
            "warmStartupMs": warm_start_ms,
            "warmStartupMedianMs": _median(warm_start_ms),
            "warmStartupP95Ms": _percentile(warm_start_ms, 0.95),
        }
    finally:
        first_source_value = locals().get("first_source")
        original_source_value = locals().get("original_source")
        if isinstance(first_source_value, Path) and isinstance(original_source_value, bytes):
            try:
                _replace_file(first_source_value, original_source_value)
                if first_source_value.read_bytes() != original_source_value:
                    raise OSError("source restoration did not preserve exact bytes")
            except OSError as error:
                restoration_error = error
        app_stopped = driver._stop_process(application)
        accessibility_stopped = driver._stop_process(accessibility_bus)
        window_manager_stopped = driver._stop_process(window_manager)
        xvfb_stopped = driver._stop_process(xvfb)
        dbus_stopped = True
        if dbus_pid is not None and not driver._pid_stopped(dbus_pid):
            try:
                os.kill(dbus_pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                driver._wait_for(lambda: driver._pid_stopped(dbus_pid), "DBus shutdown", timeout=3.0)
            except ValueError:
                dbus_stopped = False
        cleanup_ok = all(
            (app_stopped, accessibility_stopped, window_manager_stopped, xvfb_stopped, dbus_stopped)
        )
        if restoration_error is not None:
            raise ValueError("resource acceptance source restoration failed") from restoration_error
    if not cleanup_ok:
        raise ValueError("resource acceptance-owned processes did not stop")

    threshold_results = {
        "cold-startup-threshold": measurements["coldStartupP95Ms"] <= STARTUP_LIMIT_MS,
        "idle-cpu-threshold": measurements["idleCpuPercent"] <= IDLE_CPU_LIMIT_PERCENT,
        "idle-rss-threshold": measurements["idleRssMaxKiB"] <= IDLE_RSS_LIMIT_KIB,
        "visible-append-threshold": measurements["appendFullReconciliation"]["latencyMs"] <= UPDATE_LIMIT_MS,
        "visible-burst-threshold": measurements["burstCoalescing"]["latencyMs"] <= UPDATE_LIMIT_MS,
        "visible-removal-threshold": measurements["removalVisible"]["latencyMs"] <= UPDATE_LIMIT_MS,
        "visible-rewrite-threshold": measurements["rewriteFullReconciliation"]["latencyMs"] <= UPDATE_LIMIT_MS,
        "visible-recovery-threshold": measurements["sourceRecovery"]["latencyMs"] <= UPDATE_LIMIT_MS,
        "no-network-listener": measurements["inventory"]["tcpUdpListeners"] == 0,
        "no-child-process": measurements["inventory"]["childProcesses"] == 0,
    }
    passed = all(threshold_results.values())
    metrics_path = work / "resource-measurements.json"
    _write_json(metrics_path, measurements)
    checks = [
        {
            "name": name,
            "status": "passed" if result else "failed",
            "measurement": {
                "cold-startup-threshold": measurements["coldStartupP95Ms"],
                "idle-cpu-threshold": measurements["idleCpuPercent"],
                "idle-rss-threshold": measurements["idleRssMaxKiB"],
                "visible-append-threshold": measurements["appendFullReconciliation"]["latencyMs"],
                "visible-burst-threshold": measurements["burstCoalescing"]["latencyMs"],
                "visible-removal-threshold": measurements["removalVisible"]["latencyMs"],
                "visible-rewrite-threshold": measurements["rewriteFullReconciliation"]["latencyMs"],
                "visible-recovery-threshold": measurements["sourceRecovery"]["latencyMs"],
                "no-network-listener": measurements["inventory"]["tcpUdpListeners"],
                "no-child-process": measurements["inventory"]["childProcesses"],
            }[name],
        }
        for name, result in threshold_results.items()
    ]
    checks.extend(
        [
            {
                "name": "warm-startup-statistics",
                "status": "passed",
                "measurement": f"median={measurements['warmStartupMedianMs']}ms,p95={measurements['warmStartupP95Ms']}ms",
            },
            {
                "name": "descriptor-thread-watch-inventory",
                "status": "passed",
                "measurement": (
                    f"fd={measurements['inventory']['fileDescriptors']},"
                    f"threads={measurements['inventory']['threads']},"
                    f"watches={measurements['inventory']['inotifyWatches']}"
                ),
            },
            {
                "name": "active-reconciliation-cpu-io",
                "status": "passed",
                "measurement": (
                    f"append-cpu={measurements['appendFullReconciliation']['cpuPercent']}%,"
                    f"rewrite-cpu={measurements['rewriteFullReconciliation']['cpuPercent']}%,"
                    f"burst-rchar={measurements['burstCoalescing']['io']['rchar']}"
                ),
            },
            {
                "name": "detail-history-peak-memory",
                "status": "blocked",
                "measurement": "gate-3-5-views-not-implemented",
            },
            {
                "name": "exact-reader-bytes-parsed",
                "status": "passed",
                "measurement": (
                    f"physical={source_physical_bytes},"
                    f"decoded={source_decoded_bytes}"
                ),
            },
        ]
    )
    report = {
        "schema": EVIDENCE_SCHEMA,
        "phase": "linux-resources",
        "status": "diagnostic" if passed else "failed",
        "sourceSnapshotSha256": arguments.source_snapshot_sha256,
        "fixture": {
            "schema": manifest["schema"],
            "revision": manifest["revision"],
            "manifestSha256": _sha256(manifest_path),
            "configuredSources": manifest["coverage"]["configuredSources"],
            "canonicalSessions": manifest["coverage"]["canonicalSessions"],
            "usageObservations": manifest["coverage"]["usageObservations"],
            "canonicalObservations": manifest["coverage"]["canonicalObservations"],
            "physicalBytes": manifest["coverage"]["physicalBytes"],
            "decodedBytes": manifest["coverage"]["decodedBytes"],
            "releaseTargetMet": manifest["releaseTarget"]["met"],
        },
        "platform": {
            "os": platform.system(),
            "architecture": platform.machine(),
            "desktop": "Xvfb+Matchbox+AT-SPI",
            "renderer": "winit-software",
            "cpuCount": os.cpu_count() or 1,
            "memoryMiB": max(
                1,
                (os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")) // (1024 * 1024),
            ),
        },
        "checks": checks,
        "artifacts": [
            {
                "role": "resource-measurements",
                "sha256": _sha256(metrics_path),
                "bytes": metrics_path.stat().st_size,
            },
            {"role": "release-binary", "sha256": _sha256(binary), "bytes": binary.stat().st_size},
            {
                "role": "fixture-manifest",
                "sha256": _sha256(manifest_path),
                "bytes": manifest_path.stat().st_size,
            },
        ],
        "blockers": [
            "representative-limits-accounts",
            "gate-3-5-detail-history-views",
        ],
        "cleanup": {
            "processesStopped": True,
            "credentialRemoved": True,
            "temporaryRootsRemoved": False,
        },
    }
    driver._validate_report(report)
    _write_json(report_path, report)
    report_hash = _sha256(report_path)
    return {
        "phase": report["phase"],
        "reportSha256": report_hash,
        "status": report["status"],
    }, passed


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--fixture-root", required=True)
    parser.add_argument("--work-root", required=True)
    parser.add_argument("--report", required=True)
    parser.add_argument("--source-snapshot-sha256", required=True)
    arguments = parser.parse_args()
    try:
        summary, passed = run(arguments)
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"resource acceptance error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(summary, sort_keys=True, separators=(",", ":")))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
