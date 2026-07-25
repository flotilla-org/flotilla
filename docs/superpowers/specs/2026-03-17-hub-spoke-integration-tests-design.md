# Hub-Spoke Integration Tests (Issue #287)

## Summary

Add a 3-node hub-spoke topology integration test suite: 1 workstation + 2 followers (homelab-1 with codex/shpool, homelab-2 with gemini/no shpool). Tests provider heterogeneity, work correlation across 3 hosts, session persistence differences, and resilience.

## Topology

```
                    [homelab-1: follower-codex, shpool]
                   /
[workstation] ←SSH→
                   \
                    [homelab-2: follower-gemini, no shpool]
```

Single Docker network (default compose network). All three containers can reach each other at the network level, but flotilla config only peers followers with the workstation — the star shape is enforced by configuration, not network isolation. This matches real-world hub-spoke setups where the network is flat but peering is selective.

## Changes to Existing Files

### `conftest.py` — parameterize compose file

`docker_exec` and `flotilla_json` currently hardcode `COMPOSE_FILE` to `docker-compose.yml`. Refactor `docker_exec` to accept an optional `compose_file` parameter (defaulting to `COMPOSE_FILE` for backwards compatibility). `flotilla_json` and `wait_for` pass it through. The 2-node tests are unaffected.

## New Files

### `tests/integration/docker-compose.hub-spoke.yml`

Four services: a build-only `flotilla-base` service plus three role services.

**Build orchestration:** The role Dockerfiles (`docker/workstation/Dockerfile`, `docker/follower-codex/Dockerfile`, `docker/follower-gemini/Dockerfile`) all use `FROM flotilla-base`. Docker Compose `depends_on` only controls startup order, not build order — so `FROM flotilla-base` will fail if the base image doesn't exist. The fixture must explicitly build the base image first:

```
docker compose -f docker-compose.hub-spoke.yml build flotilla-base
docker compose -f docker-compose.hub-spoke.yml up -d --build
```

The compose file declares:

```yaml
services:
  flotilla-base:
    build:
      context: ../..
      dockerfile: tests/integration/docker/base/Dockerfile
      target: full
    image: flotilla-base  # tags the built image so role Dockerfiles can FROM it

  workstation:
    build:
      context: ../..
      dockerfile: tests/integration/docker/workstation/Dockerfile
    hostname: workstation
    volumes:
      - shared-keys:/shared-keys
    depends_on:
      flotilla-base:
        condition: service_started

  homelab-1:
    build:
      context: ../..
      dockerfile: tests/integration/docker/follower-codex/Dockerfile
    hostname: homelab-1
    volumes:
      - shared-keys:/shared-keys
    depends_on:
      flotilla-base:
        condition: service_started

  homelab-2:
    build:
      context: ../..
      dockerfile: tests/integration/docker/follower-gemini/Dockerfile
    hostname: homelab-2
    volumes:
      - shared-keys:/shared-keys
    depends_on:
      flotilla-base:
        condition: service_started

volumes:
  shared-keys:
```

The `flotilla-base` service exists only to produce the image. It starts and immediately exits (no CMD override needed — it just needs to build).

### `tests/integration/docker-compose.hub-spoke.dev.yml`

Dev override for pre-built binary from host (same pattern as existing `docker-compose.dev.yml`). Overrides the base image build target to `dev`.

### `tests/integration/test_hub_spoke_topology.py`

All hub-spoke tests plus a `hub_spoke_topology` session-scoped fixture.

## Fixture: `hub_spoke_topology`

1. Build base image: `docker compose -f <hub-spoke-compose> build flotilla-base`
2. Start topology: `docker compose -f <hub-spoke-compose> up -d --build`
3. Wait for SSH: workstation→homelab-1 and workstation→homelab-2
4. Git init a repo on each node (`/home/flotilla/repo`)
5. Write flotilla config:
   - **workstation** `hosts.toml`:
     ```toml
     [hosts.homelab-1]
     hostname = "homelab-1"
     expected_host_name = "homelab-1"
     daemon_socket = "/home/flotilla/.config/flotilla/flotilla.sock"

     [hosts.homelab-2]
     hostname = "homelab-2"
     expected_host_name = "homelab-2"
     daemon_socket = "/home/flotilla/.config/flotilla/flotilla.sock"
     ```
   - **homelab-1** `daemon.toml`: `follower = true`
   - **homelab-2** `daemon.toml`: `follower = true`
6. Start daemons on all 3 nodes (backgrounded via `nohup`)
7. Wait for daemon readiness on each node (`flotilla status --json`)
8. Add repos via CLI on each node (`flotilla repo add /home/flotilla/repo`)
9. Wait for both followers to show `Connected` from workstation's perspective
10. Yield `{"workstation": "workstation", "homelab-1": "homelab-1", "homelab-2": "homelab-2"}`
11. Teardown: print daemon logs from all 3 nodes, `docker compose down -v --remove-orphans`

## Test Cases

Tests are ordered so that mutating tests (resilience) run last. The resilience test restores the topology to a healthy state before returning. pytest-ordering or explicit naming conventions ensure this.

### `test_topology_shows_star_shape`

From workstation, `flotilla topology --json` shows direct routes to both homelab-1 and homelab-2. Neither homelab appears as a hop for the other (star, not chain).

### `test_provider_heterogeneity`

- `flotilla host homelab-1 providers --json` on workstation shows codex as a coding agent provider and shpool in the tool inventory.
- `flotilla host homelab-2 providers --json` on workstation shows gemini as a coding agent provider and no shpool in the tool inventory.
- The provider lists differ between the two followers.

### `test_work_correlation_across_three_hosts`

Create a checkout on branch `feat-correlated` on homelab-1. Create a checkout on branch `feat-correlated` on homelab-2. Wait for both to be visible on workstation. Verify workstation's `flotilla repo /home/flotilla/repo work --json` shows work items from both hosts attributed correctly (host field matches origin). Because the checkouts share a branch name, the correlation engine should merge them into a single work item (or group) — verify this.

### `test_followers_receive_leader_data`

Create a checkout on workstation. Wait for it to propagate. From homelab-1, verify the workstation's checkout is visible (via work items or host status query). This confirms that peer data broadcast from leader to followers works — followers see workstation-originated data without needing to query it themselves.

Note: PR/issue data propagation would require real `gh` auth which isn't available in Docker. The checkout propagation test is the meaningful verification here.

### `test_session_persistence_with_shpool`

1. Create a checkout on homelab-1
2. From workstation, run `host homelab-1 repo /home/flotilla/repo prepare-terminal <path>`
3. Verify the response includes `attachable_set_id` and the attachables registry exists on homelab-1
4. Kill the homelab-1 daemon, restart it
5. Verify the attachable set id is still present (shpool sessions persist across daemon restarts)

### `test_session_without_shpool`

1. Create a checkout on homelab-2
2. From workstation, run `host homelab-2 repo /home/flotilla/repo prepare-terminal <path>`
3. Verify terminal is prepared (response has `status: terminal_prepared`)
4. Kill the homelab-2 daemon, restart it
5. Verify the previous session state is gone (no shpool to persist it)

### `test_workspace_transfer`

1. From workstation, prepare a terminal on homelab-1 using the default workspace manager
2. Verify an attachable set exists
3. Prepare a terminal on the same checkout path but request a different workspace manager (if the CLI supports this) or verify that work item continuity is maintained when the workspace changes
4. Verify the work item still tracks the checkout regardless of which workspace manager owns the terminal

Note: The full "disconnect tmux, respawn in zellij" scenario depends on the workspace manager CLI surface. If the CLI doesn't support switching workspace managers, this test verifies the simpler case of terminal re-preparation maintaining work item identity.

### `test_resilience_kill_restart` (runs last)

1. Create a checkout `feat-resilience` on homelab-1
2. Wait for it to be visible on workstation
3. Kill the workstation daemon process (`pkill -f 'flotilla daemon'`)
4. Poll homelab-1: verify it detects disconnection (workstation no longer shows as Connected). Disconnection detection depends on heartbeat timeouts — allow up to 30s for detection.
5. Restart workstation daemon on workstation node
6. Wait for both followers to reconnect (both show Connected, allow up to 60s for reconnect + resync)
7. Verify `feat-resilience` checkout on homelab-1 is still visible from workstation (data resync confirmed)

## Unchanged

- Existing Docker role images (`docker/workstation/`, `docker/follower-codex/`, `docker/follower-gemini/`) used directly
- `test_minimal_topology.py` untouched
- `docker-compose.yml` (2-node) untouched

## Dependencies

- #286 (2-node minimal topology) — done, provides the pattern and shared helpers
