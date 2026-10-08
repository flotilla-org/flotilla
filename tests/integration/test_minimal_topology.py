"""2-node minimal topology tests (Issue #286).

All tests run commands on node-a (the "user's desktop") and validate
that multi-host peering with node-b works via the CLI JSON output.
"""

import pytest

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


def test_repository_federates_by_canonical_remote(topology):
    """A repository tracked on node-b is visible in node-a's replica view."""
    canonical_remote = "https://github.com/flotilla-org/compose-replica-source"

    wait_for(
        lambda: any(
            (record.get("object") or {}).get("spec", {}).get("identity")
            == {
                "kind": "remote",
                "canonical_remote": canonical_remote,
            }
            for record in flotilla_json(
                topology["node-a"],
                "resource list repositories --include-replicas",
            )["records"]
        ),
        f"node-a sees node-b's repository replica for {canonical_remote}",
        timeout=30,
        interval=1.0,
    )


@pytest.mark.parametrize(
    ("command", "diagnostic"),
    [
        ("repo retired-surface-repo prepare-terminal /unused", "unexpected argument 'prepare-terminal' found"),
        ("workspace retired-workspace select", "unrecognized subcommand 'workspace'"),
        ("agent retired-agent teleport", "unrecognized subcommand 'teleport'"),
    ],
)
def test_remote_personal_workspace_commands_are_retired(topology, command, diagnostic):
    """Host routing rejects the retired personal-workspace CLI before dispatch (#2915)."""
    result = docker_exec(
        topology["node-a"], f"flotilla --json host node-b {command}"
    )
    assert result.returncode != 0, f"retired command unexpectedly succeeded: {command}"
    assert diagnostic in result.stderr, result.stderr
    assert not result.stdout, result.stdout


def test_daemon_launcher_executes_binary_with_log_environment(tmp_path, monkeypatch):
    """The topology launcher starts flotillad with its logging environment and arguments."""
    import os
    import subprocess

    import conftest

    tools_dir = tmp_path / "tools"
    tools_dir.mkdir()
    daemon = tools_dir / "flotillad"
    # Stand in only for the daemon process boundary; execute the real launcher shell.
    daemon.write_text(
        '#!/bin/sh\nprintf "%s\\n%s\\n" "$RUST_LOG" "$*" > "$HOME/daemon-started"\n'
    )
    daemon.chmod(0o755)
    child_env = dict(os.environ, HOME=str(tmp_path), PATH=f"{tools_dir}:{os.environ['PATH']}")
    child_env.pop("RUST_LOG", None)

    # Docker is the environment boundary; run the exact generated command locally.
    def local_exec(service, cmd, timeout=30, compose_file=None):
        return subprocess.run(
            ["bash", "-c", cmd], env=child_env, capture_output=True, text=True, timeout=timeout
        )

    monkeypatch.setattr(conftest, "docker_exec", local_exec)
    conftest.start_daemon("local-test")
    started = tmp_path / "daemon-started"
    conftest.wait_for(
        lambda: started.exists() and started.read_text() == "flotilla_daemon=debug\n--timeout 0\n",
        "launcher executes daemon with RUST_LOG and timeout arguments",
        timeout=3,
        interval=0.01,
    )
