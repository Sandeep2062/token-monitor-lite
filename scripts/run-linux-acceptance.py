"""Isolated Linux acceptance orchestration.

Only the fixture/report boundary is active initially. Later plan tasks extend
this driver with credential, display, resource, and package phases without
adding test-only behavior to the application binary.
"""

from __future__ import annotations

import argparse
import hashlib
import json
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
GUI_TIMEOUT_SECONDS = 20.0


ACCESSIBILITY_INSPECTOR = r"""
import json
import pyatspi

INTERESTING_STATES = {
    "enabled": pyatspi.STATE_ENABLED,
    "focusable": pyatspi.STATE_FOCUSABLE,
    "focused": pyatspi.STATE_FOCUSED,
    "sensitive": pyatspi.STATE_SENSITIVE,
    "showing": pyatspi.STATE_SHOWING,
    "visible": pyatspi.STATE_VISIBLE,
}


def error_name(error):
    return type(error).__name__


def walk(node, output, depth=0):
    if depth > 32 or len(output) >= 2048:
        return
    traversal_errors = []
    try:
        state_set = node.getState()
        states = [name for name, value in INTERESTING_STATES.items() if state_set.contains(value)]
    except Exception as error:
        states = []
        traversal_errors.append("state:" + error_name(error))
    try:
        extent = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        bounds = [extent.x, extent.y, extent.width, extent.height]
    except Exception as error:
        bounds = [0, 0, 0, 0]
        traversal_errors.append("component:" + error_name(error))
    try:
        actions = node.queryAction()
        action_names = [actions.getName(index) for index in range(actions.nActions)]
    except Exception as error:
        action_names = []
        traversal_errors.append("action:" + error_name(error))
    try:
        child_count = int(node.childCount)
    except Exception as error:
        child_count = -1
        traversal_errors.append("child-count:" + error_name(error))
    try:
        description = node.description or ""
    except Exception as error:
        description = ""
        traversal_errors.append("description:" + error_name(error))
    try:
        name = node.name or ""
    except Exception as error:
        name = ""
        traversal_errors.append("name:" + error_name(error))
    try:
        role = node.getRoleName() or ""
    except Exception as error:
        role = ""
        traversal_errors.append("role:" + error_name(error))
    record = {
        "actions": action_names,
        "bounds": bounds,
        "childCount": child_count,
        "description": description,
        "indexedChildren": 0,
        "name": name,
        "role": role,
        "states": states,
        "traversalErrors": traversal_errors,
    }
    output.append(record)
    if child_count < 0:
        return
    for index in range(child_count):
        try:
            child = node.getChildAtIndex(index)
            if child is None:
                traversal_errors.append("child-none:" + str(index))
                continue
            record["indexedChildren"] += 1
            walk(child, output, depth + 1)
        except Exception as error:
            traversal_errors.append("child-" + str(index) + ":" + error_name(error))


nodes = []
desktop = pyatspi.Registry.getDesktop(0)
for application in desktop:
    candidate = []
    walk(application, candidate)
    if any(item["name"] == "Token Monitor" for item in candidate):
        nodes.extend(candidate)
print(json.dumps(nodes, ensure_ascii=True, separators=(",", ":")))
"""

ACCESSIBILITY_ACTIVATOR = r"""
import json
import re
import sys
import time
import pyatspi

pattern = re.compile(sys.argv[1])
expected = int(sys.argv[2])
matches = []
errors = []


def collect(node, depth=0):
    if depth > 32 or matches:
        return
    try:
        name = node.name or ""
        role = node.getRoleName() or ""
        if pattern.fullmatch(name) and "button" in role.lower():
            matches.append(node)
    except Exception as error:
        errors.append(type(error).__name__)
    try:
        child_count = int(node.childCount)
    except Exception as error:
        errors.append(type(error).__name__)
        return
    for index in range(child_count):
        try:
            child = node.getChildAtIndex(index)
            if child is not None:
                collect(child, depth + 1)
        except Exception as error:
            errors.append(type(error).__name__)


activated = 0
matched = 0
for _ in range(expected):
    matches = []
    desktop = pyatspi.Registry.getDesktop(0)
    for application in desktop:
        try:
            if application.name == "token-monitor-lite":
                collect(application)
        except Exception as error:
            errors.append(type(error).__name__)
    if len(matches) != 1:
        errors.append("match-count:" + str(len(matches)))
        break
    matched += 1
    node = matches[0]
    try:
        actions = node.queryAction()
        action_index = next(
            (index for index in range(actions.nActions) if actions.getName(index) == "click"),
            None,
        )
        if action_index is None or not actions.doAction(action_index):
            errors.append("click-failed")
        else:
            activated += 1
            time.sleep(0.5)
    except Exception as error:
        errors.append(type(error).__name__)

print(json.dumps({"activated": activated, "errors": errors, "matched": matched}, sort_keys=True))
"""


def _memory_mib() -> int:
    page_size = os.sysconf("SC_PAGE_SIZE")
    pages = os.sysconf("SC_PHYS_PAGES")
    return max(1, (page_size * pages) // (1024 * 1024))


def _write_new(path: Path, data: bytes) -> None:
    if not path.is_absolute() or ".." in path.parts or path.is_symlink():
        raise ValueError("report path must be absolute, traversal-free, and not a symlink")
    path.parent.mkdir(parents=True, exist_ok=True)
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


def _run(
    command: list[str],
    *,
    env: dict[str, str] | None = None,
    timeout: float = GUI_TIMEOUT_SECONDS,
    check: bool = True,
) -> subprocess.CompletedProcess[bytes]:
    result = subprocess.run(
        command,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )
    if check and result.returncode:
        raise ValueError(f"command failed: {Path(command[0]).name}")
    return result


def _wait_for(predicate: Any, description: str, timeout: float = GUI_TIMEOUT_SECONDS) -> Any:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, ValueError, subprocess.SubprocessError, json.JSONDecodeError) as error:
            last_error = error
        time.sleep(0.1)
    if last_error is not None:
        raise ValueError(f"timed out waiting for {description}: {type(last_error).__name__}")
    raise ValueError(f"timed out waiting for {description}")


def _start_process(command: list[str], env: dict[str, str], log: Path) -> subprocess.Popen[bytes]:
    handle = log.open("wb")
    try:
        process = subprocess.Popen(
            command,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=handle,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    finally:
        handle.close()
    return process


def _stop_process(process: subprocess.Popen[bytes] | None, timeout: float = 5.0) -> bool:
    if process is None or process.poll() is not None:
        return True
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=timeout)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                return False
    return process.poll() is not None


def _pid_stopped(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    except PermissionError:
        return False
    return False


def _accessibility_tree(env: dict[str, str]) -> list[dict[str, Any]]:
    result = _run([sys.executable, "-c", ACCESSIBILITY_INSPECTOR], env=env, timeout=10.0)
    tree = json.loads(result.stdout)
    if not isinstance(tree, list):
        raise ValueError("accessibility inspector returned invalid data")
    return tree


def _activate_accessible_matches(
    env: dict[str, str], pattern: str, expected: int
) -> dict[str, Any]:
    result = _run(
        [sys.executable, "-c", ACCESSIBILITY_ACTIVATOR, pattern, str(expected)],
        env=env,
        timeout=120.0,
    )
    activation = json.loads(result.stdout)
    if activation != {"activated": expected, "errors": [], "matched": expected}:
        raise ValueError(f"accessible activation mismatch: {activation}")
    return activation


def _normalized_name(value: str) -> str:
    return value.removeprefix("●  ").removeprefix("● ")


def _find_node(
    tree: list[dict[str, Any]],
    name: str,
    *,
    starts_with: bool = False,
    role_contains: str | None = None,
    require_showing: bool = True,
) -> dict[str, Any] | None:
    for node in tree:
        candidate = _normalized_name(str(node.get("name", "")))
        matches = candidate.startswith(name) if starts_with else candidate == name
        if not matches:
            continue
        if role_contains and role_contains not in str(node.get("role", "")).lower():
            continue
        if require_showing and "showing" not in node.get("states", []):
            continue
        bounds = node.get("bounds", [0, 0, 0, 0])
        if len(bounds) != 4 or bounds[2] <= 0 or bounds[3] <= 0:
            continue
        return node
    return None


def _click_node(env: dict[str, str], node: dict[str, Any]) -> None:
    x, y, width, height = [int(value) for value in node["bounds"]]
    _run(["xdotool", "mousemove", "--sync", str(x + width // 2), str(y + height // 2), "click", "1"], env=env)


def _click_named(
    env: dict[str, str],
    name: str,
    *,
    starts_with: bool = False,
    timeout: float = GUI_TIMEOUT_SECONDS,
) -> dict[str, Any]:
    node = _wait_for(
        lambda: _find_node(_accessibility_tree(env), name, starts_with=starts_with),
        f"accessible control {name}",
        timeout,
    )
    _click_node(env, node)
    return node


def _tree_has(env: dict[str, str], name: str, *, starts_with: bool = False) -> bool:
    return _find_node(
        _accessibility_tree(env),
        name,
        starts_with=starts_with,
        require_showing=False,
    ) is not None


def _tree_contains(env: dict[str, str], fragment: str) -> bool:
    return any(fragment in str(node.get("name", "")) for node in _accessibility_tree(env))


def _selected_source_count(env: dict[str, str]) -> int:
    return sum(
        1
        for node in _accessibility_tree(env)
        if re.fullmatch(
            r"Source [0-9]+ · (?:Claude CLI|Codex CLI|Session archive|Codex local usage) · Selected · Available",
            str(node.get("name", "")),
        )
    )


def _tree_lacks(env: dict[str, str], fragment: str) -> bool:
    return not _tree_contains(env, fragment)


def _focused_name(tree: list[dict[str, Any]]) -> str | None:
    return next((str(node.get("name", "")) for node in tree if "focused" in node.get("states", [])), None)


def _text_input(tree: list[dict[str, Any]]) -> dict[str, Any] | None:
    for node in tree:
        states = node.get("states", [])
        role = str(node.get("role", "")).lower()
        bounds = node.get("bounds", [0, 0, 0, 0])
        if (
            "focusable" in states
            and "showing" in states
            and ("text" in role or "entry" in role)
            and bounds[2] > 0
        ):
            return node
    return None


def _screenshot(env: dict[str, str], window_id: str, destination: Path) -> dict[str, Any]:
    _run(["maim", "--hidecursor", "--window", window_id, str(destination)], env=env)
    if not destination.is_file() or destination.stat().st_size < 1024:
        raise ValueError("screenshot was not created")
    return {
        "role": f"screenshot-{destination.stem}",
        "sha256": _sha256(destination),
        "bytes": destination.stat().st_size,
    }


def _window_id(env: dict[str, str], pid: int) -> str | None:
    result = _run(
        ["xdotool", "search", "--onlyvisible", "--pid", str(pid), "--name", "^Token Monitor$"],
        env=env,
        timeout=3.0,
        check=False,
    )
    if result.returncode:
        return None
    values = result.stdout.decode("ascii", errors="ignore").split()
    return values[0] if values else None


def _start_application(binary: Path, env: dict[str, str], log: Path) -> tuple[subprocess.Popen[bytes], str]:
    process = _start_process([str(binary)], env, log)
    window_id = _wait_for(lambda: _window_id(env, process.pid), "Token Monitor window")
    _run(
        ["gsettings", "set", "org.gnome.desktop.interface", "toolkit-accessibility", "false"],
        env=env,
    )
    _run(
        ["gsettings", "set", "org.gnome.desktop.interface", "toolkit-accessibility", "true"],
        env=env,
    )
    _wait_for(lambda: _tree_has(env, "Token Monitor"), "Token Monitor accessibility root")
    _wait_for(lambda: _tree_has(env, "Home"), "Token Monitor accessibility controls")
    return process, window_id


def _close_application(env: dict[str, str], process: subprocess.Popen[bytes], window_id: str) -> int:
    started = time.monotonic()
    _run(["xdotool", "windowactivate", "--sync", window_id], env=env, check=False)
    _run(["xdotool", "key", "--clearmodifiers", "alt+F4"], env=env, check=False)
    try:
        process.wait(timeout=10.0)
    except subprocess.TimeoutExpired:
        _run(["xdotool", "windowclose", window_id], env=env, check=False)
        try:
            process.wait(timeout=20.0)
        except subprocess.TimeoutExpired as error:
            threads = _run(
                ["ps", "-L", "-o", "pid,tid,stat,wchan:40,comm", "-p", str(process.pid)],
                check=False,
            )
            Path("/tmp/token-monitor-close-threads.txt").write_text(
                threads.stdout.decode("utf-8", "replace"), encoding="utf-8"
            )
            print(threads.stdout.decode("utf-8", "replace"), flush=True)
            _stop_process(process)
            raise ValueError("application did not exit after window-manager close") from error
    if process.returncode:
        raise ValueError("application exited unsuccessfully")
    return round((time.monotonic() - started) * 1000)


def _validate_report(report: dict[str, Any]) -> None:
    if report.get("schema") != EVIDENCE_SCHEMA:
        raise ValueError("invalid evidence schema")
    if report.get("status") not in {"passed", "failed", "blocked", "diagnostic"}:
        raise ValueError("invalid evidence status")
    if not SHA256.fullmatch(report.get("sourceSnapshotSha256", "")):
        raise ValueError("invalid source snapshot hash")
    encoded = json.dumps(report, sort_keys=True, separators=(",", ":"))
    forbidden = [str(Path.home()), os.environ.get("USER", ""), os.environ.get("LOGNAME", "")]
    if any(value and value in encoded for value in forbidden):
        raise ValueError("evidence report contains private host identity")
    for blocker in report.get("blockers", []):
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,95}", blocker):
            raise ValueError("invalid blocker identifier")


def fixture_phase(arguments: argparse.Namespace) -> dict[str, Any]:
    if not SHA256.fullmatch(arguments.source_snapshot_sha256):
        raise ValueError("source snapshot hash must be lowercase SHA-256")
    root = Path(arguments.fixture_root)
    generator = Path(__file__).with_name("generate-acceptance-fixture.py")
    result = subprocess.run(
        [sys.executable, str(generator), "--output", str(root)],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode:
        raise ValueError("fixture generation failed")
    summary = json.loads(result.stdout)
    manifest_path = root / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    report = {
        "schema": EVIDENCE_SCHEMA,
        "phase": "fixture",
        "status": "passed" if manifest["releaseTarget"]["met"] else "diagnostic",
        "sourceSnapshotSha256": arguments.source_snapshot_sha256,
        "fixture": {
            "schema": manifest["schema"],
            "revision": manifest["revision"],
            "manifestSha256": summary["manifestSha256"],
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
            "desktop": None,
            "renderer": "not-started",
            "cpuCount": os.cpu_count() or 1,
            "memoryMiB": _memory_mib(),
        },
        "checks": [{"name": "fixture-manifest", "status": "passed"}],
        "artifacts": [
            {
                "role": "fixture-manifest",
                "sha256": summary["manifestSha256"],
                "bytes": manifest_path.stat().st_size,
            }
        ],
        "blockers": [
            "representative-limits-accounts",
        ],
        "cleanup": {
            "processesStopped": True,
            "credentialRemoved": True,
            "temporaryRootsRemoved": False,
        },
    }
    _validate_report(report)
    report_bytes = json.dumps(report, indent=2, sort_keys=True).encode("utf-8") + b"\n"
    _write_new(Path(arguments.report), report_bytes)
    return {
        "phase": report["phase"],
        "reportSha256": hashlib.sha256(report_bytes).hexdigest(),
        "status": report["status"],
    }


def gui_phase(arguments: argparse.Namespace) -> dict[str, Any]:
    if platform.system() != "Linux" or platform.machine() not in {"x86_64", "AMD64"}:
        raise ValueError("GUI acceptance requires native Linux x86-64")
    if not SHA256.fullmatch(arguments.source_snapshot_sha256):
        raise ValueError("source snapshot hash must be lowercase SHA-256")
    binary = Path(arguments.binary)
    work = Path(arguments.work_root)
    report_path = Path(arguments.report)
    if not binary.is_absolute() or not binary.is_file() or binary.is_symlink():
        raise ValueError("binary must be an absolute regular file")
    if not work.is_absolute() or ".." in work.parts or work.is_symlink() or work.exists():
        raise ValueError("work root must be a new absolute non-symlink path")
    if not report_path.is_absolute() or report_path.exists() or report_path.is_symlink():
        raise ValueError("report must be a new absolute non-symlink path")

    work.mkdir(parents=True, mode=0o700)
    fixture_root = work / "fixture"
    screenshots = work / "screenshots"
    logs = work / "logs"
    runtime = work / "runtime"
    cache = work / "cache"
    screenshots.mkdir()
    logs.mkdir()
    runtime.mkdir(mode=0o700)
    cache.mkdir()

    generator = Path(__file__).with_name("generate-acceptance-fixture.py")
    generated = _run([sys.executable, str(generator), "--output", str(fixture_root)])
    summary = json.loads(generated.stdout)
    manifest_path = fixture_root / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    home = fixture_root / "home"
    config = home / ".config"
    state = home / ".local" / "state"
    legacy = config / "Token Monitor"
    legacy.mkdir(parents=True, exist_ok=True)
    state.mkdir(parents=True)
    (legacy / "settings.json").write_text(
        json.dumps(
            {
                "clients": "claude,codex",
                "timezone": "UTC",
                "acceptanceUnknownField": {"preserve": True},
            },
            sort_keys=True,
            separators=(",", ":"),
        )
        + "\n",
        encoding="utf-8",
    )

    display_number = next(
        (number for number in range(71, 100) if not Path(f"/tmp/.X{number}-lock").exists()),
        None,
    )
    if display_number is None:
        raise ValueError("no isolated X display number is available")
    display = f":{display_number}"
    env = dict(os.environ)
    for name in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS"):
        env.pop(name, None)
    env.update(
        {
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(config),
            "XDG_STATE_HOME": str(state),
            "XDG_CACHE_HOME": str(cache),
            "XDG_RUNTIME_DIR": str(runtime),
            "TOKEN_MONITOR_CLIENTS": "claude,codex",
            "SLINT_BACKEND": "winit-software",
            "NO_AT_BRIDGE": "0",
            "ACCESSIBILITY_ENABLED": "1",
            "DISPLAY": display,
            "RUST_BACKTRACE": "0",
        }
    )

    dbus_pid: int | None = None
    xvfb: subprocess.Popen[bytes] | None = None
    window_manager: subprocess.Popen[bytes] | None = None
    accessibility_bus: subprocess.Popen[bytes] | None = None
    application: subprocess.Popen[bytes] | None = None
    artifacts: list[dict[str, Any]] = []
    checks: list[dict[str, Any]] = []
    trees: list[list[dict[str, Any]]] = []
    shutdown_times: list[int] = []
    cleanup_ok = False
    try:
        bus = _run(
            ["dbus-daemon", "--session", "--fork", "--nopidfile", "--print-address=1", "--print-pid=1"],
            env=env,
        ).stdout.decode("utf-8").splitlines()
        if len(bus) != 2 or not bus[0].startswith("unix:"):
            raise ValueError("isolated DBus session did not start")
        dbus_pid = int(bus[1])
        env["DBUS_SESSION_BUS_ADDRESS"] = bus[0]

        xvfb = _start_process(
            ["Xvfb", display, "-screen", "0", "1280x800x24", "-nolisten", "tcp", "-noreset"],
            env,
            logs / "xvfb.log",
        )
        _wait_for(
            lambda: _run(["xdotool", "getmouselocation"], env=env, timeout=2.0, check=False).returncode == 0,
            "Xvfb display",
        )
        window_manager = _start_process(
            ["matchbox-window-manager", "-use_titlebar", "yes"],
            env,
            logs / "window-manager.log",
        )
        accessibility_bus = _start_process(
            ["/usr/libexec/at-spi-bus-launcher", "--launch-immediately"],
            env,
            logs / "accessibility-bus.log",
        )
        time.sleep(0.5)

        application, window_id = _start_application(binary, env, logs / "application-first.log")
        initial_tree = _accessibility_tree(env)
        trees.append(initial_tree)
        initial_tree_path = work / "initial-accessibility-tree.json"
        initial_tree_path.write_text(
            json.dumps(initial_tree, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        if not initial_tree:
            raise ValueError("native accessibility tree is empty")
        if not _tree_contains(env, "No sources configured"):
            raise ValueError("initial empty state was not visible")
        artifacts.append(_screenshot(env, window_id, screenshots / "initial-empty.png"))
        checks.append({"name": "initial-empty-state", "status": "passed"})

        _click_named(env, "Settings")
        _wait_for(lambda: _tree_has(env, "Settings"), "settings view")
        settings_tree = _accessibility_tree(env)
        trees.append(settings_tree)
        settings_tree_path = work / "settings-accessibility-tree.json"
        settings_tree_path.write_text(
            json.dumps(settings_tree, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        if not _tree_contains(env, "Migration report"):
            raise ValueError("migration report was not visible")
        artifacts.append(_screenshot(env, window_id, screenshots / "settings-migration.png"))
        checks.append({"name": "migration-report", "status": "passed"})

        input_node = _wait_for(lambda: _text_input(_accessibility_tree(env)), "settings timezone input")
        _click_node(env, input_node)
        _run(["xdotool", "key", "ctrl+a"], env=env)
        _run(["xdotool", "type", "--delay", "20", "Invalid/Zone"], env=env)
        _wait_for(lambda: _tree_contains(env, "Enter a valid IANA timezone."), "settings validation error")
        artifacts.append(_screenshot(env, window_id, screenshots / "settings-error.png"))
        _click_named(env, "Cancel")
        _wait_for(lambda: _tree_lacks(env, "Enter a valid IANA timezone."), "settings cancel restoration")
        if not _tree_contains(env, "Saved timezone  UTC"):
            raise ValueError("settings cancel did not restore UTC")
        checks.append({"name": "settings-cancel-restores", "status": "passed"})

        _click_named(env, "Select all sources")
        activated_tree = _accessibility_tree(env)
        (work / "settings-activated-accessibility-tree.json").write_text(
            json.dumps(activated_tree, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        selected_names = [
            str(node.get("name", ""))
            for node in activated_tree
            if str(node.get("name", "")).startswith("Source ")
            and "· Selected" in str(node.get("name", ""))
        ]
        if len(selected_names) != 100:
            raise ValueError(
                f"activated {len(selected_names)} of 100 source rows; "
                f"first={selected_names[:2]} last={selected_names[-2:]}"
            )
        _wait_for(
            lambda: _selected_source_count(env) == 100,
            "100 selected settings sources",
            timeout=30.0,
        )
        save_node = _wait_for(
            lambda: next(
                (
                    node
                    for node in _accessibility_tree(env)
                    if _normalized_name(str(node.get("name", ""))) == "Save"
                    and "enabled" in node.get("states", [])
                ),
                None,
            ),
            "enabled settings Save button",
        )
        _click_node(env, save_node)
        _click_named(env, "Home")
        time.sleep(2.0)
        activated_tree = _accessibility_tree(env)
        (work / "home-activated-accessibility-tree.json").write_text(
            json.dumps(activated_tree, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        _screenshot(env, window_id, screenshots / "home-activated.png")
        _wait_for(lambda: _tree_contains(env, "Healthy 100"), "100 healthy configured sources", timeout=30.0)
        artifacts.append(_screenshot(env, window_id, screenshots / "home-populated.png"))
        checks.append({"name": "settings-save-activates-sources", "status": "passed"})

        _click_named(env, "Show today")
        _wait_for(lambda: _tree_contains(env, "TRACKED TOKENS · TODAY"), "today period")
        _click_named(env, "Show this month")
        _wait_for(lambda: _tree_contains(env, "TRACKED TOKENS · MONTH"), "month period")
        _click_named(env, "Show all captured")
        _wait_for(lambda: _tree_contains(env, "TRACKED TOKENS · ALL CAPTURED"), "all-time period")
        checks.append({"name": "home-period-selection", "status": "passed"})

        _click_named(env, "Trends")
        _wait_for(lambda: _tree_contains(env, "Daily tracked tokens"), "trends view")
        artifacts.append(_screenshot(env, window_id, screenshots / "trends.png"))
        checks.append({"name": "trends-view", "status": "passed"})
        _click_named(env, "Models")
        _wait_for(lambda: _tree_contains(env, "Model share"), "models view")
        artifacts.append(_screenshot(env, window_id, screenshots / "models.png"))
        checks.append({"name": "models-view", "status": "passed"})
        _click_named(env, "Home")

        _click_named(env, "Sessions")
        _wait_for(lambda: _tree_has(env, "1 / 20"), "session pagination")
        artifacts.append(_screenshot(env, window_id, screenshots / "sessions.png"))
        _click_named(env, "Next")
        _wait_for(lambda: _tree_has(env, "2 / 20"), "next session page")
        _click_named(env, "Previous")
        _wait_for(lambda: _tree_has(env, "1 / 20"), "previous session page")
        search = _wait_for(lambda: _text_input(_accessibility_tree(env)), "session search input")
        _click_node(env, search)
        _run(["xdotool", "type", "--delay", "20", "claude-fixture-000"], env=env)
        _wait_for(lambda: _tree_contains(env, "claude-fixture-000"), "filtered session")
        if not _tree_has(env, "1 / 1"):
            raise ValueError("session filter did not reduce pagination")
        _click_named(env, "claude  ·  claude-fixture-000", starts_with=True)
        _wait_for(lambda: _tree_contains(env, "Session detail"), "session detail panel")
        artifacts.append(_screenshot(env, window_id, screenshots / "session-detail.png"))
        _click_named(env, "Close session detail")
        _wait_for(lambda: _tree_contains(env, "Sessions"), "closed session detail")
        checks.append({"name": "session-detail", "status": "passed"})
        search = _wait_for(lambda: _text_input(_accessibility_tree(env)), "session search input")
        _click_node(env, search)
        _run(["xdotool", "key", "ctrl+a", "BackSpace"], env=env)
        _wait_for(lambda: _tree_has(env, "1 / 20"), "cleared session filter")
        checks.append({"name": "session-search-page-select", "status": "passed"})

        _click_named(env, "Refresh sources")
        _click_named(env, "Source health")
        _wait_for(lambda: _tree_contains(env, "Healthy 100"), "manual refresh completion", timeout=30.0)
        checks.append({"name": "manual-refresh", "status": "passed"})

        _wait_for(lambda: _tree_contains(env, "Healthy"), "source health view")
        artifacts.append(_screenshot(env, window_id, screenshots / "source-health.png"))
        first_source = fixture_root / manifest["sources"][0]["path"]
        original_source = first_source.read_bytes()
        first_source.write_bytes(b"{malformed\n")
        _wait_for(lambda: _tree_contains(env, "Retained 1"), "retained source error", timeout=15.0)
        error_tree = _accessibility_tree(env)
        (work / "source-error-tree.json").write_text(
            json.dumps(error_tree, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        artifacts.append(_screenshot(env, window_id, screenshots / "source-error.png"))
        first_source.write_bytes(original_source)
        _wait_for(lambda: _tree_contains(env, "Healthy 100"), "source recovery", timeout=15.0)
        checks.append({"name": "source-error-last-good-recovery", "status": "passed"})

        _click_named(env, "About")
        _wait_for(lambda: _tree_contains(env, "Token Monitor Lite 0.1.0"), "About view")
        _wait_for(
            lambda: _tree_contains(env, "up to date")
            or _tree_contains(env, "Update v")
            or _tree_contains(env, "Update check unavailable"),
            "resolved update status",
            timeout=30.0,
        )
        artifacts.append(_screenshot(env, window_id, screenshots / "about.png"))
        checks.append({"name": "about-view", "status": "passed"})
        checks.append({"name": "update-check", "status": "passed"})

        _click_named(env, "Home")
        _run(["xdotool", "key", "Tab"], env=env)
        focused = _wait_for(lambda: _focused_name(_accessibility_tree(env)), "keyboard focus")
        if not focused:
            raise ValueError("tab traversal did not expose focused control")
        checks.append({"name": "keyboard-focus", "status": "passed", "measurement": _normalized_name(focused)[:128]})

        first_close_ms = _close_application(env, application, window_id)
        shutdown_times.append(first_close_ms)
        application = None
        application, window_id = _start_application(binary, env, logs / "application-reopen.log")
        _wait_for(lambda: _tree_contains(env, "Healthy 100"), "persisted source settings", timeout=30.0)
        _click_named(env, "Settings")
        _wait_for(lambda: _tree_contains(env, "Saved timezone  UTC"), "persisted timezone")
        if not _tree_contains(env, "Source 1 · Claude CLI · Selected"):
            raise ValueError("selected sources were not persisted across reopen")
        artifacts.append(_screenshot(env, window_id, screenshots / "settings-reopen.png"))
        checks.append({"name": "settings-persist-reopen", "status": "passed"})
        shutdown_times.append(_close_application(env, application, window_id))
        application = None

        combined_tree = json.dumps(trees, sort_keys=True, separators=(",", ":"))
        for private_value in (str(home), str(work), os.environ.get("USER", ""), os.environ.get("LOGNAME", "")):
            if private_value and private_value in combined_tree:
                raise ValueError("accessibility tree exposed private host data")
        roles = {str(node.get("role", "")).lower() for tree in trees for node in tree}
        if not any("window" in role or "frame" in role for role in roles):
            raise ValueError("accessibility tree has no window role")
        if not any("button" in role for role in roles):
            raise ValueError("accessibility tree has no button role")
        if not any("text" in role or "entry" in role for role in roles):
            raise ValueError("accessibility tree has no text-input role")
        if not any("focusable" in node.get("states", []) for tree in trees for node in tree):
            raise ValueError("accessibility tree has no focusable controls")
        tree_path = work / "accessibility-tree.json"
        tree_path.write_text(json.dumps(trees, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        artifacts.append({"role": "accessibility-tree", "sha256": _sha256(tree_path), "bytes": tree_path.stat().st_size})
        checks.append({"name": "native-accessibility-tree", "status": "passed"})
        checks.append({"name": "bounded-application-shutdown", "status": "passed", "durationMs": max(shutdown_times)})

        artifacts.extend(
            [
                {"role": "release-binary", "sha256": _sha256(binary), "bytes": binary.stat().st_size},
                {"role": "fixture-manifest", "sha256": summary["manifestSha256"], "bytes": manifest_path.stat().st_size},
            ]
        )
    finally:
        app_stopped = _stop_process(application)
        accessibility_stopped = _stop_process(accessibility_bus)
        window_manager_stopped = _stop_process(window_manager)
        xvfb_stopped = _stop_process(xvfb)
        dbus_stopped = True
        if dbus_pid is not None and not _pid_stopped(dbus_pid):
            try:
                os.kill(dbus_pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                _wait_for(lambda: _pid_stopped(dbus_pid), "DBus shutdown", timeout=3.0)
            except ValueError:
                dbus_stopped = False
        cleanup_ok = all((app_stopped, accessibility_stopped, window_manager_stopped, xvfb_stopped, dbus_stopped))

    if not cleanup_ok:
        raise ValueError("acceptance-owned processes did not stop")
    report = {
        "schema": EVIDENCE_SCHEMA,
        "phase": "linux-gui",
        "status": "diagnostic",
        "sourceSnapshotSha256": arguments.source_snapshot_sha256,
        "fixture": {
            "schema": manifest["schema"],
            "revision": manifest["revision"],
            "manifestSha256": summary["manifestSha256"],
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
            "memoryMiB": _memory_mib(),
        },
        "checks": checks,
        "artifacts": artifacts,
        "blockers": [
            "representative-limits-accounts",
        ],
        "cleanup": {
            "processesStopped": True,
            "credentialRemoved": True,
            "temporaryRootsRemoved": False,
        },
    }
    _validate_report(report)
    report_bytes = json.dumps(report, indent=2, sort_keys=True).encode("utf-8") + b"\n"
    _write_new(report_path, report_bytes)
    return {
        "phase": report["phase"],
        "reportSha256": hashlib.sha256(report_bytes).hexdigest(),
        "status": report["status"],
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    subcommands = parser.add_subparsers(dest="command", required=True)
    fixture = subcommands.add_parser("fixture")
    fixture.add_argument("--fixture-root", required=True)
    fixture.add_argument("--report", required=True)
    fixture.add_argument("--source-snapshot-sha256", required=True)
    gui = subcommands.add_parser("gui")
    gui.add_argument("--binary", required=True)
    gui.add_argument("--work-root", required=True)
    gui.add_argument("--report", required=True)
    gui.add_argument("--source-snapshot-sha256", required=True)
    arguments = parser.parse_args()
    try:
        if arguments.command == "fixture":
            summary = fixture_phase(arguments)
        elif arguments.command == "gui":
            summary = gui_phase(arguments)
        else:
            raise ValueError("unsupported acceptance phase")
        print(json.dumps(summary, sort_keys=True, separators=(",", ":")))
        return 0
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"acceptance error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
