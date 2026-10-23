#!/usr/bin/env python3
"""Record raw refusal results from the installed cleat using real attachments."""

import contextlib
import json
import os
from pathlib import Path
import pty
import subprocess
import threading
import time

DESTINATION = Path(__file__).resolve().parent


def cleat(*args):
    return subprocess.run(["cleat", *args], capture_output=True, text=True, check=False)


def inspect(session):
    result = cleat("inspect", session, "--json")
    result.check_returncode()
    return json.loads(result.stdout)


@contextlib.contextmanager
def session_with_attach(name, command):
    session = f"record-3713-{name}"
    cleat("launch", session, "--tag", "purpose=probe", "--cmd", command).check_returncode()
    master, slave = pty.openpty()
    client = subprocess.Popen(
        ["cleat", "attach", session, "--no-create", "--program-status", "off"],
        stdin=slave, stdout=slave, stderr=slave,
        env={**os.environ, "TERM": "xterm-256color"},
    )
    os.close(slave)

    def drain():
        # The initial full-screen render can block the attach before it reads input.
        try:
            while os.read(master, 65536):
                pass
        except OSError:
            pass

    threading.Thread(target=drain, daemon=True).start()
    try:
        deadline = time.monotonic() + 5
        while not inspect(session)["attachments"]:
            if client.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("attach did not connect")
            time.sleep(0.01)
        time.sleep(0.1)
        yield session, master
    finally:
        client.kill()
        client.wait()
        os.close(master)
        cleat("kill", session).check_returncode()


def submit(session, idle="10s"):
    return cleat("send", session, "--submit", "--controller-idle", idle, "turn")


def record(cause, result):
    assert result.returncode == 1 and f"controller.cause={cause}" in result.stderr, result
    captured = {"stdout": result.stdout, "stderr": result.stderr, "exit_code": result.returncode}
    (DESTINATION / f"{cause}.json").write_text(json.dumps(captured, indent=2) + "\n")
    print(f"recorded {cause}")


def main():
    cases = [
        ("fence-pending", b"x", "stty -echo; sleep 60"),
        ("fence-unverifiable", b"\x1b[13u", "stty -echo; sleep 60"),
        ("fence-settling", b"x", "cat"),
        ("quiet-time", b"x", "cat"),
    ]
    for cause, data, command in cases:
        with session_with_attach(cause, command) as (session, master):
            os.write(master, data)
            deadline = time.monotonic() + 5
            while inspect(session)["controller_input_generation"] == 0:
                if time.monotonic() >= deadline:
                    raise RuntimeError("controller input was not accepted")
                time.sleep(0.001)
            if cause == "quiet-time":
                time.sleep(0.2)
            record(cause, submit(session))
            if cause == "fence-unverifiable":
                (DESTINATION / "unverifiable-inspect.json").write_text(cleat("inspect", session, "--json").stdout)

    with session_with_attach("busy", "cat") as (session, _):
        for _ in range(50):
            first = subprocess.Popen(
                ["cleat", "send", session, "--submit", "--controller-idle", "0ms", "first"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            )
            time.sleep(0.015)
            result = submit(session, "0ms")
            first.communicate()
            if "controller.cause=busy" in result.stderr:
                record("busy", result)
                break
        else:
            raise RuntimeError("busy scheduling window was not observed; retry recording")
    (DESTINATION / "version.txt").write_text(cleat("--version").stdout)


if __name__ == "__main__":
    main()
