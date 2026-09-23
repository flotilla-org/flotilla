#!/usr/bin/env python3

import base64
import json
import os
import stat
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "codex-token-refresh"


def make_jwt(claims):
    header = base64.urlsafe_b64encode(json.dumps({"alg": "none"}).encode()).rstrip(b"=").decode()
    payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).rstrip(b"=").decode()
    return f"{header}.{payload}.sig"


class TokenServer:
    """A minimal, scripted stand-in for codex's OAuth token endpoint."""

    def __init__(self):
        self.responses = []
        self.received = []
        handler = self._make_handler()
        self._httpd = HTTPServer(("127.0.0.1", 0), handler)
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)
        self._thread.start()

    def _make_handler(server_self):
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length).decode("utf-8"))
                server_self.received.append(body)
                status, payload = server_self.responses.pop(0)
                encoded = json.dumps(payload).encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        return Handler

    @property
    def url(self):
        host, port = self._httpd.server_address
        return f"http://{host}:{port}/oauth/token"

    def queue(self, status, payload):
        self.responses.append((status, payload))

    def close(self):
        self._httpd.shutdown()
        self._thread.join(timeout=5)
        self._httpd.server_close()


class CodexTokenRefreshTest(unittest.TestCase):
    def setUp(self):
        self.server = TokenServer()
        self.addCleanup(self.server.close)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.codex_home = Path(self.temp.name) / "codex-home"

    def write_auth(self, codex_home, refresh_token="rt-original", extra=None):
        codex_home.mkdir(parents=True, exist_ok=True)
        auth = {
            "OPENAI_API_KEY": None,
            "tokens": {
                "id_token": make_jwt({"email": "crew@example.com"}),
                "access_token": make_jwt({"exp": 1_700_000_000}),
                "refresh_token": refresh_token,
                "account_id": "acct_123",
            },
            "last_refresh": "2026-01-01T00:00:00Z",
        }
        if extra:
            auth.update(extra)
        path = codex_home / "auth.json"
        path.write_text(json.dumps(auth, indent=2))
        path.chmod(0o600)
        return path

    def run_script(self, codex_home):
        env = dict(os.environ)
        env["CODEX_REFRESH_TOKEN_URL_OVERRIDE"] = self.server.url
        env.pop("CODEX_APP_SERVER_LOGIN_CLIENT_ID", None)
        return subprocess.run(
            [str(SCRIPT), "--codex-home", str(codex_home)],
            text=True,
            capture_output=True,
            env=env,
            check=False,
        )

    def test_successful_refresh_rotates_tokens_and_writes_atomically(self):
        codex_home = self.codex_home
        auth_path = self.write_auth(codex_home, refresh_token="rt-original")
        new_exp = 1_800_000_000
        self.server.queue(
            200,
            {
                "id_token": make_jwt({"email": "crew@example.com"}),
                "access_token": make_jwt({"exp": new_exp}),
                "refresh_token": "rt-rotated",
            },
        )

        result = self.run_script(codex_home)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.server.received, [{"grant_type": "refresh_token", "client_id": "app_EMoamEEZ73f0CkXaXp7hrann", "refresh_token": "rt-original"}])

        updated = json.loads(auth_path.read_text())
        self.assertEqual(updated["tokens"]["refresh_token"], "rt-rotated")
        self.assertNotEqual(updated["last_refresh"], "2026-01-01T00:00:00Z")
        self.assertEqual(updated["tokens"]["account_id"], "acct_123")  # untouched fields survive
        self.assertEqual(updated["OPENAI_API_KEY"], None)

        mode = stat.S_IMODE(auth_path.stat().st_mode)
        self.assertEqual(mode, 0o600)

        leftovers = [entry for entry in codex_home.iterdir() if entry.name != "auth.json"]
        self.assertEqual(leftovers, [], "no temp files should remain after a successful run")

    def test_run_is_safe_to_repeat_on_a_timer(self):
        codex_home = self.codex_home
        self.write_auth(codex_home, refresh_token="rt-1")
        self.server.queue(200, {"access_token": make_jwt({"exp": 1}), "refresh_token": "rt-2"})
        self.server.queue(200, {"access_token": make_jwt({"exp": 2}), "refresh_token": "rt-3"})

        first = self.run_script(codex_home)
        second = self.run_script(codex_home)

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual([body["refresh_token"] for body in self.server.received], ["rt-1", "rt-2"])

    def test_dead_refresh_token_fails_clearly_with_named_reason(self):
        codex_home = self.codex_home
        auth_path = self.write_auth(codex_home, refresh_token="rt-dead")
        before = auth_path.read_text()
        self.server.queue(400, {"error": "refresh_token_expired"})

        result = self.run_script(codex_home)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refresh_token_expired", result.stderr)
        self.assertEqual(auth_path.read_text(), before, "a failed refresh must not touch the existing auth.json")

    def test_reused_refresh_token_is_classified_as_permanent(self):
        codex_home = self.codex_home
        self.write_auth(codex_home, refresh_token="rt-dead")
        self.server.queue(400, {"error": "refresh_token_reused"})

        result = self.run_script(codex_home)

        self.assertEqual(result.returncode, 1)
        self.assertIn("refresh_token_reused", result.stderr)

    def test_invalidated_refresh_token_is_classified_as_permanent(self):
        codex_home = self.codex_home
        self.write_auth(codex_home, refresh_token="rt-dead")
        self.server.queue(400, {"error": "refresh_token_invalidated"})

        result = self.run_script(codex_home)

        self.assertEqual(result.returncode, 1)
        self.assertIn("refresh_token_invalidated", result.stderr)

    def test_ambiguous_400_without_a_recognized_error_code_is_transient(self):
        # codex-rs (`classify_refresh_token_failure` in manager.rs) only
        # escalates a 400 to a permanent, alert-worthy failure when it
        # recognizes the `error` code (or it's exactly `invalid_grant`). A
        # well-formed 400 body that carries no (or an unrecognized) `error`
        # field is treated the same as any other unrecognized 400: transient,
        # so a scheduled retry — not a false dead-token alert — happens next.
        codex_home = self.codex_home
        self.write_auth(codex_home, refresh_token="rt-ambiguous")
        self.server.queue(400, {})

        result = self.run_script(codex_home)

        self.assertEqual(result.returncode, 2)
        self.assertIn("refresh_rejected", result.stderr)

    def test_invalid_grant_bad_request_is_classified_as_permanent(self):
        codex_home = self.codex_home
        self.write_auth(codex_home, refresh_token="rt-dead")
        self.server.queue(400, {"error": "invalid_grant", "error_description": "Token is malformed."})

        result = self.run_script(codex_home)

        self.assertEqual(result.returncode, 1)
        self.assertIn("invalid_grant", result.stderr)

    def test_server_error_is_classified_as_transient(self):
        codex_home = self.codex_home
        self.write_auth(codex_home, refresh_token="rt-flaky")
        self.server.queue(503, {"error": "server_error"})

        result = self.run_script(codex_home)

        self.assertEqual(result.returncode, 2)
        self.assertIn("refresh_rejected", result.stderr)

    def test_missing_auth_file_fails_clearly(self):
        codex_home = self.codex_home

        result = self.run_script(codex_home)

        self.assertEqual(result.returncode, 3)
        self.assertIn("auth_file_missing", result.stderr)

    def test_no_secrets_in_argv_or_output(self):
        codex_home = self.codex_home
        self.write_auth(codex_home, refresh_token="rt-super-secret-value")
        self.server.queue(200, {"access_token": make_jwt({"exp": 1}), "refresh_token": "rt-also-secret"})

        result = self.run_script(codex_home)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("rt-super-secret-value", result.stdout)
        self.assertNotIn("rt-super-secret-value", result.stderr)
        self.assertNotIn("rt-also-secret", result.stdout)
        self.assertNotIn("rt-also-secret", result.stderr)
        # No command line ever carries the refresh token: only --codex-home is passed.
        self.assertEqual(result.args[:3], [str(SCRIPT), "--codex-home", str(codex_home)])


if __name__ == "__main__":
    unittest.main()
