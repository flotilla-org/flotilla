#!/usr/bin/env python3
"""Check the crew tea launcher without requiring a live Forgejo server."""

import importlib.util
import json
import os
import pathlib
import stat
import tempfile
import unittest
from unittest.mock import patch


LAUNCHER = pathlib.Path(__file__).resolve().with_name("tea-crew.py")
SPEC = importlib.util.spec_from_file_location("tea_crew", LAUNCHER)
assert SPEC and SPEC.loader
tea_crew = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(tea_crew)


class TeaAuthTests(unittest.TestCase):
    def test_no_forgejo_credentials_leave_tea_unconfigured(self):
        with tempfile.TemporaryDirectory() as home:
            with patch.dict(os.environ, {"HOME": home, "XDG_CONFIG_HOME": home}, clear=True):
                tea_crew.configure_forgejo_login()
            self.assertFalse((pathlib.Path(home) / "tea" / "config.yml").exists())

    def test_login_tracks_token_file_without_exposing_token_in_arguments(self):
        with tempfile.TemporaryDirectory() as home:
            token_file = pathlib.Path(home) / "token"
            token_file.write_text("first-token\n", encoding="utf-8")
            environment = {
                "HOME": home,
                "XDG_CONFIG_HOME": home,
                "FORGEJO_SERVER_URL": "https://forgejo.lab.flotilla.work/",
                "FORGEJO_USERNAME": "crew-user",
                "FORGEJO_TOKEN_FILE": str(token_file),
            }
            with patch.dict(os.environ, environment, clear=True):
                tea_crew.configure_forgejo_login()
                config_path = pathlib.Path(home) / "tea" / "config.yml"
                self.assertEqual(stat.S_IMODE(config_path.stat().st_mode), 0o600)
                self.assertEqual(json.loads(config_path.read_text(encoding="utf-8")), {
                    "logins": [{
                        "name": "flotilla-forgejo",
                        "url": "https://forgejo.lab.flotilla.work",
                        "token": "first-token",
                        "user": "crew-user",
                        "default": True,
                    }]
                })

                token_file.write_text("rotated-token\n", encoding="utf-8")
                tea_crew.configure_forgejo_login()
                self.assertEqual(json.loads(config_path.read_text(encoding="utf-8"))["logins"][0]["token"], "rotated-token")


if __name__ == "__main__":
    unittest.main()
