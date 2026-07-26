#!/usr/bin/env python3
"""Capture the manifest connector's patch stream to JSONL, headless.

Serves a unix socket, runs `flotilla pm connect --wheelhouse-socket` against it
for a few seconds, writes one JSON patch per line. The other half of the local
presentation harness (render side: `cargo run -p andamento-controller` in the
andamento repo). Born from the andamento#37 postmortem: never debug the
composed pipeline through zellij relaunches again."""
import json, os, socket, subprocess, sys, tempfile, threading, time

flotilla_bin = os.environ.get("FLOTILLA_BIN", os.path.expanduser("~/dev/flotilla/target/debug/flotilla"))
out_path = sys.argv[1] if len(sys.argv) > 1 else "pm-patches.jsonl"
seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 8.0

sock_path = os.path.join(tempfile.mkdtemp(), "wh.sock")
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(sock_path)
server.listen(64)
server.settimeout(0.5)
lines = []
stop = threading.Event()

def serve():
    while not stop.is_set():
        try:
            conn, _ = server.accept()
        except socket.timeout:
            continue
        with conn:
            buf = b""
            conn.settimeout(1.0)
            try:
                while True:
                    chunk = conn.recv(65536)
                    if not chunk:
                        break
                    buf += chunk
            except socket.timeout:
                pass
            for line in buf.decode(errors="replace").splitlines():
                if line.strip():
                    lines.append(line)

t = threading.Thread(target=serve, daemon=True)
t.start()
proc = subprocess.Popen([flotilla_bin, "pm", "connect", "--flotilla-bin", flotilla_bin, "--wheelhouse-socket", sock_path],
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(seconds)
proc.terminate()
stop.set(); t.join(timeout=2)
seen, out = set(), []
for l in lines:
    try:
        key = json.dumps(json.loads(l), sort_keys=True)
    except Exception:
        key = l
    if key not in seen:
        seen.add(key); out.append(l)
with open(out_path, "w") as f:
    f.write("\n".join(out) + ("\n" if out else ""))
print(f"captured {len(lines)} patches ({len(out)} unique) -> {out_path}")
