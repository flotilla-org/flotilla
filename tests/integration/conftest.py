"""Shared fixtures and helpers for flotilla integration tests."""

import json
import subprocess
import time
from pathlib import Path

import pytest

COMPOSE_DIR = Path(__file__).parent
COMPOSE_FILE = str(COMPOSE_DIR / "docker-compose.yml")


def docker_exec(
    service: str, cmd: str, timeout: int = 30
) -> subprocess.CompletedProcess:
    """Run a command inside a container via docker compose exec."""
    return subprocess.run(
        [
            "docker", "compose", "-f", COMPOSE_FILE,
            "exec", "-T", "-u", "flotilla", service, "bash", "-c", cmd,
        ],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def flotilla_json(service: str, args: str, timeout: int = 30) -> dict | list:
    """Run a flotilla CLI command with --json and return parsed output."""
    result = docker_exec(service, f"flotilla {args} --json", timeout=timeout)
    assert result.returncode == 0, (
        f"flotilla {args} failed (rc={result.returncode}):\n"
        f"stdout: {result.stdout}\nstderr: {result.stderr}"
    )
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as e:
        raise AssertionError(
            f"flotilla {args} returned non-JSON output:\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        ) from e


def wait_for(
    predicate, description: str, timeout: int = 60, interval: float = 2.0
):
    """Poll until predicate returns True or timeout.

    AssertionError is re-raised immediately (hard failure, not transient).
    Other exceptions are retried until timeout.
    """
    deadline = time.monotonic() + timeout
    last_err = None
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except AssertionError:
            raise
        except Exception as e:
            last_err = e
        time.sleep(interval)
    msg = f"Timed out waiting for: {description}"
    if last_err:
        msg += f" (last error: {last_err})"
    raise TimeoutError(msg)


@pytest.fixture(scope="session")
def topology():
    """Spin up 2-node topology, wait for peering, yield, tear down."""
    subprocess.run(
        ["docker", "compose", "-f", COMPOSE_FILE, "up", "-d", "--build",
         "--force-recreate"],
        check=True,
        timeout=600,
    )
    try:
        # Wait for SSH readiness between nodes
        wait_for(
            lambda: docker_exec(
                "node-a",
                "ssh -o StrictHostKeyChecking=no -o BatchMode=yes node-b true",
            ).returncode == 0,
            "SSH from node-a to node-b",
        )

        # Create a git repo in each container
        for node in ("node-a", "node-b"):
            result = docker_exec(
                node,
                "git config --global user.email test@test.com && "
                "git config --global user.name test && "
                "git init /home/flotilla/repo && "
                "cd /home/flotilla/repo && "
                "git commit --allow-empty -m init",
            )
            assert result.returncode == 0, (
                f"git init failed on {node}: {result.stderr}"
            )

        # Write flotilla config: node-a is leader with node-b as peer
        result = docker_exec("node-a", "\n".join([
            "mkdir -p ~/.config/flotilla",
            "cat > ~/.config/flotilla/hosts.toml << 'TOML'",
            "[hosts.node-b]",
            'hostname = "node-b"',
            'expected_host_name = "node-b"',
            'daemon_socket = "/home/flotilla/.config/flotilla/flotilla.sock"',
            "TOML",
        ]))
        assert result.returncode == 0, (
            f"hosts.toml write failed: {result.stderr}"
        )

        # Write flotilla config: node-b is follower
        result = docker_exec("node-b", "\n".join([
            "mkdir -p ~/.config/flotilla",
            "cat > ~/.config/flotilla/daemon.toml << 'TOML'",
            "follower = true",
            "TOML",
        ]))
        assert result.returncode == 0, (
            f"daemon.toml write failed: {result.stderr}"
        )

        # Start daemons (backgrounded, reparented to container PID 1)
        for node in ("node-a", "node-b"):
            docker_exec(
                node,
                "nohup flotilla daemon >/dev/null 2>~/.config/flotilla/daemon-panic.log &",
            )

        # Wait for daemon readiness on each node
        for node in ("node-a", "node-b"):
            wait_for(
                lambda n=node: docker_exec(
                    n, "flotilla status --json"
                ).returncode == 0,
                f"daemon ready on {node}",
                timeout=30,
            )

        # Add repos via CLI
        for node in ("node-a", "node-b"):
            result = docker_exec(
                node, "flotilla repo add /home/flotilla/repo"
            )
            assert result.returncode == 0, (
                f"repo add failed on {node}: {result.stderr}"
            )

        # Wait for peering: node-a sees node-b as Connected
        def peers_connected():
            result = flotilla_json("node-a", "host list")
            return any(
                h["host"] == "node-b"
                and h["connection_status"] == "Connected"
                for h in result.get("hosts", [])
            )

        wait_for(peers_connected, "node-b connected to node-a", timeout=90)

        yield {"node-a": "node-a", "node-b": "node-b"}

    finally:
        # Print daemon logs for debugging (daemon writes to state_dir/daemon.log)
        for node in ("node-a", "node-b"):
            result = docker_exec(node, "cat ~/.local/state/flotilla/daemon.log")
            if result.stdout:
                print(f"\n=== {node} daemon log ===\n{result.stdout}")
            panic_result = docker_exec(node, "cat ~/.config/flotilla/daemon-panic.log")
            if panic_result.stdout:
                print(f"\n=== {node} daemon panic log ===\n{panic_result.stdout}")

        result = subprocess.run(
            [
                "docker", "compose", "-f", COMPOSE_FILE,
                "down", "-v", "--remove-orphans",
            ],
            capture_output=True,
            text=True,
            timeout=60,
        )
        if result.returncode != 0:
            print(f"\n=== teardown failed (rc={result.returncode}) ===")
            print(result.stderr)
