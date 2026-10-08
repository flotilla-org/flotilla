"""Three-node hub-spoke topology coverage for the compose harness.

The workstation peers directly with two heterogeneous followers:
homelab-1 has codex, while homelab-2 has gemini and uses the
passthrough terminal pool. Commands run through the real CLI/daemon/SSH
boundary and assertions use the current JSON protocol.
"""

import json
from pathlib import Path

import pytest

from conftest import (
    compose,
    daemon_log,
    docker_exec,
    flotilla_json,
    start_daemon,
    stop_daemon,
    wait_for,
)

COMPOSE_DIR = Path(__file__).parent
HUB_SPOKE_COMPOSE = str(COMPOSE_DIR / "docker-compose.hub-spoke.yml")

REPO_PATH = "/home/flotilla/repo"

NODES = ("workstation", "homelab-1", "homelab-2")
FOLLOWERS = ("homelab-1", "homelab-2")
pytestmark = pytest.mark.timeout(1800)


def hub_exec(service: str, cmd: str, timeout: int = 30):
    return docker_exec(
        service,
        cmd,
        timeout=timeout,
        compose_file=HUB_SPOKE_COMPOSE,
    )


def hub_json(service: str, args: str, timeout: int = 30) -> dict | list:
    return flotilla_json(
        service,
        args,
        timeout=timeout,
        compose_file=HUB_SPOKE_COMPOSE,
    )


def local_repository_key(node: str) -> str:
    """Address the adopted identity on this host, not a fleet-wide path."""
    hosts = hub_json(node, "resource list hosts")["records"]
    host_ref = next(
        (
            record["object"]["metadata"]["name"]
            for record in hosts
            if record["object"]["spec"]["display_name"] == node
        ),
        None,
    )
    assert host_ref is not None, f"no Host for {node}: {hosts}"
    repositories = hub_json(node, "resource list repositories")["records"]
    repository_key = next(
        (
            record["object"]["metadata"]["name"]
            for record in repositories
            if record["object"]["spec"]["identity"].get("git_common_dir") == f"{REPO_PATH}/.git"
            and record["object"]["spec"]["identity"].get("host_ref") == host_ref
        ),
        None,
    )
    assert repository_key is not None, f"no adopted Repository for {node} ({host_ref}): {repositories}"
    return repository_key


def hub_compose(*args: str, timeout: int = 60):
    return compose(
        *args,
        timeout=timeout,
        compose_file=HUB_SPOKE_COMPOSE,
    )


def hub_start_daemon(service: str):
    start_daemon(service, compose_file=HUB_SPOKE_COMPOSE)


def hub_stop_daemon(service: str):
    stop_daemon(service, compose_file=HUB_SPOKE_COMPOSE)


def wait_for_follower(follower: str):
    wait_for(
        lambda: (
            next(
                host
                for host in hub_json("workstation", "host list")["hosts"]
                if host["host"] == follower
            )["link"]
            == "Connected"
        ),
        f"{follower} connected to workstation",
        timeout=90,
        interval=0.5,
    )


@pytest.fixture(scope="module")
def hub_spoke_topology():
    """Start the three nodes, configure the star, and wait for replication."""
    result = hub_compose("build", "flotilla-base", timeout=900)
    assert result.returncode == 0, (
        f"flotilla-base build failed:\nstdout: {result.stdout}\nstderr: {result.stderr}"
    )
    result = hub_compose(
        "up",
        "-d",
        "--build",
        "--force-recreate",
        timeout=1800,
    )
    assert result.returncode == 0, (
        f"compose up failed:\nstdout: {result.stdout}\nstderr: {result.stderr}"
    )

    try:
        for follower in FOLLOWERS:
            wait_for(
                lambda f=follower: (
                    hub_exec(
                        "workstation",
                        f"ssh -o StrictHostKeyChecking=no -o BatchMode=yes {f} true",
                    ).returncode
                    == 0
                ),
                f"SSH from workstation to {follower}",
            )

        for node in NODES:
            result = hub_exec(
                node,
                "git config --global user.email test@test.com && "
                "git config --global user.name test && "
                f"git init --initial-branch=master {REPO_PATH} && "
                f"cd {REPO_PATH} && "
                "git commit --allow-empty -m init",
            )
            assert result.returncode == 0, f"git init failed on {node}: {result.stderr}"

        result = hub_exec(
            "workstation",
            "\n".join(
                [
                    "mkdir -p ~/.config/flotilla",
                    "cat > ~/.config/flotilla/hosts.toml << 'TOML'",
                    "[hosts.homelab-1]",
                    'hostname = "homelab-1"',
                    'expected_host_name = "homelab-1"',
                    "",
                    "[hosts.homelab-2]",
                    'hostname = "homelab-2"',
                    'expected_host_name = "homelab-2"',
                    "TOML",
                ]
            ),
        )
        assert result.returncode == 0, f"hosts.toml write failed: {result.stderr}"

        for follower in FOLLOWERS:
            result = hub_exec(
                follower,
                "\n".join(
                    [
                        "mkdir -p ~/.config/flotilla",
                        "cat > ~/.config/flotilla/daemon.toml << 'TOML'",
                        "follower = true",
                        "TOML",
                    ]
                ),
            )
            assert result.returncode == 0, (
                f"daemon.toml write failed on {follower}: {result.stderr}"
            )

        for node in NODES:
            hub_start_daemon(node)

        def daemon_ready(node):
            result = hub_exec(node, "flotilla status --json")
            if result.returncode != 0:
                raise RuntimeError(
                    f"flotilla status failed on {node} (rc={result.returncode}): "
                    f"{result.stderr.strip()}"
                )
            return True

        for node in NODES:
            wait_for(
                lambda n=node: daemon_ready(n),
                f"daemon ready on {node}",
                timeout=30,
                interval=0.5,
            )

        for node in NODES:
            result = hub_exec(node, f"flotilla repo add {REPO_PATH}")
            assert result.returncode == 0, f"repo add failed on {node}: {result.stderr}"

        for follower in FOLLOWERS:
            wait_for_follower(follower)

        # Read each node's own adopted identity once; these durable keys survive restart.
        yield {node: local_repository_key(node) for node in NODES}
    finally:
        for node in NODES:
            log = daemon_log(node, compose_file=HUB_SPOKE_COMPOSE)
            if log:
                print(f"\n=== {node} daemon log ===\n{log}")
            stdio = hub_exec(node, "cat ~/.config/flotilla/daemon-stdio.log")
            if stdio.stdout:
                print(f"\n=== {node} daemon stdio ===\n{stdio.stdout}")

        result = hub_compose("down", "-v", "--remove-orphans")
        if result.returncode != 0:
            print(
                f"\n=== teardown failed (rc={result.returncode}) ===\n{result.stderr}"
            )


def test_all_daemons_running(hub_spoke_topology):
    """All three daemons expose their locally tracked repository."""
    for node in NODES:
        result = hub_json(node, "status")
        assert result["repos"]


def test_topology_shows_star_shape(hub_spoke_topology):
    """The workstation has two direct routes with no follower as next hop."""
    result = hub_json("workstation", "topology")
    assert result["local_node"]["display_name"] == "workstation"

    routes = result["routes"]
    for follower in FOLLOWERS:
        route = next(
            (route for route in routes if route["target"]["display_name"] == follower),
            None,
        )
        assert route is not None, f"no route to {follower}: {routes}"
        assert route["direct"]
        assert route["connected"]
        assert route["next_hop"]["display_name"] == follower


def test_provider_heterogeneity(hub_spoke_topology):
    """Each follower publishes its distinct real tool inventory."""
    homelab_1 = hub_json("workstation", "host homelab-1 providers")
    homelab_2 = hub_json("workstation", "host homelab-2 providers")

    assert homelab_1["connection_status"] == "Connected"
    assert homelab_2["connection_status"] == "Connected"

    binaries_1 = {
        binary["name"] for binary in homelab_1["summary"]["inventory"]["binaries"]
    }
    binaries_2 = {
        binary["name"] for binary in homelab_2["summary"]["inventory"]["binaries"]
    }
    assert "codex" in binaries_1
    assert "codex" not in binaries_2
    assert binaries_1 != binaries_2

    gemini = hub_exec("homelab-2", "command -v gemini")
    assert gemini.returncode == 0, gemini.stderr
    no_gemini = hub_exec("homelab-1", "command -v gemini")
    assert no_gemini.returncode != 0


@pytest.mark.parametrize("host", ["homelab-1", "homelab-2"])
def test_remote_personal_terminal_preparation_is_retired(hub_spoke_topology, host):
    """Retired prepare-terminal is rejected for both persistent and passthrough hosts (#2915)."""
    result = hub_exec(
        "workstation",
        f"flotilla --json host {host} repo retired-surface-repo prepare-terminal /unused",
    )
    assert result.returncode != 0, "retired prepare-terminal unexpectedly succeeded"
    assert "unexpected argument 'prepare-terminal' found" in result.stderr, result.stderr
    assert not result.stdout, result.stdout
