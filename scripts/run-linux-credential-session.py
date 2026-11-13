"""Run the ignored real credential lifecycle in an isolated Secret Service."""

from __future__ import annotations

import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time


OBJECT_PATH = re.compile(r'"(/org/freedesktop/secrets/collection/[^"\s]+)"')


def wait_for_secret_service(environment: dict[str, str], daemon: subprocess.Popen[bytes]) -> bool:
    for _ in range(50):
        if daemon.poll() is not None:
            return False
        result = subprocess.run(
            [
                "busctl",
                "--user",
                "--timeout=1",
                "call",
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                "s",
                "org.freedesktop.secrets",
            ],
            env=environment,
            stdout=subprocess.PIPE,
            text=True,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if result.returncode == 0 and result.stdout.strip() == "b true":
            return True
        time.sleep(0.1)
    return False


def application_item_state(environment: dict[str, str]) -> str:
    result = subprocess.run(
        [
            "busctl",
            "--user",
            "--timeout=5",
            "call",
            "org.freedesktop.secrets",
            "/org/freedesktop/secrets",
            "org.freedesktop.Secret.Service",
            "SearchItems",
            "a{ss}",
            "1",
            "service",
            "token-monitor-lite",
        ],
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return "query-failed"
    if OBJECT_PATH.search(result.stdout) is None:
        return "empty"
    return "not-empty"


def ensure_default_collection(environment: dict[str, str]) -> bool:
    read_alias = [
        "busctl",
        "--user",
        "--timeout=5",
        "call",
        "org.freedesktop.secrets",
        "/org/freedesktop/secrets",
        "org.freedesktop.Secret.Service",
        "ReadAlias",
        "s",
        "default",
    ]
    current = subprocess.run(
        read_alias,
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )
    current_path = OBJECT_PATH.search(current.stdout)
    if current.returncode == 0 and current_path is not None:
        return True

    collections = subprocess.run(
        [
            "busctl",
            "--user",
            "--timeout=5",
            "get-property",
            "org.freedesktop.secrets",
            "/org/freedesktop/secrets",
            "org.freedesktop.Secret.Service",
            "Collections",
        ],
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )
    paths = OBJECT_PATH.findall(collections.stdout)
    if collections.returncode != 0 or not paths:
        return False
    login_path = next((path for path in paths if path.endswith("/login")), paths[0])
    assigned = subprocess.run(
        [
            "busctl",
            "--user",
            "--timeout=5",
            "call",
            "org.freedesktop.secrets",
            "/org/freedesktop/secrets",
            "org.freedesktop.Secret.Service",
            "SetAlias",
            "so",
            "default",
            login_path,
        ],
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if assigned.returncode != 0:
        return False
    verified = subprocess.run(
        read_alias,
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )
    return verified.returncode == 0 and login_path in verified.stdout


def run_child() -> int:
    keyring = shutil.which("gnome-keyring-daemon")
    cargo = shutil.which("cargo")
    if keyring is None or cargo is None:
        print("credential_session=blocked-missing-keyring")
        return 2
    original_home = Path.home()
    root = Path(tempfile.mkdtemp(prefix="token-monitor-credential-"))
    home = root / "home"
    runtime = root / "runtime"
    home.mkdir(mode=0o700)
    runtime.mkdir(mode=0o700)
    password = bytearray(os.urandom(32).hex().encode("ascii") + b"\n")
    environment = dict(os.environ)
    environment.update(
        HOME=str(home),
        XDG_CONFIG_HOME=str(home / ".config"),
        XDG_DATA_HOME=str(home / ".local/share"),
        XDG_CACHE_HOME=str(home / ".cache"),
        XDG_STATE_HOME=str(home / ".local/state"),
        XDG_RUNTIME_DIR=str(runtime),
        CARGO_HOME=os.environ.get("CARGO_HOME", str(original_home / ".cargo")),
        RUSTUP_HOME=os.environ.get("RUSTUP_HOME", str(original_home / ".rustup")),
        TOKEN_MONITOR_LIVE_CREDENTIAL_WRITE="1",
    )
    daemon = subprocess.Popen(
        [keyring, "--foreground", "--components=secrets"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_for_secret_service(environment, daemon):
            print("credential_session=blocked-keyring-startup")
            return 2
        unlock = subprocess.Popen(
            [keyring, "--unlock"],
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            if unlock.stdin is None:
                print("credential_session=blocked-keyring-stdin")
                return 2
            unlock.stdin.write(password)
            unlock.stdin.close()
        finally:
            password[:] = b"\0" * len(password)
        if unlock.wait(timeout=5) != 0:
            print("credential_session=blocked-keyring-unlock")
            return 2
        if not ensure_default_collection(environment):
            print("credential_session=blocked-default-collection")
            return 2
        command = [
            cargo,
            "test",
            "--locked",
            "--no-default-features",
            "--test",
            "credentials",
            "native_adapter_completes_real_store_lifecycle_without_leaking_secret",
            "--",
            "--ignored",
            "--nocapture",
        ]
        result = subprocess.run(command, env=environment, check=False)
        item_state = application_item_state(environment)
        if item_state != "empty":
            print(f"credential_session=failed-store-{item_state}")
            return 1
        if result.returncode == 0:
            print("credential_session=passed-empty-store")
        return result.returncode
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=5)
        shutil.rmtree(root, ignore_errors=True)


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "--inside-session":
        return run_child()
    dbus = shutil.which("dbus-run-session")
    if dbus is None:
        print("credential_session=blocked-missing-dbus")
        return 2
    return subprocess.run(
        [dbus, "--", sys.executable, str(Path(__file__).resolve()), "--inside-session"],
        check=False,
    ).returncode


if __name__ == "__main__":
    raise SystemExit(main())
