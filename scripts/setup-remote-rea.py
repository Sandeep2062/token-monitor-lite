"""Install explicitly approved, isolated analysis tooling on the SSH VM."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import urllib.request

ROOT = Path.home() / ".local/share/token-monitor-lite-tools"
NODE_VERSION = "v24.22.2"
REA_VERSION = "6.7.0"


def download(url, destination):
    with urllib.request.urlopen(url, timeout=60) as response, destination.open("xb") as output:
        while chunk := response.read(1024 * 1024):
            output.write(chunk)


if ROOT.exists():
    raise SystemExit(f"Refusing to overwrite existing tooling directory: {ROOT}")
ROOT.mkdir(parents=True, mode=0o700)
archive_name = f"node-{NODE_VERSION}-linux-x64.tar.xz"
base_url = f"https://nodejs.org/dist/{NODE_VERSION}/"
with urllib.request.urlopen(base_url + "SHASUMS256.txt", timeout=30) as response:
    sums = response.read().decode("ascii")
expected = next(line.split()[0] for line in sums.splitlines() if line.split()[-1] == archive_name)
archive = ROOT / archive_name
download(base_url + archive_name, archive)
actual = hashlib.file_digest(archive.open("rb"), "sha256").hexdigest()
if actual != expected:
    raise SystemExit("Official Node archive checksum mismatch; nothing executed")
with tarfile.open(archive, "r:xz") as package:
    package.extractall(ROOT, filter="data")
node_home = ROOT / f"node-{NODE_VERSION}-linux-x64"
environment = dict(os.environ, PATH=f"{node_home / 'bin'}:{os.environ['PATH']}")
rea_home = ROOT / f"rea-{REA_VERSION}"
subprocess.run(
    [str(node_home / "bin/npm"), "install", "--prefix", str(rea_home), "--ignore-scripts",
     "--no-audit", "--no-fund", "--save-exact", f"rea-agents@{REA_VERSION}"],
    env=environment, check=True,
)
metadata = {
    "node_version": NODE_VERSION, "node_sha256": actual, "rea_version": REA_VERSION,
    "node_home": str(node_home), "rea_home": str(rea_home), "install_scripts": False,
}
(ROOT / "installation.json").write_text(json.dumps(metadata, indent=2) + "\n")
print(json.dumps(metadata, indent=2))
subprocess.run([str(rea_home / "node_modules/.bin/rea"), "--help"], env=environment, check=True)
