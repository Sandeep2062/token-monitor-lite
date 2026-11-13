"""Acquire only missing exact pinned reader support files on the SSH VM."""

import hashlib
import json
from pathlib import Path
import urllib.request

PIN = "d3df620b2cdd9502c70aececdbc6ece61fe26fc9"
root = Path.home() / ".local/state/token-monitor-lite/evidence" / f"tokscale-{PIN}"
with urllib.request.urlopen(
    f"https://api.github.com/repos/Javis603/tokscale/git/trees/{PIN}?recursive=1", timeout=60
) as response:
    tree = json.load(response)
if tree.get("truncated"):
    raise SystemExit("Pinned inventory truncated")
candidates = [row["path"] for row in tree["tree"] if row["type"] == "blob" and (
    row["path"] in (
        "crates/tokscale-core/src/sessions/mod.rs",
        "crates/tokscale-core/src/sessions/utils.rs",
        "crates/tokscale-core/src/scanner.rs",
        "crates/tokscale-core/src/sessionize.rs",
    ) or (row["path"].startswith("crates/tokscale-core/src/utils/") and row["path"].endswith(".rs"))
)]
manifest = []
for path in candidates:
    destination = root / path
    if destination.exists():
        data = destination.read_bytes()
    else:
        with urllib.request.urlopen(f"https://raw.githubusercontent.com/Javis603/tokscale/{PIN}/{path}", timeout=60) as response:
            data = response.read()
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open("xb") as output:
            output.write(data)
    manifest.append({"path": path, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
with (root / "reader-support-utils-manifest.json").open("x") as output:
    json.dump(manifest, output, indent=2)
    output.write("\n")
print(json.dumps(manifest, indent=2))
