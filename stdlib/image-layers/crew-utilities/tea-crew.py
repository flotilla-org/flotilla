#!/usr/bin/env python3
"""Run tea with the crew's injected Forgejo login when one is available."""

import json
import os
import pathlib
import sys
import tempfile
from urllib.parse import urlparse


def configure_forgejo_login() -> None:
    server = os.environ.get("FORGEJO_SERVER_URL")
    username = os.environ.get("FORGEJO_USERNAME")
    token_file = os.environ.get("FORGEJO_TOKEN_FILE")
    if not all((server, username, token_file)):
        return

    server = server.rstrip("/")
    parsed = urlparse(server)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password:
        raise ValueError("FORGEJO_SERVER_URL must be an HTTPS server URL")

    token = pathlib.Path(token_file).read_text(encoding="utf-8").rstrip("\r\n")
    if not token:
        raise ValueError("FORGEJO_TOKEN_FILE is empty")

    config_home = pathlib.Path(os.environ.get("XDG_CONFIG_HOME") or pathlib.Path.home() / ".config")
    config_dir = config_home / "tea"
    config_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    config_path = config_dir / "config.yml"
    login = {
        "logins": [{
            "name": "flotilla-forgejo",
            "url": server,
            "token": token,
            "user": username,
            "default": True,
        }]
    }
    # JSON is valid YAML. Atomic replacement keeps concurrent tea calls from
    # observing a partial config and avoids putting the token in argv or logs.
    descriptor, temporary_path = tempfile.mkstemp(prefix=".config-", dir=config_dir)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as config:
            json.dump(login, config)
            config.write("\n")
        os.replace(temporary_path, config_path)
    finally:
        if os.path.exists(temporary_path):
            os.unlink(temporary_path)


if __name__ == "__main__":
    configure_forgejo_login()
    os.execv("/usr/local/bin/tea-real", ["tea", *sys.argv[1:]])
