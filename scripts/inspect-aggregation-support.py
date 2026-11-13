"""Acquire exact pinned aggregation support files on the SSH VM."""

import hashlib
import json
from pathlib import Path
import urllib.request

PIN = "d3df620b2cdd9502c70aececdbc6ece61fe26fc9"
ROOT = Path.home() / ".local/state/token-monitor-lite/evidence" / f"tokscale-{PIN}"
WANTED = {
    "crates/tokscale-core/src/aggregator.rs",
    "crates/tokscale-core/src/bucket_tz.rs",
    "crates/tokscale-core/src/lib.rs",
}

with urllib.request.urlopen(
    f"https://api.github.com/repos/Javis603/tokscale/git/trees/{PIN}?recursive=1",
    timeout=60,
) as response:
    tree = json.load(response)
if tree.get("truncated"):
    raise SystemExit("Pinned inventory truncated")

available = {row["path"] for row in tree["tree"] if row["type"] == "blob"}
missing = sorted(WANTED - available)
if missing:
    raise SystemExit(f"Pinned aggregation paths missing: {missing}")

manifest = []
for path in sorted(WANTED):
    with urllib.request.urlopen(
        f"https://raw.githubusercontent.com/Javis603/tokscale/{PIN}/{path}", timeout=60
    ) as response:
        data = response.read()
    destination = ROOT / path
    if destination.exists() and destination.read_bytes() != data:
        raise SystemExit(f"Refusing to overwrite differing evidence: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(data)
    manifest.append(
        {"path": path, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    )

manifest_path = ROOT / "aggregation-boundary-manifest.json"
if manifest_path.exists():
    if json.loads(manifest_path.read_text()) != manifest:
        raise SystemExit(f"Refusing to overwrite differing manifest: {manifest_path}")
else:
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
print(json.dumps(manifest, indent=2))
