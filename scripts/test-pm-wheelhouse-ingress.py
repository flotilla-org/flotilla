#!/usr/bin/env python3
"""Run #2523 acceptance through a disposable current Wheelhouse ingress + decoder.

Build Wheelhouse's tools/prepare-andamento-build.py consumer first. Arguments:
  WHEELHOUSE_CHECKOUT ANDAMENTO_CHECKOUT LIBRARY_DIRECTORY LOG_DIRECTORY
Uses the real wheelhouse_ingress and andamento_ffi libraries, never a fake HTTP sink.
"""
import argparse
import ctypes as C
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
for name in ("wheelhouse", "andamento", "libraries", "logs"):
    parser.add_argument(name, type=Path)
args = parser.parse_args()
wheelhouse, andamento, libraries, logs = (getattr(args, name).resolve() for name in ("wheelhouse", "andamento", "libraries", "logs"))
logs.mkdir(parents=True, exist_ok=True)
flotilla = Path(__file__).resolve().parents[1]
with (logs / "revisions.txt").open("w") as revisions:
    for name, repository in (("flotilla", flotilla), ("wheelhouse", wheelhouse), ("andamento", andamento)):
        revision = subprocess.check_output(["git", "-C", str(repository), "rev-parse", "HEAD"], text=True).strip()
        status = subprocess.check_output(["git", "-C", str(repository), "status", "--porcelain"], text=True)
        revisions.write(f"{name}: {revision}\n{status}")
lib = C.CDLL(str(libraries / "libwheelhouse_ingress.so"))
core = C.CDLL(str(libraries / "libandamento_ffi.so"))

class Text(C.Structure):
    _fields_ = [("data", C.c_void_p), ("len", C.c_size_t)]

WAKE = C.CFUNCTYPE(None)
APPLY = C.CFUNCTYPE(C.c_uint32, C.c_void_p, C.c_void_p, C.c_size_t)
core.andamento_create.argtypes = [C.c_char_p, C.c_size_t, C.c_void_p]
core.andamento_create.restype = C.c_void_p
core.andamento_apply_patch_json.argtypes = [C.c_void_p, C.c_uint64, Text, C.c_void_p]
core.andamento_apply_patch_json.restype = C.c_uint32
core.andamento_destroy.argtypes = [C.c_void_p]
lib.wheelhouse_ingress_start.argtypes = [C.c_char_p, C.c_size_t, WAKE, C.c_void_p, C.c_size_t]
lib.wheelhouse_ingress_start.restype = C.c_void_p
lib.wheelhouse_ingress_poll.argtypes = [C.c_void_p, APPLY, C.c_void_p]
lib.wheelhouse_ingress_stop.argtypes = [C.c_void_p]
config = (wheelhouse / "data/sidebar/fixture.kdl").read_bytes()
handle = core.andamento_create(config, len(config), None)
assert handle, "real sidebar config must decode"
patches = []
rejected = []
started = time.monotonic()
with (logs / "patches.jsonl").open("w") as captured:
    def apply(_, data, size):
        raw = C.string_at(data, size)
        result = core.andamento_apply_patch_json(handle, int((time.monotonic() - started) * 1000), Text(data, size), None)
        captured.write(raw.decode() + "\n")
        captured.flush()
        patches.append(json.loads(raw))
        if result != 1:
            rejected.append(raw)
        return result
    callback = APPLY(apply)
    wake = WAKE(lambda: None)
    # UDS paths must fit SUN_LEN, independently of the build/test TMPDIR.
    with tempfile.TemporaryDirectory(prefix="pm2523-", dir="/tmp") as tmp:
        socket = str(Path(tmp) / "facts.sock").encode()
        error = C.create_string_buffer(512)
        server = lib.wheelhouse_ingress_start(socket, len(socket), wake, error, len(error))
        assert server, error.value.decode()
        process = None
        try:
            env = dict(os.environ, FLOTILLA_TEST_WHEELHOUSE_SOCKET=socket.decode())
            with (logs / "connector.log").open("w") as output:
                process = subprocess.Popen(["cargo", "test", "-p", "flotilla-tui", "--test", "pm_connector_e2e", "--locked", "connector_real_wheelhouse_ingress", "--", "--ignored", "--nocapture"], env=env, cwd=flotilla, stdout=output, stderr=subprocess.STDOUT)
                deadline = time.monotonic() + 600
                while process.poll() is None:
                    if time.monotonic() > deadline:
                        raise TimeoutError("acceptance test exceeded ten minutes")
                    lib.wheelhouse_ingress_poll(server, callback, None)
                    time.sleep(.005)
            assert process.returncode == 0, (logs / "connector.log").read_text()
        finally:
            if process is not None and process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
            lib.wheelhouse_ingress_stop(server)
            core.andamento_destroy(handle)
assert not rejected, f"real decoder rejected {len(rejected)} patches"
keys = {key for patch in patches for key in patch.get("set", {})}
for key in ["flotilla.subject.produces", "flotilla.project", "flotilla.role.current_attempt", "flotilla.role.attempts", "action.primary.recipe", "flotilla.change_request.title"]:
    assert key in keys, f"missing acceptance fact {key}"
log = (logs / "connector.log").read_text()
assert "include-replicas watches do not support cursor resume" not in log
# One stable subscription, plus the one deliberate reconnect in the scenario.
assert log.count("pm connector subscribed; publishing catalog") == 2, log
print(f"PASS: {len(patches)} patches accepted by real Wheelhouse ingress and Andamento decoder; two intentional subscriptions")
