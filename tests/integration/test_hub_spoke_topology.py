"""Three-node hub-spoke topology coverage for the compose harness.

The workstation peers directly with two heterogeneous followers:
homelab-1 has codex and shpool, while homelab-2 has gemini and uses the
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

NODES = ("workstation", "homelab-1", "homelab-2")
FOLLOWERS = ("homelab-1", "homelab-2")
pytestmark = pytest.mark.timeout(900)


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
                if host["host_name"] == follower
            )["connection_status"]
            == "Connected"
        ),
        f"{follower} connected to workstation",
        timeout=90,
        interval=0.5,
    )


def wait_for_checkout_on_workstation(branch: str, source: str):
    wait_for(
        lambda: any(
            item.get("branch") == branch and item.get("source") == source
            for item in hub_json("workstation", "repo /home/flotilla/repo work")[
                "work_items"
            ]
        ),
        f"workstation sees {branch} from {source}",
        timeout=30,
        interval=1.0,
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
        timeout=900,
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
                "git init /home/flotilla/repo && "
                "cd /home/flotilla/repo && "
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
                    'daemon_socket = "/home/flotilla/.config/flotilla/flotilla.sock"',
                    "",
                    "[hosts.homelab-2]",
                    'hostname = "homelab-2"',
                    'expected_host_name = "homelab-2"',
                    'daemon_socket = "/home/flotilla/.config/flotilla/flotilla.sock"',
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
            result = hub_exec(node, "tmux new-session -d -s integration")
            assert result.returncode == 0, (
                f"tmux start failed on {node}: {result.stderr}"
            )
            hub_start_daemon(node)

        for node in NODES:
            wait_for(
                lambda n=node: hub_exec(n, "flotilla status --json").returncode == 0,
                f"daemon ready on {node}",
                timeout=30,
                interval=0.5,
            )

        for node in NODES:
            result = hub_exec(node, "flotilla repo add /home/flotilla/repo")
            assert result.returncode == 0, f"repo add failed on {node}: {result.stderr}"

        for follower in FOLLOWERS:
            wait_for_follower(follower)

        yield
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
    assert "shpool" in binaries_1
    assert "codex" not in binaries_2
    assert "shpool" not in binaries_2
    assert binaries_1 != binaries_2

    gemini = hub_exec("homelab-2", "command -v gemini")
    assert gemini.returncode == 0, gemini.stderr
    no_gemini = hub_exec("homelab-1", "command -v gemini")
    assert no_gemini.returncode != 0


def test_work_correlation_across_three_hosts(hub_spoke_topology):
    """Matching main branches retain all three sources on workstation."""

    def all_nodes_visible():
        result = hub_json("workstation", "repo /home/flotilla/repo work")
        sources = {
            item["source"]
            for item in result["work_items"]
            if item.get("branch") == "master" and item["is_main_checkout"]
        }
        return set(NODES) <= sources

    wait_for(
        all_nodes_visible,
        "workstation sees the main checkout from all three nodes",
        timeout=30,
        interval=1.0,
    )


def test_followers_receive_leader_data(hub_spoke_topology):
    """A follower sees a workstation checkout through peer publication."""
    checkout = hub_json(
        "workstation",
        "repo /home/flotilla/repo checkout --fresh feat-leader-data",
    )
    assert checkout["kind"] == "checkout_created"

    wait_for(
        lambda: any(
            item.get("branch") == "feat-leader-data"
            and item.get("source") == "workstation"
            for item in hub_json("homelab-1", "repo /home/flotilla/repo work")[
                "work_items"
            ]
        ),
        "homelab-1 sees workstation checkout feat-leader-data",
        timeout=30,
        interval=1.0,
    )


def test_shpool_attachable_set_survives_daemon_restart(
    hub_spoke_topology,
):
    """A shpool-backed terminal keeps its durable set across restart."""
    checkout = hub_json(
        "homelab-1",
        "repo /home/flotilla/repo checkout --fresh feat-shpool-persist",
    )
    assert checkout["kind"] == "checkout_created"
    checkout_path = checkout["path"]["path"]
    wait_for_checkout_on_workstation("feat-shpool-persist", "homelab-1")

    prepared = hub_json(
        "workstation",
        f"host homelab-1 repo /home/flotilla/repo prepare-terminal {checkout_path}",
        timeout=60,
    )
    assert prepared["kind"] == "terminal_prepared"
    assert prepared["attachable_set_id"]

    assert any(
        arg.get("value", "").startswith("shpool ")
        for command in prepared["commands"]
        for arg in command["args"]
    )

    def attachable_registry():
        registry = hub_exec(
            "homelab-1",
            "cat ~/.config/flotilla/attachables/registry.json",
        )
        assert registry.returncode == 0, registry.stderr
        return json.loads(registry.stdout)

    attachable_set_id = prepared["attachable_set_id"]
    assert attachable_set_id in attachable_registry()["sets"]

    hub_stop_daemon("homelab-1")
    hub_start_daemon("homelab-1")
    wait_for(
        lambda: hub_exec("homelab-1", "flotilla status --json").returncode == 0,
        "homelab-1 daemon restarted",
        timeout=30,
        interval=0.5,
    )
    wait_for_follower("homelab-1")
    assert attachable_set_id in attachable_registry()["sets"]


def test_terminal_without_shpool_uses_passthrough(hub_spoke_topology):
    """The follower without shpool returns executable fallback commands."""
    checkout = hub_json(
        "homelab-2",
        "repo /home/flotilla/repo checkout --fresh feat-no-shpool",
    )
    assert checkout["kind"] == "checkout_created"
    checkout_path = checkout["path"]["path"]
    wait_for_checkout_on_workstation("feat-no-shpool", "homelab-2")

    prepared = hub_json(
        "workstation",
        f"host homelab-2 repo /home/flotilla/repo prepare-terminal {checkout_path}",
        timeout=60,
    )
    assert prepared["kind"] == "terminal_prepared"
    assert prepared["attachable_set_id"]
    assert prepared["commands"]
    assert all(
        "shpool" not in arg.get("value", "")
        for command in prepared["commands"]
        for arg in command["args"]
    )


def test_reprepare_reuses_attachable_identity(hub_spoke_topology):
    """Repeated preparation keeps the checkout's attachable-set identity."""
    checkout = hub_json(
        "homelab-1",
        "repo /home/flotilla/repo checkout --fresh feat-workspace-reprepare",
    )
    assert checkout["kind"] == "checkout_created"
    checkout_path = checkout["path"]["path"]
    wait_for_checkout_on_workstation("feat-workspace-reprepare", "homelab-1")
    command = (
        f"host homelab-1 repo /home/flotilla/repo prepare-terminal {checkout_path}"
    )

    first = hub_json("workstation", command, timeout=60)
    second = hub_json("workstation", command, timeout=60)

    assert first["kind"] == "terminal_prepared"
    assert second["kind"] == "terminal_prepared"
    assert first["attachable_set_id"] == second["attachable_set_id"]
    assert first["commands"] == second["commands"]
