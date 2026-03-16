# Integration Test Harness Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a pytest-based integration test harness that spins up a 2-node Docker topology and validates multi-host peering via flotilla's CLI JSON output.

**Architecture:** Two Docker containers (node-a as leader, node-b as follower) peer over SSH. Tests run commands on node-a via `docker compose exec`. The base Dockerfile gains a multi-target build (full cargo build vs dev pre-built binary). A session-scoped pytest fixture manages the full lifecycle: compose up, SSH readiness, config, daemon start, peering, teardown.

**Tech Stack:** Docker, Docker Compose, Python 3.10+, pytest

**Spec:** `docs/superpowers/specs/2026-03-15-integration-test-harness-design.md`

---

## File Structure

| Action | Path | Responsibility |
|--------|------|----------------|
| Modify | `tests/integration/docker/base/Dockerfile` | Add `runtime-base`, `full`, and `dev` build targets |
| Modify | `tests/integration/docker-compose.yml` | Replace placeholder with 2-node topology using base image |
| Create | `tests/integration/docker-compose.dev.yml` | Override to select `dev` target for pre-built binary |
| Create | `tests/integration/pyproject.toml` | Pytest config and Python version |
| Create | `tests/integration/conftest.py` | Shared fixtures: `docker_exec`, `flotilla_json`, `wait_for`, `topology` |
| Create | `tests/integration/test_minimal_topology.py` | Test cases for 2-node peering |

---

## Chunk 1: Docker Infrastructure

### Task 1: Add build targets to base Dockerfile

The existing Dockerfile has a single build path. We restructure it so the cargo-chef stages feed into a `full` target, while a `dev` target copies a pre-built binary. Both share a `runtime-base` stage.

**Files:**
- Modify: `tests/integration/docker/base/Dockerfile`

- [ ] **Step 1: Rewrite the Dockerfile with multi-target support**

The existing file is at `tests/integration/docker/base/Dockerfile`. Replace its contents:

```dockerfile
# === Cargo build stages (only executed when building 'full' target) ===
FROM rust:slim-bookworm AS chef
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release

# === Shared runtime base (used by both full and dev) ===
FROM debian:bookworm-slim AS runtime-base

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    openssh-client \
    git \
    ca-certificates \
    libssl3 \
    gosu \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir /run/sshd

# Disable password auth (pubkey is already the default)
RUN mkdir -p /etc/ssh/sshd_config.d \
    && echo "PasswordAuthentication no" > /etc/ssh/sshd_config.d/flotilla.conf

RUN useradd -m -s /bin/bash flotilla \
    && mkdir -p /home/flotilla/.ssh \
    && chmod 700 /home/flotilla/.ssh \
    && chown flotilla:flotilla /home/flotilla/.ssh

COPY tests/integration/docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22
ENTRYPOINT ["/entrypoint.sh"]
CMD ["sleep", "infinity"]

# --- Full build target (default): cargo-built binary ---
FROM runtime-base AS full
COPY --from=builder /app/target/release/flotilla /usr/local/bin/flotilla

# --- Dev target: pre-built binary from host ---
FROM runtime-base AS dev
COPY target/release/flotilla /usr/local/bin/flotilla
```

- [ ] **Step 2: Verify the full target builds**

Run: `cd /home/robert/dev/flotilla.multi-host-tests && docker build -f tests/integration/docker/base/Dockerfile --target full -t flotilla-base-test .`

Expected: Image builds successfully (slow — full cargo build inside Docker).

Note: If you have a local `target/release/flotilla` binary, you can skip this and test the dev target instead (much faster):

Run: `docker build -f tests/integration/docker/base/Dockerfile --target dev -t flotilla-base-test .`

Expected: Image builds successfully.

- [ ] **Step 3: Commit**

```bash
git add tests/integration/docker/base/Dockerfile
git commit -m "feat: add dev/full build targets to base Dockerfile

Restructure with shared runtime-base stage. 'full' target does
cargo-chef multi-stage build. 'dev' target copies pre-built binary
for fast iteration."
```

### Task 2: Replace placeholder docker-compose.yml

The existing file uses `jumpbox/Dockerfile` and is a placeholder. Replace with the 2-node topology using the base Dockerfile directly.

**Files:**
- Modify: `tests/integration/docker-compose.yml`

- [ ] **Step 1: Replace docker-compose.yml**

```yaml
services:
  node-a:
    build:
      context: ../..
      dockerfile: tests/integration/docker/base/Dockerfile
      target: full
    hostname: node-a
    volumes:
      - shared-keys:/shared-keys

  node-b:
    build:
      context: ../..
      dockerfile: tests/integration/docker/base/Dockerfile
      target: full
    hostname: node-b
    volumes:
      - shared-keys:/shared-keys

volumes:
  shared-keys:
```

- [ ] **Step 2: Create docker-compose.dev.yml**

```yaml
# Override: use pre-built binary from host instead of cargo build.
# Usage: docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d --build
services:
  node-a:
    build:
      target: dev
  node-b:
    build:
      target: dev
```

- [ ] **Step 3: Verify compose config is valid**

Run: `cd /home/robert/dev/flotilla.multi-host-tests/tests/integration && docker compose config --quiet`

Expected: No errors.

Run: `docker compose -f docker-compose.yml -f docker-compose.dev.yml config --quiet`

Expected: No errors.

- [ ] **Step 4: Commit**

```bash
git add tests/integration/docker-compose.yml tests/integration/docker-compose.dev.yml
git commit -m "feat: 2-node topology compose files with dev override

Replace placeholder compose with node-a (leader) and node-b
(follower) using base Dockerfile. Dev override selects pre-built
binary target for fast iteration."
```

---

## Chunk 2: Pytest Harness

### Task 3: Create pyproject.toml

**Files:**
- Create: `tests/integration/pyproject.toml`

- [ ] **Step 1: Create pyproject.toml**

```toml
[project]
name = "flotilla-integration-tests"
requires-python = ">=3.10"
dependencies = [
    "pytest",
    "pytest-timeout",
]

[tool.pytest.ini_options]
testpaths = ["."]
# Per-test timeout (seconds). Topology fixture has its own internal timeouts.
timeout = 120
```

Install with: `pip install -e tests/integration` or `pip install pytest pytest-timeout`

- [ ] **Step 2: Commit**

```bash
git add tests/integration/pyproject.toml
git commit -m "chore: add pyproject.toml for integration tests"
```

### Task 4: Create conftest.py with helpers and topology fixture

This is the core of the harness. Three helpers (`docker_exec`, `flotilla_json`, `wait_for`) and one session-scoped `topology` fixture that manages the full lifecycle.

**Files:**
- Create: `tests/integration/conftest.py`

**Key design decisions:**
- `COMPOSE_FILE` is an absolute path computed from `conftest.py`'s location, so pytest works from any cwd.
- `docker_exec` uses `-T` (no pseudo-TTY) since there's no terminal.
- The topology fixture: compose up → SSH ready → git repos → config → daemons → readiness → repo add → peering → yield → logs → compose down.
- Daemon logs are captured to `/tmp/flotilla.log` inside each container and printed on teardown for debugging.
- The `peers_connected` predicate uses the correct JSON field names: `h["host"]` and `h["connection_status"] == "Connected"`.

- [ ] **Step 1: Create conftest.py**

```python
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
            "exec", "-T", service, "bash", "-c", cmd,
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
    return json.loads(result.stdout)


def wait_for(
    predicate, description: str, timeout: int = 60, interval: float = 2.0
):
    """Poll until predicate returns True or timeout."""
    deadline = time.monotonic() + timeout
    last_err = None
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
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
        ["docker", "compose", "-f", COMPOSE_FILE, "up", "-d", "--build"],
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
                "nohup flotilla daemon > /tmp/flotilla.log 2>&1 &",
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
        # Print daemon logs for debugging
        for node in ("node-a", "node-b"):
            result = docker_exec(node, "cat /tmp/flotilla.log")
            if result.stdout:
                print(f"\n=== {node} daemon log ===\n{result.stdout}")
            if result.stderr:
                print(f"\n=== {node} daemon stderr ===\n{result.stderr}")

        subprocess.run(
            [
                "docker", "compose", "-f", COMPOSE_FILE,
                "down", "-v", "--remove-orphans",
            ],
            timeout=60,
        )
```

- [ ] **Step 2: Verify conftest.py has no syntax errors**

Run: `python3 -c "import ast; ast.parse(open('tests/integration/conftest.py').read()); print('OK')"`

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add tests/integration/conftest.py
git commit -m "feat: pytest conftest with topology fixture and helpers

Session-scoped fixture manages full lifecycle: compose up, SSH
readiness, git repos, flotilla config, daemon start with readiness
check, repo add, peer connection wait, and teardown with log capture."
```

---

## Chunk 3: Test Cases

### Task 5: Create test_minimal_topology.py

All tests are read-only queries against the session-scoped topology. They use the exact JSON field names from the protocol types in `crates/flotilla-protocol/src/query.rs`.

**JSON shape reference (from protocol types):**

- `HostListEntry`: `host` (string), `is_local` (bool), `configured` (bool), `connection_status` (string: "Connected"|"Disconnected"|...), `has_summary` (bool), `repo_count` (int), `work_item_count` (int)
- `TopologyRoute`: `target` (string), `next_hop` (string), `direct` (bool), `connected` (bool), `fallbacks` (list)
- `TopologyResponse`: `local_host` (string), `routes` (list of TopologyRoute)
- `StatusResponse`: `repos` (list of RepoSummary)
- `HostStatusResponse`: `host` (string), `is_local` (bool), `configured` (bool), `connection_status` (string), `summary` (optional HostSummary), `repo_count` (int), `work_item_count` (int)
- `HostProvidersResponse`: `host` (string), `is_local` (bool), `configured` (bool), `connection_status` (string), `summary` (HostSummary with `providers` and `inventory` fields)

**Files:**
- Create: `tests/integration/test_minimal_topology.py`

- [ ] **Step 1: Create test_minimal_topology.py**

```python
"""2-node minimal topology tests (Issue #286).

All tests run commands on node-a (the "user's desktop") and validate
that multi-host peering with node-b works via the CLI JSON output.
"""

from conftest import docker_exec, flotilla_json


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
    assert peer["connection_status"] == "Connected"
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
    assert result["local_host"] == "node-a"

    routes = result["routes"]
    node_b_route = next(
        (r for r in routes if r["target"] == "node-b"), None
    )
    assert node_b_route is not None, f"no route to node-b: {routes}"
    assert node_b_route["direct"]
    assert node_b_route["connected"]
    assert node_b_route["next_hop"] == "node-b"


def test_host_status_peer(topology):
    """Can query node-b's status from node-a."""
    result = flotilla_json(topology["node-a"], "host node-b status")
    assert result["host"] == "node-b"
    assert result["connection_status"] == "Connected"
    assert not result["is_local"]
    assert result["repo_count"] >= 1


def test_host_providers_peer(topology):
    """Can query node-b's providers from node-a."""
    result = flotilla_json(topology["node-a"], "host node-b providers")
    assert result["host"] == "node-b"
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


def test_peer_repo_visible_via_host_status(topology):
    """node-b's tracked repo is visible from node-a via host status."""
    result = flotilla_json(topology["node-a"], "host node-b status")
    assert result["repo_count"] >= 1
```

- [ ] **Step 2: Verify test file has no syntax errors**

Run: `python3 -c "import ast; ast.parse(open('tests/integration/test_minimal_topology.py').read()); print('OK')"`

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add tests/integration/test_minimal_topology.py
git commit -m "feat: 2-node integration tests for multi-host peering

Tests: both daemons running, host list shows peer, topology shows
direct route, host status/providers queries, repo visibility across
hosts. All read-only queries using correct protocol JSON field names."
```

---

## Chunk 4: Smoke Test

### Task 6: Run the integration tests end-to-end

This task validates everything works together. Use the dev build path if you have a Linux binary, or the full build path otherwise.

**Files:** None (execution only)

- [ ] **Step 1: Build flotilla (for dev target)**

Run: `cd /home/robert/dev/flotilla.multi-host-tests && cargo build --release`

Expected: Build succeeds.

- [ ] **Step 2: Bring up containers with dev target**

Run: `cd /home/robert/dev/flotilla.multi-host-tests/tests/integration && docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d --build`

Expected: Both containers start. If the dev target fails (e.g., binary is for wrong arch), fall back to the full target:

Run: `docker compose up -d --build`

- [ ] **Step 3: Quick manual sanity check**

Run: `docker compose exec -T node-a flotilla --help`

Expected: Flotilla help text appears.

Run: `docker compose exec -T node-a ssh -o StrictHostKeyChecking=no node-b hostname`

Expected: `node-b`

- [ ] **Step 4: Tear down manual test containers**

Run: `docker compose down -v --remove-orphans`

- [ ] **Step 5: Run pytest**

Run: `cd /home/robert/dev/flotilla.multi-host-tests/tests/integration && pytest -v`

Expected: All tests pass. If tests fail, check the daemon log output printed during teardown for clues.

Common failure modes:
- **Timeout waiting for SSH**: entrypoint.sh key exchange may need more time. Increase `wait_for` timeout.
- **Timeout waiting for daemon**: Check `flotilla daemon` actually starts (look at the log). May need a socket path fix.
- **Timeout waiting for peering**: Check `hosts.toml` has correct `daemon_socket` path. Check SSH tunnel can reach node-b's socket.
- **JSON parse error**: A CLI command may be returning human output instead of JSON. Check `--json` flag handling.

- [ ] **Step 6: Fix any issues and re-run**

If tests fail, iterate. The daemon logs printed on teardown are the primary debugging tool. You can also run individual tests:

Run: `pytest -v -k test_host_list_shows_peer`

- [ ] **Step 7: Final commit (if any fixes were needed)**

```bash
git add -u
git commit -m "fix: integration test adjustments from smoke test"
```
