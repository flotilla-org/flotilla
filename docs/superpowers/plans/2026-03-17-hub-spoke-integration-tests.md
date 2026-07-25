# Hub-Spoke Integration Tests Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a 3-node hub-spoke integration test suite (workstation + 2 followers with different providers) to verify provider heterogeneity, work correlation, session persistence, and resilience.

**Architecture:** Extends the existing 2-node Docker integration test infrastructure with a new compose file for 3 role-differentiated containers and a new test file with its own session-scoped fixture. Shared helpers in `conftest.py` are parameterized to support multiple compose files.

**Tech Stack:** Docker Compose, pytest, flotilla CLI (`--json` output)

**Spec:** `docs/superpowers/specs/2026-03-17-hub-spoke-integration-tests-design.md`

---

## File Map

| Action | File | Purpose |
|--------|------|---------|
| Modify | `tests/integration/conftest.py` | Add `compose_file` parameter to `docker_exec`, `flotilla_json` |
| Create | `tests/integration/docker-compose.hub-spoke.yml` | 3-node compose: flotilla-base + workstation + homelab-1 + homelab-2 |
| Create | `tests/integration/docker-compose.hub-spoke.dev.yml` | Dev override (pre-built binary from host) |
| Create | `tests/integration/test_hub_spoke_topology.py` | Hub-spoke fixture + 9 test cases |

---

### Task 1: Parameterize conftest.py helpers

**Files:**
- Modify: `tests/integration/conftest.py:14-42`

- [ ] **Step 1: Add `compose_file` parameter to `docker_exec`**

Change the signature and body of `docker_exec` to accept an optional `compose_file` parameter:

```python
def docker_exec(
    service: str, cmd: str, timeout: int = 30,
    compose_file: str = COMPOSE_FILE,
) -> subprocess.CompletedProcess:
    """Run a command inside a container via docker compose exec."""
    return subprocess.run(
        [
            "docker", "compose", "-f", compose_file,
            "exec", "-T", "-u", "flotilla", service, "bash", "-c", cmd,
        ],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
```

- [ ] **Step 2: Add `compose_file` parameter to `flotilla_json`**

```python
def flotilla_json(
    service: str, args: str, timeout: int = 30,
    compose_file: str = COMPOSE_FILE,
) -> dict | list:
    """Run a flotilla CLI command with --json and return parsed output."""
    result = docker_exec(service, f"flotilla {args} --json", timeout=timeout,
                         compose_file=compose_file)
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
```

- [ ] **Step 3: Verify existing 2-node tests still import and work**

The existing `test_minimal_topology.py` imports `docker_exec`, `flotilla_json`, `wait_for` from `conftest`. Since all new parameters have defaults matching the old behavior, no changes are needed to `test_minimal_topology.py`. Verify the imports resolve:

```bash
cd tests/integration && python -c "from conftest import docker_exec, flotilla_json, wait_for; print('OK')"
```

Expected: `OK`

- [ ] **Step 4: Commit**

```bash
git add tests/integration/conftest.py
git commit -m "refactor: parameterize conftest helpers with compose_file arg"
```

---

### Task 2: Create docker-compose.hub-spoke.yml

**Files:**
- Create: `tests/integration/docker-compose.hub-spoke.yml`

- [ ] **Step 1: Write the compose file**

```yaml
# 3-node hub-spoke topology: workstation (leader) + 2 followers.
#
# Build order matters: flotilla-base must be built first since role
# Dockerfiles use FROM flotilla-base.  The test fixture runs:
#   docker compose -f docker-compose.hub-spoke.yml build flotilla-base
#   docker compose -f docker-compose.hub-spoke.yml up -d --build --force-recreate
services:
  flotilla-base:
    build:
      context: ../..
      dockerfile: tests/integration/docker/base/Dockerfile
      target: full
    image: flotilla-base

  workstation:
    build:
      context: ../..
      dockerfile: tests/integration/docker/workstation/Dockerfile
    hostname: workstation
    volumes:
      - shared-keys:/shared-keys

  homelab-1:
    build:
      context: ../..
      dockerfile: tests/integration/docker/follower-codex/Dockerfile
    hostname: homelab-1
    volumes:
      - shared-keys:/shared-keys

  homelab-2:
    build:
      context: ../..
      dockerfile: tests/integration/docker/follower-gemini/Dockerfile
    hostname: homelab-2
    volumes:
      - shared-keys:/shared-keys

volumes:
  shared-keys:
```

- [ ] **Step 2: Commit**

```bash
git add tests/integration/docker-compose.hub-spoke.yml
git commit -m "feat: add docker-compose.hub-spoke.yml for 3-node topology"
```

---

### Task 3: Create docker-compose.hub-spoke.dev.yml

**Files:**
- Create: `tests/integration/docker-compose.hub-spoke.dev.yml`

- [ ] **Step 1: Write the dev override**

```yaml
# Override: use pre-built binary from host instead of cargo build.
# Usage: docker compose -f docker-compose.hub-spoke.yml -f docker-compose.hub-spoke.dev.yml up -d --build
#
# The host binary must be glibc-compatible with the container's Debian trixie
# (glibc 2.41). Most recent Linux distros (Arch, Fedora 41+, Ubuntu 25.04+) work.
# If yours doesn't, use the full target or build statically with musl.
services:
  flotilla-base:
    build:
      target: dev
```

- [ ] **Step 2: Commit**

```bash
git add tests/integration/docker-compose.hub-spoke.dev.yml
git commit -m "feat: add hub-spoke dev compose override"
```

---

### Task 4: Write the hub-spoke fixture and basic connectivity tests

**Files:**
- Create: `tests/integration/test_hub_spoke_topology.py`

This is the largest task. The fixture sets up the entire 3-node topology. We write it alongside the first two tests to validate the fixture works.

- [ ] **Step 1: Write the test file with fixture and first two tests**

```python
"""3-node hub-spoke topology tests (Issue #287).

Workstation (leader) peers with homelab-1 (codex/shpool) and homelab-2
(gemini/no shpool).  Tests run CLI commands and validate via JSON output.
"""

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
                f"daemon ready on {n}",
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
```

- [ ] **Step 2: Verify syntax**

```bash
cd tests/integration && python -c "import ast; ast.parse(open('test_hub_spoke_topology.py').read()); print('OK')"
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add tests/integration/test_hub_spoke_topology.py
git commit -m "feat: hub-spoke fixture and topology shape tests"
```

---

### Task 5: Add provider heterogeneity test

**Files:**
- Modify: `tests/integration/test_hub_spoke_topology.py`

- [ ] **Step 1: Append test to the file**

Add after the existing tests:

```python
def test_provider_heterogeneity(hub_spoke_topology):
    """homelab-1 and homelab-2 have different coding agents and tools."""
    hl1 = _flotilla_json("workstation", "host homelab-1 providers")
    hl2 = _flotilla_json("workstation", "host homelab-2 providers")

    # Both should be connected
    assert hl1["connection_status"] == "Connected"
    assert hl2["connection_status"] == "Connected"

    hl1_providers = hl1["summary"]["providers"]
    hl2_providers = hl2["summary"]["providers"]

    # homelab-1 has codex (cloud_agent)
    hl1_agents = [p for p in hl1_providers if p["category"] == "cloud_agent"]
    assert any(p["name"] == "Codex" for p in hl1_agents), (
        f"homelab-1 should have Codex provider, got: {hl1_agents}"
    )

    # homelab-2 has no coding agent provider (gemini binary installed but no
    # flotilla provider factory exists for it yet)
    hl2_agents = [p for p in hl2_providers if p["category"] == "cloud_agent"]
    assert len(hl2_agents) == 0, (
        f"homelab-2 should have no cloud_agent providers, got: {hl2_agents}"
    )

    # homelab-1 has shpool in inventory
    hl1_binaries = [b["name"] for b in hl1["summary"]["inventory"]["binaries"]]
    assert "shpool" in hl1_binaries, (
        f"homelab-1 should have shpool binary, got: {hl1_binaries}"
    )

    # homelab-2 does NOT have shpool
    hl2_binaries = [b["name"] for b in hl2["summary"]["inventory"]["binaries"]]
    assert "shpool" not in hl2_binaries, (
        f"homelab-2 should not have shpool binary, got: {hl2_binaries}"
    )
```

- [ ] **Step 2: Verify syntax**

```bash
cd tests/integration && python -c "import ast; ast.parse(open('test_hub_spoke_topology.py').read()); print('OK')"
```

- [ ] **Step 3: Commit**

```bash
git add tests/integration/test_hub_spoke_topology.py
git commit -m "feat: add provider heterogeneity test for hub-spoke"
```

---

### Task 6: Add work correlation and leader data propagation tests

**Files:**
- Modify: `tests/integration/test_hub_spoke_topology.py`

- [ ] **Step 1: Append correlation test**

```python
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
```

- [ ] **Step 2: Append leader data propagation test**

```python
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
```

- [ ] **Step 3: Verify syntax**

```bash
cd tests/integration && python -c "import ast; ast.parse(open('test_hub_spoke_topology.py').read()); print('OK')"
```

- [ ] **Step 4: Commit**

```bash
git add tests/integration/test_hub_spoke_topology.py
git commit -m "feat: add work correlation and leader data propagation tests"
```

---

### Task 7: Add session persistence tests

**Files:**
- Modify: `tests/integration/test_hub_spoke_topology.py`

- [ ] **Step 1: Append shpool session persistence test**

```python
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
```

- [ ] **Step 2: Append no-shpool session test**

```python
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
    registry_before = _docker_exec(
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
```

- [ ] **Step 3: Verify syntax**

```bash
cd tests/integration && python -c "import ast; ast.parse(open('test_hub_spoke_topology.py').read()); print('OK')"
```

- [ ] **Step 4: Commit**

```bash
git add tests/integration/test_hub_spoke_topology.py
git commit -m "feat: add session persistence tests (shpool vs no-shpool)"
```

---

### Task 8: Add workspace transfer and resilience tests

**Files:**
- Modify: `tests/integration/test_hub_spoke_topology.py`

- [ ] **Step 1: Append workspace transfer test**

```python
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
    set_id_1 = prepared1.get("attachable_set_id")

    # Second prepare-terminal on same checkout (simulates workspace transfer)
    prepared2 = _flotilla_json(
        "workstation",
        f"host homelab-1 repo /home/flotilla/repo prepare-terminal {checkout_path}",
        timeout=60,
    )
    assert prepared2["status"] == "terminal_prepared"
    set_id_2 = prepared2.get("attachable_set_id")

    # Work item should still track this checkout
    result = _flotilla_json("workstation", "repo /home/flotilla/repo work")
    items = [
        item for item in result["work_items"]
        if item.get("branch") == "feat-workspace-xfer"
        and item.get("host") == "homelab-1"
    ]
    assert len(items) >= 1, f"checkout should still be tracked: {result['work_items']}"
```

- [ ] **Step 2: Append resilience test (must be last)**

```python
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
        import json
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
```

- [ ] **Step 3: Verify syntax**

```bash
cd tests/integration && python -c "import ast; ast.parse(open('test_hub_spoke_topology.py').read()); print('OK')"
```

- [ ] **Step 4: Commit**

```bash
git add tests/integration/test_hub_spoke_topology.py
git commit -m "feat: add workspace transfer and resilience tests"
```

---

### Task 9: End-to-end validation

- [ ] **Step 1: Verify all files are in place**

```bash
ls -la tests/integration/docker-compose.hub-spoke*.yml
ls -la tests/integration/test_hub_spoke_topology.py
```

Expected: 3 files (hub-spoke compose, hub-spoke dev compose, test file)

- [ ] **Step 2: Verify test collection**

```bash
cd tests/integration && python -m pytest --collect-only test_hub_spoke_topology.py
```

Expected: 9 tests collected (test_all_daemons_running, test_topology_shows_star_shape, test_provider_heterogeneity, test_work_correlation_across_three_hosts, test_followers_receive_leader_data, test_session_persistence_with_shpool, test_session_without_shpool, test_workspace_transfer, test_resilience_kill_restart)

- [ ] **Step 3: Run the hub-spoke tests (requires Docker)**

```bash
cd tests/integration && python -m pytest test_hub_spoke_topology.py -v --timeout=600
```

This will take several minutes (Docker build + topology setup + tests + teardown). Watch for:
- Fixture setup succeeds (base image builds, all 3 containers start, SSH works, daemons ready, peering established)
- All 9 tests pass

- [ ] **Step 4: Final commit if any fixes were needed**

```bash
git add -A tests/integration/
git commit -m "fix: address issues found during hub-spoke test run"
```
