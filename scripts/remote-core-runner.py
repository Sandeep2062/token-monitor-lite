"""Build/test isolated transferred sources on pixvault-vm, not locally."""

import base64
import hashlib
import difflib
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tarfile
import tempfile

payload = json.loads(Path(sys.argv[1]).read_text())
root = Path.home() / ".local/state/token-monitor-lite/evidence"
root.mkdir(parents=True, exist_ok=True)
snapshot = Path(tempfile.mkdtemp(prefix="core-", dir=root))
for entry in payload["files"]:
    relative = PurePosixPath(entry["path"])
    if relative.is_absolute() or ".." in relative.parts:
        raise SystemExit("Invalid transfer path")
    data = base64.b64decode(entry["bytes"], validate=True)
    if hashlib.sha256(data).hexdigest() != entry["sha256"]:
        raise SystemExit(f"Transfer checksum mismatch: {relative}")
    path = snapshot / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    print(hashlib.sha256(data).hexdigest(), relative, flush=True)
env = dict(os.environ, CARGO_BUILD_JOBS="2", CARGO_TARGET_DIR=str(root / "target"))
env["PATH"] = str(Path.home() / ".cargo/bin") + ":" + env["PATH"]
# Ordinary verification must never discover or auto-activate the builder's
# login credential store. The dedicated lifecycle mode creates its own bus.
env["DBUS_SESSION_BUS_ADDRESS"] = "unix:path=/nonexistent-token-monitor-lite-session-bus"
print("SOURCE_SNAPSHOT=" + str(snapshot), flush=True)
(snapshot / "transfer-manifest.json").write_text(json.dumps(
    [{"path": item["path"], "sha256": item["sha256"]} for item in payload["files"]], indent=2
) + "\n")
node = Path.home() / ".local/share/token-monitor-lite-tools/node-v24.22.2-linux-x64/bin/node"
commands = [[str(node), "scripts/check-catalog.cjs"]]
mode = payload.get("mode", "test")
before_format = {path: path.read_text() for path in snapshot.rglob("*.rs")} if mode == "format" else {}
if mode == "format":
    # Preserve locked transitive versions while admitting the new direct dep.
    # Regenerating the entire lock can reject already-locked yanked packages.
    commands.append(["cargo", "check", "--no-default-features", "--lib"])
    commands.append(["cargo", "fmt", "--all"])
elif mode == "test":
    commands.append(["cargo", "test", "--locked", "--no-default-features", "--test", payload["test"]])
elif mode == "verify":
    commands.extend([
        ["cargo", "fmt", "--all", "--", "--check"],
        ["cargo", "check", "--locked", "--no-default-features", "--all-targets"],
        ["cargo", "test", "--locked", "--no-default-features"],
        ["cargo", "clippy", "--locked", "--no-default-features", "--all-targets", "--", "-D", "warnings"],
        ["cargo", "build", "--locked", "--no-default-features", "--release"],
    ])
elif mode == "desktop-check":
    commands.append(["cargo", "check", "--locked", "--all-targets", "--all-features"])
elif mode == "windows-cross":
    commands.extend([
        ["cargo", "build", "--locked", "--release", "--all-features", "--target", "x86_64-pc-windows-gnu"],
        ["cargo", "test", "--locked", "--no-run", "--all-features", "--target", "x86_64-pc-windows-gnu"],
    ])
elif mode == "desktop-verify":
    commands.extend([
        ["cargo", "fmt", "--all", "--", "--check"],
        ["cargo", "check", "--locked", "--all-targets", "--all-features"],
        ["cargo", "test", "--locked", "--all-targets", "--all-features"],
        ["cargo", "clippy", "--locked", "--all-targets", "--all-features", "--", "-D", "warnings"],
        ["cargo", "build", "--locked", "--release", "--all-features"],
    ])
elif mode == "credential-probe":
    commands.append([
        "cargo", "test", "--locked", "--no-default-features", "--test", "credentials",
        "native_adapter_reports_available_or_unavailable_within_deadline",
        "--", "--ignored", "--nocapture",
    ])
elif mode == "credential-lifecycle":
    commands.append(["python3", "scripts/run-linux-credential-session.py"])
elif mode == "linux-gui":
    snapshot_hash = hashlib.sha256((snapshot / "transfer-manifest.json").read_bytes()).hexdigest()
    commands.extend([
        ["cargo", "build", "--locked", "--release", "--all-features"],
        [
            "python3", "scripts/run-linux-acceptance.py", "gui",
            "--binary", str(root / "target" / "release" / "token-monitor-lite"),
            "--work-root", str(snapshot / "linux-gui-evidence"),
            "--report", str(snapshot / "linux-gui-report.json"),
            "--source-snapshot-sha256", snapshot_hash,
        ],
    ])
elif mode == "linux-resource":
    snapshot_hash = hashlib.sha256((snapshot / "transfer-manifest.json").read_bytes()).hexdigest()
    setup = snapshot / "linux-resource-setup"
    commands.extend([
        ["cargo", "build", "--locked", "--release", "--all-features"],
        [
            "python3", "scripts/run-linux-acceptance.py", "gui",
            "--binary", str(root / "target" / "release" / "token-monitor-lite"),
            "--work-root", str(setup),
            "--report", str(snapshot / "linux-resource-setup-report.json"),
            "--source-snapshot-sha256", snapshot_hash,
        ],
        [
            "python3", "scripts/run-linux-resource-acceptance.py",
            "--binary", str(root / "target" / "release" / "token-monitor-lite"),
            "--home", str(setup / "fixture" / "home"),
            "--fixture-root", str(setup / "fixture"),
            "--work-root", str(snapshot / "linux-resource-evidence"),
            "--report", str(snapshot / "linux-resource-report.json"),
            "--source-snapshot-sha256", snapshot_hash,
        ],
    ])
elif mode == "linux-package":
    snapshot_hash = hashlib.sha256((snapshot / "transfer-manifest.json").read_bytes()).hexdigest()
    commands.extend([
        ["cargo", "build", "--locked", "--release", "--all-features"],
        [
            "python3", "scripts/run-linux-package-acceptance.py",
            "--binary", str(root / "target" / "release" / "token-monitor-lite"),
            "--builder", str(snapshot / "scripts" / "build-linux-packages.py"),
            "--generator", str(snapshot / "scripts" / "generate-acceptance-fixture.py"),
            "--desktop", str(snapshot / "packaging" / "linux" / "token-monitor-lite.desktop"),
            "--metainfo", str(snapshot / "packaging" / "linux" / "io.github.sandeep2062.TokenMonitorLite.metainfo.xml"),
            "--icon", str(snapshot / "packaging" / "linux" / "token-monitor-lite.svg"),
            "--license", str(snapshot / "LICENSE"),
            "--work-root", str(snapshot / "linux-package-evidence"),
            "--report", str(snapshot / "linux-package-report.json"),
            "--source-snapshot-sha256", snapshot_hash,
        ],
    ])
else:
    raise SystemExit("Invalid verification mode")
returncode = 0
for index, command in enumerate(commands):
    print("RUN=" + " ".join(command), flush=True)
    with (snapshot / f"gate-{index}.log").open("w") as log:
        result = subprocess.run(command, cwd=snapshot, env=env, stdout=log, stderr=subprocess.STDOUT)
    print((snapshot / f"gate-{index}.log").read_text(), flush=True)
    if result.returncode:
        returncode = result.returncode
        break
if (snapshot / "Cargo.lock").exists():
    print("LOCKFILE_PATH=" + str(snapshot / "Cargo.lock"), flush=True)
if mode == "windows-cross" and not returncode:
    target = Path(env["CARGO_TARGET_DIR"]) / "x86_64-pc-windows-gnu"
    archive = snapshot / "windows-cross.tar.gz"
    with tarfile.open(archive, "w:gz") as bundle:
        bundle.add(target / "release/token-monitor-lite.exe", arcname="token-monitor-lite.exe")
        for executable in sorted((target / "debug/deps").glob("*.exe")):
            bundle.add(executable, arcname="deps/" + executable.name)
    print("WINDOWS_CROSS_ARCHIVE=" + str(archive), flush=True)
if mode == "format" and not returncode:
    patch = ["*** Begin Patch\n"]
    for path, original in before_format.items():
        current = path.read_text()
        if original == current:
            continue
        patch.append(f"*** Update File: {path.relative_to(snapshot).as_posix()}\n")
        for line in list(difflib.unified_diff(original.splitlines(True), current.splitlines(True)))[2:]:
            patch.append("@@\n" if line.startswith("@@") else line)
    patch.append("*** End Patch\n")
    text = "".join(patch)
    (snapshot / "format.patch").write_text(text)
    print(text, flush=True)
raise SystemExit(returncode)
