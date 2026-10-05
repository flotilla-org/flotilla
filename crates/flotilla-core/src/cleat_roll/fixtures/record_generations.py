"""Record private generation turnover with the installed cleat CLI."""

import http.server
import json
import os
import pathlib
import shutil
import signal
import socketserver
import subprocess
import tempfile
import threading

FIXTURES = pathlib.Path(__file__).resolve().parent
CLEAT = shutil.which("cleat")


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


class Handler(http.server.BaseHTTPRequestHandler):
    def respond(self, body):
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/":
            self.respond({
                "generation": 26,
                "drain_state": self.server.state,
                "build": self.server.build,
                "session_count": 3,
            })
        elif self.path == "/sessions":
            # The drain contract consumes only array length (IgnoredAny).
            self.respond({"sessions": [{}, {}, {}]})
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != "/drain":
            self.send_error(404)
            return
        self.server.state = "draining"
        self.respond({"drain_state": "draining", "session_count": 3})

    def log_message(self, *args):
        pass


def main():
    if CLEAT is None:
        raise RuntimeError("cleat must be installed to record CLI evidence")
    root = pathlib.Path(tempfile.mkdtemp(prefix="cleat-record-", dir="/tmp"))
    server = None
    try:
        installed = json.loads(subprocess.check_output([CLEAT, "version", "--json"]))["client"]
        old = json.loads(json.loads((FIXTURES / "success.json").read_text())["stdout"])["old"]["build"]
        path = root / "default@26"
        (path / "sessions").mkdir(parents=True)
        server = Server(str(path / "socket"), Handler)
        server.state = "serving"
        server.build = old
        threading.Thread(target=server.serve_forever, daemon=True).start()
        (root / "default").symlink_to("default@26")
        env = {**os.environ, "CLEAT_RUNTIME_DIR": str(root)}

        def capture(name, verb):
            args = [CLEAT, "--runtime-root", str(root), "--server", name, *verb]
            result = subprocess.run(args, env=env, text=True, capture_output=True, timeout=30)
            return {
                "stdout": result.stdout.replace(str(root), "{root}"),
                "stderr": result.stderr.replace(str(root), "{root}"),
                "success": result.returncode == 0,
            }

        drain = capture("default", ["server", "drain", "--json"])
        if not drain["success"]:
            raise RuntimeError(f"private drain failed: {drain}")
        records = {
            "drain": drain,
            "installed": installed,
            "current": capture("default", ["version", "--daemon", "--json"]),
            "listing": capture("default", ["daemons", "--json"]),
            "empty": capture("missing", ["version", "--daemon", "--json"]),
        }
        (root / "default").unlink()
        (root / "default").symlink_to("default@26")
        records["stale"] = capture("default", ["version", "--daemon", "--json"])
        (FIXTURES / "generations.json").write_text(json.dumps(records, indent=2) + "\n")
    finally:
        if server is not None:
            server.shutdown()
            server.server_close()
        # Only processes in this private root are eligible for cleanup.
        for pidpath in root.glob("*/daemon.pid"):
            try:
                os.kill(int(pidpath.read_text().strip()), signal.SIGTERM)
            except (ProcessLookupError, ValueError):
                pass
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
