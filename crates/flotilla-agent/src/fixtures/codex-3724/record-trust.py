"""Refresh the first-party trust fixture without computing any hashes ourselves."""

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[2]
version = subprocess.check_output(["codex", "--version"], text=True).strip()
if version != "codex-cli 0.160.0":
    raise RuntimeError(f"Expected codex-cli 0.160.0, got {version}")
subscriptions = (root / "hooks.rs").read_text().split("const CODEX_HOOK_SUBSCRIPTIONS:")[1].split("];")[0]
events = re.findall(r'\("([^"]+)", "([^"]+)",', subscriptions)
assert events, "No subscriptions found"
args = ["codex", "app-server", "-c", "features.hooks=true", "-c", "notify=[]"]
for native, event in events:
    args += ["-c", f'hooks.{native}=[{{hooks=[{{type="command",command="flotilla hook codex {event}"}}]}}]']


def record(home, states=None):
    command = args + (["-c", "hooks.state=" + states] if states else [])
    process = subprocess.Popen(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env={**os.environ, "CODEX_HOME": home},
        cwd="/tmp",
    )

    def send(message):
        process.stdin.write(json.dumps(message) + "\n")
        process.stdin.flush()

    def response(request_id):
        while True:
            line = process.stdout.readline()
            if not line:
                raise RuntimeError("App server exited before responding")
            message = json.loads(line)
            if message.get("id") == request_id:
                if "error" in message:
                    raise RuntimeError(message["error"])
                return message["result"]

    try:
        send({
            "id": 0,
            "method": "initialize",
            "params": {
                "clientInfo": {"name": "fixture_recorder", "version": "1"},
                "capabilities": {"experimentalApi": True},
            },
        })
        response(0)
        send({"method": "initialized"})
        send({"id": 1, "method": "hooks/list", "params": {"cwds": ["/tmp"]}})
        return response(1)
    finally:
        process.terminate()
        process.wait(timeout=10)


with tempfile.TemporaryDirectory(prefix="codex-hooks-") as home:
    hooks = record(home)["data"][0]["hooks"]
    states = "{" + ",".join(
        json.dumps(hook["key"]) + "={enabled=true,trusted_hash=" + json.dumps(hook["currentHash"]) + "}"
        for hook in hooks
    ) + "}"
    result = record(home, states)
    hooks = result["data"][0]["hooks"]
    assert len(hooks) == len(events)
    assert all(hook["trustStatus"] == "trusted" for hook in hooks)
    (root / "fixtures/codex-hooks-list-0.160.0.json").write_text(json.dumps(result, indent=2) + "\n")
    print([(hook["eventName"], hook["timeoutSec"], hook["trustStatus"]) for hook in hooks])
