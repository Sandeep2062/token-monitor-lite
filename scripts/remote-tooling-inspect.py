"""Read-only prerequisite metadata; execute through SSH, never locally."""

import json
import pathlib
import urllib.request


def get_json(url):
    with urllib.request.urlopen(url, timeout=30) as response:
        return json.load(response)


releases = get_json("https://nodejs.org/dist/index.json")
release = next(row for row in releases if row["version"].startswith("v24.") and row["lts"])
package = get_json("https://registry.npmjs.org/rea-agents/6.7.0")
result = {
    "node": {key: release[key] for key in ("version", "date", "lts", "npm")},
    "rea": {key: package.get(key) for key in ("name", "version", "engines", "bin", "dist")},
    "tools_directory_exists": pathlib.Path.home().joinpath(
        ".local/share/token-monitor-lite-tools"
    ).exists(),
}
print(json.dumps(result, indent=2))
