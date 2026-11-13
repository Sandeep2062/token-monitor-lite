"""Fetch exact parser provenance into a new VM-owned evidence directory."""

import hashlib
import json
from pathlib import Path
import urllib.request

PIN = "d3df620b2cdd9502c70aececdbc6ece61fe26fc9"
ROOT = Path.home() / ".local/state/token-monitor-lite/evidence" / f"tokscale-{PIN}"
with urllib.request.urlopen(
    f"https://api.github.com/repos/Javis603/tokscale/git/trees/{PIN}?recursive=1", timeout=60
) as response:
    tree = json.load(response)
if tree.get("truncated"):
    raise SystemExit("Pinned Git tree inventory is truncated")
paths = [row["path"] for row in tree["tree"] if row["type"] == "blob" and (
    row["path"] == "LICENSE" or row["path"].endswith("/clients.rs") or
    (row["path"].endswith(".rs") and any(name in row["path"] for name in ("claude", "codex")))
)]
print(json.dumps({"pin": PIN, "paths": paths}, indent=2))
ROOT.mkdir(parents=True, exist_ok=True)
manifest = []
for path in paths:
    with urllib.request.urlopen(f"https://raw.githubusercontent.com/Javis603/tokscale/{PIN}/{path}", timeout=60) as response:
        data = response.read()
    destination = ROOT / path
    if destination.exists() and destination.read_bytes() != data:
        raise SystemExit(f"Refusing to overwrite differing parser evidence: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(data)
    manifest.append({"path": path, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)})
(ROOT / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(json.dumps({"evidence_directory": str(ROOT), "files": manifest}, indent=2))
