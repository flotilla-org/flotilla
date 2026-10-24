#!/usr/bin/env python3
"""Record hooks/list using a stage_codex_crew launch's home and CLI overrides.

Usage: python3 crates/flotilla-agent/examples/record_codex_hooks.py /absolute/probe-root
The TUI itself must still be launched in cleat; this records discovery only.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import threading

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path, help="absolute staged probe root")
    parser.add_argument("--timeout", type=float, default=30, help="total app-server timeout in seconds (default: 30)")
    options = parser.parse_args()
    if not 0 < options.timeout < float("inf"):
        parser.error("--timeout must be finite and positive")
    root = options.root
    plan = json.loads((root / "launch.json").read_text())
    args = shlex.split(plan["command"])
    overrides = []
    for index, arg in enumerate(args):
        if arg == "-c":
            overrides.extend(args[index:index + 2])

    with (root / "app-server.stderr").open("w") as stderr:
        server = subprocess.Popen(
            [args[0], "app-server", *overrides],
            cwd=plan["cwd"],
            env={**os.environ, "CODEX_HOME": str(root / "home")},
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
            text=True,
        )

        timed_out = threading.Event()

        def expire():
            timed_out.set()
            server.kill()

        # Bound the whole protocol exchange, including blocking pipe reads.
        watchdog = threading.Timer(options.timeout, expire)
        watchdog.daemon = True
        watchdog.start()

        def send(message):
            server.stdin.write(json.dumps(message) + "\n")
            server.stdin.flush()

        def response(request_id):
            for line in server.stdout:
                message = json.loads(line)
                if message.get("id") == request_id:
                    if "error" in message:
                        raise RuntimeError(message["error"])
                    return message
            if timed_out.is_set():
                raise TimeoutError(f"Codex app-server timed out after {options.timeout} seconds")
            raise RuntimeError("Codex app-server exited before responding")

        try:
            send({
                "id": 1, "method": "initialize",
                "params": {
                    "clientInfo": {"name": "probe3823", "version": "1"},
                    "capabilities": {"experimentalApi": True},
                },
            })
            response(1)
            send({"method": "initialized"})
            send({"id": 2, "method": "hooks/list", "params": {"cwds": [plan["cwd"]]}})
            result = response(2)
            (root / "hooks-list.json").write_text(json.dumps(result, indent=2))
            print(json.dumps(result, indent=2))
        finally:
            watchdog.cancel()
            server.terminate()
            server.wait(timeout=10)


if __name__ == "__main__":
    main()
