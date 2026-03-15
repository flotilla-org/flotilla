# Integration Test Harness: 2-Node Topology + Pytest

**Date**: 2026-03-15
**Issue**: #286
**Status**: Draft

## Motivation

Docker images exist (#285). CLI query and control commands are implemented. What's missing is the actual test harness — a way to spin up containers, run CLI commands inside them, and assert on the JSON output. This spec covers the pytest harness skeleton and the minimal 2-node topology.

## Design

### Mental Model

node-a is "the user's desktop." All test commands run via `docker compose exec node-a ...`. When tests need to interact with node-b, they do so the same way a real user would — node-a's flotilla daemon peers with node-b over SSH, and CLI commands on node-a report data from both hosts.

There is no SSH from the test runner into containers. The only host→container boundary is `docker compose exec`.

### Peering Direction

Peering is initiator-driven. The host with `hosts.toml` entries spawns SSH connections to the listed peers. In the 2-node topology:

- **node-a** has `hosts.toml` listing node-b → initiates SSH to node-b
- **node-b** has `daemon.toml` with `follower = true` → accepts inbound connections, does not initiate

node-b does not need its own `hosts.toml`. Its daemon listens on its socket, and node-a's SSH tunnel connects to it.

### Dockerfile Build Modes

The base Dockerfile is restructured with a shared `runtime-base` stage and two build targets. The existing cargo-chef multi-stage build is preserved above `runtime-base` and only executes when building the `full` target.

```dockerfile
# === Cargo build stages (only used by 'full' target) ===
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

# === Shared runtime base ===
FROM debian:bookworm-slim AS runtime-base
RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server openssh-client git ca-certificates libssl3 gosu \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir /run/sshd
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

# --- Full build target (default) ---
FROM runtime-base AS full
COPY --from=builder /app/target/release/flotilla /usr/local/bin/flotilla

# --- Dev target: pre-built binary from host ---
FROM runtime-base AS dev
COPY target/release/flotilla /usr/local/bin/flotilla
```

Compose selects the target via `target: dev` or `target: full` (default). A `docker-compose.dev.yml` override sets `target: dev` for fast iteration.

### Docker Compose: 2-Node Topology

Replaces the existing placeholder `docker-compose.yml`.

```yaml
services:
  node-a:
    build:
      context: ../..
      dockerfile: tests/integration/docker/base/Dockerfile
    hostname: node-a
    volumes:
      - shared-keys:/shared-keys

  node-b:
    build:
      context: ../..
      dockerfile: tests/integration/docker/base/Dockerfile
    hostname: node-b
    volumes:
      - shared-keys:/shared-keys

volumes:
  shared-keys:
```

Both nodes use the base image (jumpbox-level). The workstation/follower role images add tooling for provider diversity tests (topology 2, issue #287) — for the minimal topology we only need SSH + flotilla.

Config and repo directories are created inside the containers by the test fixture, not mounted as volumes.

### Docker Compose Dev Override

`docker-compose.dev.yml` — used with `-f docker-compose.yml -f docker-compose.dev.yml`:

```yaml
services:
  node-a:
    build:
      target: dev
  node-b:
    build:
      target: dev
```

### Container Bootstrap

Before tests run, each container needs:

1. **A git repo** to track — tests create one via `git init` inside the container.
2. **Flotilla config** — `hosts.toml` on node-a pointing to node-b (with required `daemon_socket` field), `daemon.toml` on node-b setting `follower = true`.
3. **Daemon running** — started by a pytest fixture after config is written, with a readiness check before proceeding.

The pytest `topology` fixture handles this setup sequence:

1. `docker compose up -d` — starts containers, SSH keys exchange via shared volume
2. Wait for SSH readiness (retry `docker compose exec node-a ssh -o StrictHostKeyChecking=no node-b true`)
3. Create git repos in each container (with git user config for modern git)
4. Write flotilla config files
5. Start flotilla daemons (`flotilla daemon &`)
6. Wait for daemon readiness (`flotilla status --json` succeeds on each node)
7. Add repos via CLI
8. Wait for peering (`flotilla host list --json` until node-b appears connected)
9. Yield to tests
10. `docker compose down -v --remove-orphans` on teardown

### Pytest Structure

```
tests/integration/
├── docker-compose.yml
├── docker-compose.dev.yml       # override: target=dev for pre-built binary
├── docker/
│   ├── base/Dockerfile
│   ├── workstation/Dockerfile   # (existing, for topology 2+)
│   ├── ...
├── conftest.py                  # shared fixtures and helpers
├── test_minimal_topology.py     # 2-node test cases
└── pyproject.toml               # pytest config
```

### pyproject.toml

```toml
[project]
name = "flotilla-integration-tests"
requires-python = ">=3.10"

[tool.pytest.ini_options]
testpaths = ["."]
timeout = 120
```

### Key Fixtures and Helpers

**`conftest.py`:**

```python
import json
import subprocess
import pytest
import time
from pathlib import Path

COMPOSE_DIR = Path(__file__).parent
COMPOSE_FILE = str(COMPOSE_DIR / "docker-compose.yml")


def docker_exec(service: str, cmd: str, timeout: int = 30) -> subprocess.CompletedProcess:
    """Run a command inside a container via docker compose exec."""
    return subprocess.run(
        ["docker", "compose", "-f", COMPOSE_FILE, "exec", "-T", service,
         "bash", "-c", cmd],
        capture_output=True, text=True, timeout=timeout,
    )


def flotilla_json(service: str, args: str, timeout: int = 30) -> dict | list:
    """Run a flotilla CLI command with --json and return parsed output."""
    result = docker_exec(service, f"flotilla {args} --json", timeout=timeout)
    assert result.returncode == 0, (
        f"flotilla {args} failed (rc={result.returncode}):\n"
        f"stdout: {result.stdout}\nstderr: {result.stderr}"
    )
    return json.loads(result.stdout)


def wait_for(predicate, description: str, timeout: int = 60, interval: float = 2.0):
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
```

**`topology` fixture (session-scoped):**

```python
@pytest.fixture(scope="session")
def topology():
    # 1. docker compose up
    subprocess.run(
        ["docker", "compose", "-f", COMPOSE_FILE, "up", "-d", "--build"],
        check=True, timeout=600,
    )
    try:
        # 2. Wait for SSH between nodes
        wait_for(
            lambda: docker_exec(
                "node-a",
                "ssh -o StrictHostKeyChecking=no -o BatchMode=yes node-b true"
            ).returncode == 0,
            "SSH from node-a to node-b",
        )

        # 3. Create git repos (git config needed for modern git)
        for node in ("node-a", "node-b"):
            docker_exec(node, (
                "git config --global user.email test@test.com && "
                "git config --global user.name test && "
                "git init /home/flotilla/repo && "
                "cd /home/flotilla/repo && "
                "git commit --allow-empty -m init"
            ))

        # 4. Write flotilla config
        docker_exec("node-a", "\n".join([
            "mkdir -p ~/.config/flotilla",
            "cat > ~/.config/flotilla/hosts.toml << 'TOML'",
            "[hosts.node-b]",
            'hostname = "node-b"',
            'expected_host_name = "node-b"',
            'daemon_socket = "/home/flotilla/.config/flotilla/flotilla.sock"',
            "TOML",
        ]))
        docker_exec("node-b", "\n".join([
            "mkdir -p ~/.config/flotilla",
            "cat > ~/.config/flotilla/daemon.toml << 'TOML'",
            "follower = true",
            "TOML",
        ]))

        # 5. Start daemons (backgrounded, will be reparented to PID 1)
        for node in ("node-a", "node-b"):
            docker_exec(node, "nohup flotilla daemon > /tmp/flotilla.log 2>&1 &")

        # 6. Wait for daemon readiness on each node
        for node in ("node-a", "node-b"):
            wait_for(
                lambda n=node: docker_exec(n, "flotilla status --json").returncode == 0,
                f"daemon ready on {node}",
                timeout=30,
            )

        # 7. Add repos
        for node in ("node-a", "node-b"):
            result = docker_exec(node, "flotilla repo add /home/flotilla/repo")
            assert result.returncode == 0, f"repo add failed on {node}: {result.stderr}"

        # 8. Wait for peering
        def peers_connected():
            result = flotilla_json("node-a", "host list")
            return any(
                h["host"] == "node-b"
                and h["connection_status"] == "Connected"
                for h in result.get("hosts", [])
            )

        wait_for(peers_connected, "node-b connected to node-a")

        yield {"node-a": "node-a", "node-b": "node-b"}
    finally:
        # Capture daemon logs for debugging before teardown
        for node in ("node-a", "node-b"):
            result = docker_exec(node, "cat /tmp/flotilla.log")
            if result.stdout:
                print(f"\n=== {node} daemon log ===\n{result.stdout}")

        subprocess.run(
            ["docker", "compose", "-f", COMPOSE_FILE, "down", "-v",
             "--remove-orphans"],
            timeout=60,
        )
```

### JSON Response Shapes Reference

From `crates/flotilla-protocol/src/query.rs` — the actual field names tests must use:

**`HostName`**: `#[serde(transparent)]` — serializes as a plain string, e.g. `"node-b"`

**`PeerConnectionState`**: enum — `"Connected"`, `"Disconnected"`, `"Connecting"`, `"Reconnecting"`, or `{"Rejected": {"reason": "..."}}`

**`StatusResponse`**: `{ "repos": [{ "path": "...", "slug": "...", "provider_health": {...}, "work_item_count": N, "error_count": N }] }`

**`HostListResponse`**: `{ "hosts": [{ "host": "node-b", "is_local": false, "configured": true, "connection_status": "Connected", "has_summary": true, "repo_count": N, "work_item_count": N }] }`

**`TopologyResponse`**: `{ "local_host": "node-a", "routes": [{ "target": "node-b", "next_hop": "node-b", "direct": true, "connected": true, "fallbacks": [] }] }`

**`HostProvidersResponse`**: `{ "host": "node-b", "is_local": false, "configured": true, "connection_status": "Connected", "summary": { "providers": [...], "inventory": [...] } }`

### Test Cases

All tests receive the `topology` fixture and run commands on node-a.

```python
def test_both_daemons_running(topology):
    """Both daemons respond to status."""
    for node in (topology["node-a"], topology["node-b"]):
        result = flotilla_json(node, "status")
        assert "repos" in result


def test_host_list_shows_peer(topology):
    """node-a sees node-b in host list."""
    result = flotilla_json(topology["node-a"], "host list")
    hosts = result["hosts"]
    peer = next(h for h in hosts if h["host"] == "node-b")
    assert peer["connection_status"] == "Connected"
    assert not peer["is_local"]
    assert peer["configured"]


def test_topology_shows_two_nodes(topology):
    """Topology shows both nodes with a direct route."""
    result = flotilla_json(topology["node-a"], "topology")
    assert result["local_host"] == "node-a"
    routes = result["routes"]
    node_b_route = next(r for r in routes if r["target"] == "node-b")
    assert node_b_route["direct"]
    assert node_b_route["connected"]


def test_host_providers(topology):
    """Can query node-b's providers from node-a."""
    result = flotilla_json(topology["node-a"], "host node-b providers")
    assert result["host"] == "node-b"
    assert "summary" in result
    # summary contains providers and inventory from the peer's HostSummary
    summary = result["summary"]
    assert "providers" in summary or "inventory" in summary


def test_host_status(topology):
    """Can query node-b's status from node-a."""
    result = flotilla_json(topology["node-a"], "host node-b status")
    assert result["host"] == "node-b"
    assert result["connection_status"] == "Connected"


def test_status_includes_repos(topology):
    """Status reflects repos from local host."""
    result = flotilla_json(topology["node-a"], "status")
    assert len(result["repos"]) >= 1


def test_repo_on_peer_visible_via_host_status(topology):
    """node-b has a repo tracked, visible from node-a via host status."""
    result = flotilla_json(topology["node-a"], "host node-b status")
    assert result["repo_count"] >= 1
```

### Test Isolation

All tests share the same session-scoped containers and daemon state. The initial test cases are read-only queries, so ordering doesn't matter. Future test authors adding mutating tests (e.g., `repo add` a second repo) should be aware that state carries across tests within a session.

### Dev Workflow

```bash
# Build flotilla on host (Linux only, or cross-compile)
cargo build --release

# Use dev compose override (copies host binary instead of building in Docker)
cd tests/integration
docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d --build

# Run tests
pytest -v

# Full build (works on any platform, slower)
docker compose up -d --build
pytest -v
```

### What's Deferred

- **`watch --json` tests** — needs the watch fix first (separate work in progress)
- **Remote control command tests** (e.g., `host node-b repo add`) — remote command routing works in socket daemon mode but can be added incrementally
- **Hub-spoke topology** (#287) and **jump box topology** (#288) — separate issues, same harness
- **CI integration** — add to GitHub Actions once the harness is stable locally
