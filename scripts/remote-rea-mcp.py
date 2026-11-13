"""Drive the approved remote REA MCP server. Run on pixvault-vm only."""

import json
import os
from pathlib import Path
import subprocess
import hashlib
import argparse

parser = argparse.ArgumentParser()
parser.add_argument("--batch", choices=("foundation", "collection"), default="foundation")
args = parser.parse_args()

ROOT = Path.home() / ".local/state/token-monitor-lite/evidence/rea-6.7.0"
ROOT.mkdir(parents=True, exist_ok=True)
TOOLS = Path.home() / ".local/share/token-monitor-lite-tools"
environment = dict(os.environ, PATH=f"{TOOLS / 'node-v24.22.2-linux-x64/bin'}:{os.environ['PATH']}")
launcher = TOOLS / "rea-6.7.0/node_modules/.bin/rea"
sequence = 0


def request(process, method, params):
    global sequence
    sequence += 1
    message = {"jsonrpc": "2.0", "id": sequence, "method": method, "params": params}
    process.stdin.write(json.dumps(message) + "\n")
    process.stdin.flush()
    for line in process.stdout:
        response = json.loads(line)
        if response.get("id") == sequence:
            if "error" in response:
                raise RuntimeError(response["error"])
            return response["result"]
        with (ROOT / "notifications.jsonl").open("a") as log:
            log.write(line)
    raise RuntimeError("REA MCP closed before responding")


with (ROOT / f"{args.batch}-server.stderr.log").open("x") as errors:
    process = subprocess.Popen(
        [str(launcher), "--mcp"], env=environment,
        cwd=Path.home() / "Github/token-monitor-lite/token-monitor",
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, text=True,
    )
    try:
        initialization = request(process, "initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "token-monitor-lite-ssh-evidence", "version": "0.1.0"},
        })
        process.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        process.stdin.flush()
        inventory = request(process, "tools/list", {})
        (ROOT / f"{args.batch}-initialize.json").write_text(json.dumps(initialization, indent=2) + "\n")
        (ROOT / f"{args.batch}-tools.json").write_text(json.dumps(inventory, indent=2) + "\n")
        names = {tool["name"] for tool in inventory["tools"]}
        required = {"analyze_javascript_application", "export_evidence_bundle"}
        if not required.issubset(names):
            raise RuntimeError(f"Missing remote MCP tools: {required - names}")
        output = ROOT / f"{args.batch}-analysis.json"
        bundle = ROOT / f"{args.batch}-bundle.json"
        if output.exists() or bundle.exists():
            raise RuntimeError("Batch evidence already exists; inspect it instead of reanalysis")
        selections = {
            "foundation": ["package.json", "src/shared/clientCatalog.js",
                           "src/shared/limits/providers.js", "src/shared/usage.js"],
            "collection": ["src/shared/collector.js", "src/shared/clientSources.js",
                           "src/shared/clientSourceRegistration.js", "src/shared/clientHealth.js",
                           "src/shared/history.js", "src/shared/usage/sessionUsageArchive.js",
                           "src/shared/usage/sessionUsageArchiveStore.js"],
        }
        target = Path.home() / "Github/token-monitor-lite/token-monitor"
        missing = [path for path in selections[args.batch] if not (target / path).is_file()]
        if missing:
            raise RuntimeError(f"Source selection requires verified paths: {missing}")
        parameters = {
            "input_path": str(Path.home() / "Github/token-monitor-lite/token-monitor"),
            "format": "directory", "detail": "summary",
            "source_paths": selections[args.batch],
            "max_heap_mb": 1024, "analysis_timeout_ms": 60000,
            "max_semantic_nodes": 100000,
        }
        print(json.dumps({"server": initialization.get("serverInfo"), "parameters": parameters}), flush=True)
        analysis = request(process, "tools/call", {
            "name": "analyze_javascript_application", "arguments": parameters,
        })
        output.write_text(json.dumps(analysis, indent=2) + "\n")
        if analysis.get("isError"):
            raise RuntimeError(f"REA analysis failed; retained response: {output}")
        exported = request(process, "tools/call", {
            "name": "export_evidence_bundle", "arguments": {"path": str(bundle)},
        })
        (ROOT / f"{args.batch}-export.json").write_text(json.dumps(exported, indent=2) + "\n")
        if exported.get("isError"):
            raise RuntimeError("REA export failed; analysis response retained")
        print(json.dumps({"analysis": analysis.get("structuredContent", analysis),
                          "export": exported.get("structuredContent", exported),
                          "bundle_sha256": hashlib.file_digest(bundle.open("rb"), "sha256").hexdigest()}, indent=2))
    finally:
        process.stdin.close()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.terminate()
            process.wait(timeout=10)
