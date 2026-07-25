"""Shared fixtures and helpers for flotilla integration tests."""

import json
import subprocess
import time
from pathlib import Path

import pytest

COMPOSE_DIR = Path(__file__).parent
COMPOSE_FILE = str(COMPOSE_DIR / "docker-compose.yml")
DAEMON_LOG = "~/.local/state/flotilla/daemon.log"


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


def compose(*args: str, timeout: int = 60) -> subprocess.CompletedProcess:
    """Run docker compose against the integration topology."""
    return subprocess.run(
        ["docker", "compose", "-f", COMPOSE_FILE, *args],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def flotilla_json(service: str, args: str, timeout: int = 30) -> dict | list:
    """Run a flotilla CLI command with --json and return parsed output."""
    result = docker_exec(service, f"flotilla --json {args}", timeout=timeout)
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


def start_daemon(service: str):
    """Start flotillad directly and record its PID for deterministic restarts."""
    result = docker_exec(
        service,
        "mkdir -p ~/.config/flotilla ~/.local/state/flotilla; "
        "tmux_env=$(tmux display-message -p "
        "'#{socket_path},#{pid},#{window_index}'); "
        "nohup env TMUX=\"$tmux_env\" "
        "RUST_LOG=flotilla_daemon=debug flotillad --timeout 0 "
        ">~/.config/flotilla/daemon-stdio.log 2>&1 & "
        "echo $! > ~/.config/flotilla/flotillad.pid",
    )
    assert result.returncode == 0, (
        f"flotillad start failed on {service}:\n"
        f"stdout: {result.stdout}\nstderr: {result.stderr}"
    )


def stop_daemon(service: str):
    """Stop the recorded flotillad process, if it is still running."""
    result = docker_exec(
        service,
        "if test -f ~/.config/flotilla/flotillad.pid; then "
        "pid=$(cat ~/.config/flotilla/flotillad.pid); "
        "kill \"$pid\" 2>/dev/null || true; "
        "for attempt in $(seq 1 50); do "
        "state=$(ps -o stat= -p \"$pid\" 2>/dev/null || true); "
        "case \"$state\" in ''|Z*) exit 0 ;; esac; sleep 0.1; "
        "done; exit 1; "
        "fi",
    )
    assert result.returncode == 0, (
        f"flotillad stop failed on {service}:\n"
        f"stdout: {result.stdout}\nstderr: {result.stderr}"
    )


def daemon_log(service: str) -> str:
    """Read a node's structured daemon log."""
    result = docker_exec(service, f"cat {DAEMON_LOG}")
    return result.stdout if result.returncode == 0 else ""


@pytest.fixture(scope="module")
def topology():
    """Spin up 2-node topology, wait for peering, yield, tear down."""
    result = compose(
        "up", "-d", "--build", "--force-recreate", timeout=900
    )
    assert result.returncode == 0, (
        f"compose up failed:\nstdout: {result.stdout}\nstderr: {result.stderr}"
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

        # Give discovery a real presentation surface, then start the current
        # standalone daemon binary on both nodes.
        for node in ("node-a", "node-b"):
            result = docker_exec(
                node, "tmux new-session -d -s integration"
            )
            assert result.returncode == 0, (
                f"tmux start failed on {node}: {result.stderr}"
            )
            start_daemon(node)

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
                h["host_name"] == "node-b"
                and h["connection_status"] == "Connected"
                for h in result.get("hosts", [])
            )

        wait_for(peers_connected, "node-b connected to node-a", timeout=90)

        yield {"node-a": "node-a", "node-b": "node-b"}

    finally:
        # Print daemon logs for debugging (daemon writes to state_dir/daemon.log)
        for node in ("node-a", "node-b"):
            log = daemon_log(node)
            if log:
                print(f"\n=== {node} daemon log ===\n{log}")
            stdio = docker_exec(
                node, "cat ~/.config/flotilla/daemon-stdio.log"
            )
            if stdio.stdout:
                print(f"\n=== {node} daemon stdio ===\n{stdio.stdout}")

        result = compose("down", "-v", "--remove-orphans")
        if result.returncode != 0:
            print(f"\n=== teardown failed (rc={result.returncode}) ===")
            print(result.stderr)
