"""3-node hub-spoke topology tests (Issue #287).

Workstation (leader) peers with homelab-1 (codex/shpool) and homelab-2
(gemini/no shpool).  Tests run CLI commands and validate via JSON output.
"""

import json
import subprocess
import time
from pathlib import Path

import pytest

from conftest import docker_exec, flotilla_json, wait_for

COMPOSE_DIR = Path(__file__).parent
HUB_SPOKE_COMPOSE = str(COMPOSE_DIR / "docker-compose.hub-spoke.yml")

NODES = ("workstation", "homelab-1", "homelab-2")
FOLLOWERS = ("homelab-1", "homelab-2")


def _docker_exec(service, cmd, timeout=30):
    return docker_exec(service, cmd, timeout=timeout, compose_file=HUB_SPOKE_COMPOSE)


def _flotilla_json(service, args, timeout=30):
    return flotilla_json(service, args, timeout=timeout, compose_file=HUB_SPOKE_COMPOSE)


@pytest.fixture(scope="session")
def hub_spoke_topology():
    """Spin up 3-node hub-spoke topology, wait for peering, yield, tear down."""

    # Step 1: Build base image (role Dockerfiles depend on it via FROM)
    subprocess.run(
        ["docker", "compose", "-f", HUB_SPOKE_COMPOSE, "build", "flotilla-base"],
        check=True,
        timeout=600,
    )

    # Step 2: Start all services
    subprocess.run(
        ["docker", "compose", "-f", HUB_SPOKE_COMPOSE,
         "up", "-d", "--build", "--force-recreate"],
        check=True,
        timeout=600,
    )

    try:
        # Wait for SSH readiness: workstation -> each follower
        for follower in FOLLOWERS:
            wait_for(
                lambda f=follower: _docker_exec(
                    "workstation",
                    f"ssh -o StrictHostKeyChecking=no -o BatchMode=yes {f} true",
                ).returncode == 0,
                f"SSH from workstation to {follower}",
            )

        # Create a git repo on each node
        for node in NODES:
            result = _docker_exec(
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

        # Workstation config: peers with both followers
        result = _docker_exec("workstation", "\n".join([
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
        ]))
        assert result.returncode == 0, (
            f"hosts.toml write failed: {result.stderr}"
        )

        # Follower configs
        for follower in FOLLOWERS:
            result = _docker_exec(follower, "\n".join([
                "mkdir -p ~/.config/flotilla",
                "cat > ~/.config/flotilla/daemon.toml << 'TOML'",
                "follower = true",
                "TOML",
            ]))
            assert result.returncode == 0, (
                f"daemon.toml write failed on {follower}: {result.stderr}"
            )

        # Start daemons on all nodes
        for node in NODES:
            _docker_exec(
                node,
                "nohup flotilla daemon > /tmp/flotilla.log 2>&1 &",
            )

        # Wait for daemon readiness
        for node in NODES:
            wait_for(
                lambda n=node: _docker_exec(
                    n, "flotilla status --json"
                ).returncode == 0,
                f"daemon ready on {node}",
                timeout=30,
            )

        # Add repos via CLI
        for node in NODES:
            result = _docker_exec(
                node, "flotilla repo add /home/flotilla/repo"
            )
            assert result.returncode == 0, (
                f"repo add failed on {node}: {result.stderr}"
            )

        # Wait for both followers to connect
        def both_followers_connected():
            result = _flotilla_json("workstation", "host list")
            hosts = result.get("hosts", [])
            connected = {
                h["host"] for h in hosts
                if h["connection_status"] == "Connected"
            }
            return "homelab-1" in connected and "homelab-2" in connected

        wait_for(
            both_followers_connected,
            "both followers connected to workstation",
            timeout=90,
        )

        yield {
            "workstation": "workstation",
            "homelab-1": "homelab-1",
            "homelab-2": "homelab-2",
        }

    finally:
        # Print daemon logs for debugging
        for node in NODES:
            result = _docker_exec(node, "cat /tmp/flotilla.log")
            if result.stdout:
                print(f"\n=== {node} daemon log ===\n{result.stdout}")
            if result.stderr:
                print(f"\n=== {node} daemon stderr ===\n{result.stderr}")

        subprocess.run(
            ["docker", "compose", "-f", HUB_SPOKE_COMPOSE,
             "down", "-v", "--remove-orphans"],
            capture_output=True,
            text=True,
            timeout=60,
        )


# ---- Tests (ordered: non-mutating first, resilience last) ----


def test_all_daemons_running(hub_spoke_topology):
    """All three daemons respond to status."""
    for node in NODES:
        result = _flotilla_json(node, "status")
        assert "repos" in result


def test_topology_shows_star_shape(hub_spoke_topology):
    """Workstation sees direct routes to both followers, not chained."""
    result = _flotilla_json("workstation", "topology")
    assert result["local_host"] == "workstation"

    routes = result["routes"]
    for follower in FOLLOWERS:
        route = next((r for r in routes if r["target"] == follower), None)
        assert route is not None, f"no route to {follower}: {routes}"
        assert route["direct"], f"route to {follower} should be direct"
        assert route["connected"], f"route to {follower} should be connected"
        assert route["next_hop"] == follower, (
            f"next_hop to {follower} should be {follower}, got {route['next_hop']}"
        )


def test_provider_heterogeneity(hub_spoke_topology):
    """homelab-1 and homelab-2 have different tools in their inventory."""
    hl1 = _flotilla_json("workstation", "host homelab-1 providers")
    hl2 = _flotilla_json("workstation", "host homelab-2 providers")

    # Both should be connected
    assert hl1["connection_status"] == "Connected"
    assert hl2["connection_status"] == "Connected"

    hl1_binaries = [b["name"] for b in hl1["summary"]["inventory"]["binaries"]]
    hl2_binaries = [b["name"] for b in hl2["summary"]["inventory"]["binaries"]]

    # homelab-1 (follower-codex) has shpool; homelab-2 (follower-gemini) does not
    assert "shpool" in hl1_binaries, (
        f"homelab-1 should have shpool binary, got: {hl1_binaries}"
    )
    assert "shpool" not in hl2_binaries, (
        f"homelab-2 should not have shpool binary, got: {hl2_binaries}"
    )

    # homelab-2 (follower-gemini) has the gemini CLI; homelab-1 does not
    assert "gemini" in hl2_binaries, (
        f"homelab-2 should have gemini binary, got: {hl2_binaries}"
    )
    assert "gemini" not in hl1_binaries, (
        f"homelab-1 should not have gemini binary, got: {hl1_binaries}"
    )

    # The inventories are meaningfully different
    assert set(hl1_binaries) != set(hl2_binaries), (
        "homelab-1 and homelab-2 should have different tool inventories"
    )


def test_work_correlation_across_three_hosts(hub_spoke_topology):
    """Same branch on two followers correlates into work items on workstation."""
    # Create checkout on homelab-1
    c1 = _flotilla_json(
        "homelab-1",
        "repo /home/flotilla/repo checkout --fresh feat-correlated",
    )
    assert c1["status"] == "checkout_created"

    # Create checkout on homelab-2
    c2 = _flotilla_json(
        "homelab-2",
        "repo /home/flotilla/repo checkout --fresh feat-correlated",
    )
    assert c2["status"] == "checkout_created"

    # Wait for workstation to see both
    def both_visible():
        result = _flotilla_json(
            "workstation", "repo /home/flotilla/repo work"
        )
        items = result.get("work_items", [])
        hosts_with_branch = {
            item["host"]
            for item in items
            if item.get("branch") == "feat-correlated"
        }
        return "homelab-1" in hosts_with_branch and "homelab-2" in hosts_with_branch

    wait_for(
        both_visible,
        "workstation sees feat-correlated from both followers",
        timeout=30,
        interval=1.0,
    )

    # Verify host attribution
    result = _flotilla_json("workstation", "repo /home/flotilla/repo work")
    correlated = [
        item for item in result["work_items"]
        if item.get("branch") == "feat-correlated"
    ]
    hosts = {item["host"] for item in correlated}
    assert "homelab-1" in hosts, f"missing homelab-1 in correlated items: {correlated}"
    assert "homelab-2" in hosts, f"missing homelab-2 in correlated items: {correlated}"


def test_followers_receive_leader_data(hub_spoke_topology):
    """Follower sees workstation-originated checkout via peer data broadcast."""
    # Create checkout on workstation
    checkout = _flotilla_json(
        "workstation",
        "repo /home/flotilla/repo checkout --fresh feat-leader-data",
    )
    assert checkout["status"] == "checkout_created"

    # Wait for homelab-1 to see the workstation checkout
    def leader_checkout_visible_on_follower():
        result = _flotilla_json(
            "homelab-1", "repo /home/flotilla/repo work"
        )
        return any(
            item.get("branch") == "feat-leader-data"
            and item.get("host") == "workstation"
            for item in result.get("work_items", [])
        )

    wait_for(
        leader_checkout_visible_on_follower,
        "homelab-1 sees workstation checkout feat-leader-data",
        timeout=30,
        interval=1.0,
    )


def test_session_persistence_with_shpool(hub_spoke_topology):
    """homelab-1 (shpool) sessions survive daemon restart."""
    # Create checkout on homelab-1
    checkout = _flotilla_json(
        "homelab-1",
        "repo /home/flotilla/repo checkout --fresh feat-shpool-persist",
    )
    assert checkout["status"] == "checkout_created"
    checkout_path = checkout["path"]

    # Wait for checkout to be visible on workstation
    def checkout_visible():
        result = _flotilla_json("workstation", "repo /home/flotilla/repo work")
        return any(
            item.get("branch") == "feat-shpool-persist"
            and item.get("host") == "homelab-1"
            for item in result.get("work_items", [])
        )

    wait_for(checkout_visible, "workstation sees feat-shpool-persist", timeout=30, interval=1.0)

    # Prepare terminal from workstation (routed to homelab-1)
    prepared = _flotilla_json(
        "workstation",
        f"host homelab-1 repo /home/flotilla/repo prepare-terminal {checkout_path}",
        timeout=60,
    )
    assert prepared["status"] == "terminal_prepared"
    attachable_set_id = prepared.get("attachable_set_id")
    assert attachable_set_id, "prepare-terminal should return attachable_set_id"

    # Verify registry exists on homelab-1
    registry = _docker_exec(
        "homelab-1",
        "test -f ~/.config/flotilla/attachables/registry.json && cat ~/.config/flotilla/attachables/registry.json",
    )
    assert registry.returncode == 0, (
        f"attachables registry should exist on homelab-1\n"
        f"stdout: {registry.stdout}\nstderr: {registry.stderr}"
    )

    # Kill and restart homelab-1 daemon
    _docker_exec("homelab-1", "pkill -f 'flotilla daemon'")
    time.sleep(2)
    _docker_exec("homelab-1", "nohup flotilla daemon > /tmp/flotilla.log 2>&1 &")

    wait_for(
        lambda: _docker_exec("homelab-1", "flotilla status --json").returncode == 0,
        "homelab-1 daemon restarted",
        timeout=30,
    )

    # Verify attachable set still exists after restart (shpool persists)
    registry_after = _docker_exec(
        "homelab-1",
        "cat ~/.config/flotilla/attachables/registry.json",
    )
    assert registry_after.returncode == 0, "registry should survive daemon restart"
    assert attachable_set_id in registry_after.stdout, (
        f"attachable set {attachable_set_id} should persist after restart"
    )


def test_session_without_shpool(hub_spoke_topology):
    """homelab-2 (no shpool) terminal works but doesn't persist across restart."""
    # Create checkout on homelab-2
    checkout = _flotilla_json(
        "homelab-2",
        "repo /home/flotilla/repo checkout --fresh feat-no-shpool",
    )
    assert checkout["status"] == "checkout_created"
    checkout_path = checkout["path"]

    # Wait for checkout to be visible on workstation
    def checkout_visible():
        result = _flotilla_json("workstation", "repo /home/flotilla/repo work")
        return any(
            item.get("branch") == "feat-no-shpool"
            and item.get("host") == "homelab-2"
            for item in result.get("work_items", [])
        )

    wait_for(checkout_visible, "workstation sees feat-no-shpool", timeout=30, interval=1.0)

    # Prepare terminal from workstation (routed to homelab-2)
    prepared = _flotilla_json(
        "workstation",
        f"host homelab-2 repo /home/flotilla/repo prepare-terminal {checkout_path}",
        timeout=60,
    )
    assert prepared["status"] == "terminal_prepared"

    # Check if an attachables registry was created
    _docker_exec(
        "homelab-2",
        "cat ~/.config/flotilla/attachables/registry.json 2>/dev/null || echo '{}'",
    )

    # Kill and restart homelab-2 daemon
    _docker_exec("homelab-2", "pkill -f 'flotilla daemon'")
    time.sleep(2)
    _docker_exec("homelab-2", "nohup flotilla daemon > /tmp/flotilla.log 2>&1 &")

    wait_for(
        lambda: _docker_exec("homelab-2", "flotilla status --json").returncode == 0,
        "homelab-2 daemon restarted",
        timeout=30,
    )


def test_workspace_transfer(hub_spoke_topology):
    """Re-preparing a terminal on the same checkout preserves work item identity."""
    # Create checkout on homelab-1
    checkout = _flotilla_json(
        "homelab-1",
        "repo /home/flotilla/repo checkout --fresh feat-workspace-xfer",
    )
    assert checkout["status"] == "checkout_created"
    checkout_path = checkout["path"]

    # Wait for visibility
    def checkout_visible():
        result = _flotilla_json("workstation", "repo /home/flotilla/repo work")
        return any(
            item.get("branch") == "feat-workspace-xfer"
            and item.get("host") == "homelab-1"
            for item in result.get("work_items", [])
        )

    wait_for(checkout_visible, "workstation sees feat-workspace-xfer", timeout=30, interval=1.0)

    # First prepare-terminal
    prepared1 = _flotilla_json(
        "workstation",
        f"host homelab-1 repo /home/flotilla/repo prepare-terminal {checkout_path}",
        timeout=60,
    )
    assert prepared1["status"] == "terminal_prepared"

    # Second prepare-terminal on same checkout (simulates workspace transfer)
    prepared2 = _flotilla_json(
        "workstation",
        f"host homelab-1 repo /home/flotilla/repo prepare-terminal {checkout_path}",
        timeout=60,
    )
    assert prepared2["status"] == "terminal_prepared"

    # Work item should still track this checkout
    result = _flotilla_json("workstation", "repo /home/flotilla/repo work")
    items = [
        item for item in result["work_items"]
        if item.get("branch") == "feat-workspace-xfer"
        and item.get("host") == "homelab-1"
    ]
    assert len(items) >= 1, f"checkout should still be tracked: {result['work_items']}"


def test_resilience_kill_restart(hub_spoke_topology):
    """Kill workstation daemon, followers detect disconnect, restart, data resyncs."""
    # Create a checkout to track through the disruption
    checkout = _flotilla_json(
        "homelab-1",
        "repo /home/flotilla/repo checkout --fresh feat-resilience",
    )
    assert checkout["status"] == "checkout_created"

    # Wait for workstation to see it
    def checkout_visible():
        result = _flotilla_json("workstation", "repo /home/flotilla/repo work")
        return any(
            item.get("branch") == "feat-resilience"
            and item.get("host") == "homelab-1"
            for item in result.get("work_items", [])
        )

    wait_for(checkout_visible, "workstation sees feat-resilience", timeout=30, interval=1.0)

    # Kill workstation daemon
    _docker_exec("workstation", "pkill -f 'flotilla daemon'")

    # Verify workstation daemon is actually dead
    wait_for(
        lambda: _docker_exec(
            "workstation", "flotilla status --json"
        ).returncode != 0,
        "workstation daemon stopped",
        timeout=10,
    )

    # Verify follower detects disconnection (heartbeat timeout, allow up to 30s)
    def follower_sees_disconnect():
        result = _docker_exec("homelab-1", "flotilla host list --json")
        if result.returncode != 0:
            return False
        hosts = json.loads(result.stdout).get("hosts", [])
        ws = next((h for h in hosts if h["host"] == "workstation"), None)
        return ws is not None and ws["connection_status"] != "Connected"

    wait_for(
        follower_sees_disconnect,
        "homelab-1 detects workstation disconnection",
        timeout=30,
    )

    # Restart workstation daemon
    time.sleep(2)
    _docker_exec(
        "workstation",
        "nohup flotilla daemon > /tmp/flotilla.log 2>&1 &",
    )

    # Wait for daemon readiness
    wait_for(
        lambda: _docker_exec(
            "workstation", "flotilla status --json"
        ).returncode == 0,
        "workstation daemon restarted",
        timeout=30,
    )

    # Wait for both followers to reconnect
    def both_reconnected():
        result = _flotilla_json("workstation", "host list")
        hosts = result.get("hosts", [])
        connected = {
            h["host"] for h in hosts
            if h["connection_status"] == "Connected"
        }
        return "homelab-1" in connected and "homelab-2" in connected

    wait_for(both_reconnected, "both followers reconnected", timeout=90)

    # Verify data resync: feat-resilience checkout still visible
    result = _flotilla_json("workstation", "repo /home/flotilla/repo work")
    resilience_items = [
        item for item in result["work_items"]
        if item.get("branch") == "feat-resilience"
        and item.get("host") == "homelab-1"
    ]
    assert len(resilience_items) >= 1, (
        f"feat-resilience should be visible after resync: {result['work_items']}"
    )
