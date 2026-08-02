"""2-node minimal topology tests (Issue #286).

All tests run commands on node-a (the "user's desktop") and validate
that multi-host peering with node-b works via the CLI JSON output.
"""

from conftest import docker_exec, flotilla_json, wait_for


def test_both_daemons_running(topology):
    """Both daemons respond to status."""
    for node in (topology["node-a"], topology["node-b"]):
        result = flotilla_json(node, "status")
        assert "repos" in result


def test_host_list_shows_peer(topology):
    """node-a sees node-b as a connected peer in host list."""
    result = flotilla_json(topology["node-a"], "host list")
    hosts = result["hosts"]

    # Should see at least local host + node-b
    assert len(hosts) >= 2

    peer = next((h for h in hosts if h["host"] == "node-b"), None)
    assert peer is not None, f"node-b not in host list: {hosts}"
    assert peer["link"] == "Connected"
    assert not peer["is_local"]
    assert peer["configured"]


def test_host_list_shows_local(topology):
    """node-a sees itself as local in host list."""
    result = flotilla_json(topology["node-a"], "host list")
    local = next((h for h in result["hosts"] if h["is_local"]), None)
    assert local is not None, "no local host in host list"
    assert local["host"] == "node-a"


def test_topology_shows_direct_route(topology):
    """Topology shows a direct, connected route to node-b."""
    result = flotilla_json(topology["node-a"], "topology")
    assert result["local_node"]["display_name"] == "node-a"

    routes = result["routes"]
    node_b_route = next(
        (r for r in routes if r["target"]["display_name"] == "node-b"), None
    )
    assert node_b_route is not None, f"no route to node-b: {routes}"
    assert node_b_route["direct"]
    assert node_b_route["connected"]
    assert node_b_route["next_hop"]["display_name"] == "node-b"


def test_host_status_peer(topology):
    """Can query node-b's status from node-a."""
    result = flotilla_json(topology["node-a"], "host node-b status")
    assert result["host_name"] == "node-b"
    assert result["connection_status"] == "Connected"
    # The query executes on node-b, so the returned status is local from
    # node-b's point of view.
    assert result["is_local"]
    assert result["summary"]["host_name"] == "node-b"
    assert result["visible_environments"]


def test_host_providers_peer(topology):
    """Can query node-b's providers from node-a."""
    result = flotilla_json(topology["node-a"], "host node-b providers")
    assert result["host_name"] == "node-b"
    assert result["connection_status"] == "Connected"

    summary = result["summary"]
    # Both fields exist on HostSummary
    assert "providers" in summary
    assert "inventory" in summary


def test_status_shows_repos(topology):
    """Status shows at least the locally tracked repo."""
    result = flotilla_json(topology["node-a"], "status")
    assert len(result["repos"]) >= 1

    repo = result["repos"][0]
    assert "path" in repo
    assert "work_item_count" in repo


def test_peer_repo_visible_in_merged_repo_work(topology):
    """node-b's tracked repo is visible in node-a's merged repo work."""
    result = flotilla_json(
        topology["node-a"], "repo /home/flotilla/repo work"
    )
    assert any(
        item.get("source") == "node-b"
        for item in result.get("work_items", [])
    )


def test_remote_prepare_terminal_returns_attachable_set_id(topology):
    """Remote prepare-terminal returns a Flotilla-owned attachable set id."""
    checkout = flotilla_json(
        topology["node-b"],
        "repo /home/flotilla/repo checkout --fresh feat-prepare",
    )
    assert checkout["kind"] == "checkout_created"
    checkout_path = checkout["path"]["path"]

    def checkout_visible_on_node_a():
        result = flotilla_json(topology["node-a"], "repo /home/flotilla/repo work")
        return any(
            item.get("branch") == "feat-prepare"
            and item.get("source") == "node-b"
            for item in result.get("work_items", [])
        )

    wait_for(
        checkout_visible_on_node_a,
        "node-a sees node-b checkout feat-prepare",
        timeout=30,
        interval=1.0,
    )

    prepared = flotilla_json(
        topology["node-a"],
        f"host node-b repo /home/flotilla/repo prepare-terminal {checkout_path}",
        timeout=60,
    )
    assert prepared["kind"] == "terminal_prepared"
    assert prepared.get("attachable_set_id"), (
        "remote prepare-terminal should include attachable_set_id"
    )

    registry = docker_exec(
        topology["node-b"],
        "test -f ~/.config/flotilla/attachables/registry.json && cat ~/.config/flotilla/attachables/registry.json",
    )
    assert registry.returncode == 0, (
        "remote prepare-terminal should create attachables registry\n"
        f"stdout: {registry.stdout}\nstderr: {registry.stderr}"
    )
