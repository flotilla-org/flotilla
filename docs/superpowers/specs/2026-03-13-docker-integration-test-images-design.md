# Docker Integration Test Images

**Date**: 2026-03-13
**Status**: Draft
**Issue**: #285

## Motivation

Multi-host integration tests need containers that simulate realistic deployment topologies — different hostnames, heterogeneous tool installations, SSH connectivity. These images provide the building blocks that compose files (Issues 6-8) assemble into test topologies.

## File Layout

```
tests/integration/
├── docker/
│   ├── base/
│   │   └── Dockerfile
│   ├── workstation/
│   │   └── Dockerfile
│   ├── follower-codex/
│   │   └── Dockerfile
│   ├── follower-gemini/
│   │   └── Dockerfile
│   ├── jumpbox/
│   │   └── Dockerfile
│   └── entrypoint.sh
├── docker-compose.yml          # placeholder, topologies defined in Issues 6-8
└── .dockerignore
```

Build context is the repo root (Dockerfiles need access to the full Rust workspace). The `.dockerignore` lives at repo root.

## Base Image

`tests/integration/docker/base/Dockerfile`

Multi-stage build with three stages:

### Stage 1 — Chef (dependency cache)

```dockerfile
FROM rust:slim-bookworm AS chef
RUN cargo install cargo-chef
WORKDIR /app
```

### Stage 2 — Planner

```dockerfile
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src/main.rs src/main.rs
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json
```

The planner copies manifests and source so `cargo chef prepare` can analyze the full crate graph. This layer changes rarely.

### Stage 3 — Builder

```dockerfile
FROM chef AS builder

# Cook dependencies (cached unless Cargo.lock changes)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Build flotilla (only this layer rebuilds on source changes)
COPY . .
RUN cargo build --release
```

### Stage 4 — Runtime

```dockerfile
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    openssh-client \
    git \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# SSH server setup
RUN mkdir /run/sshd

# Create flotilla user
RUN useradd -m -s /bin/bash flotilla
RUN mkdir -p /home/flotilla/.ssh && chown flotilla:flotilla /home/flotilla/.ssh

# Copy flotilla binary from builder
COPY --from=builder /app/target/release/flotilla /usr/local/bin/flotilla

# Shared entrypoint
COPY tests/integration/docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22
ENTRYPOINT ["/entrypoint.sh"]
CMD ["sleep", "infinity"]
```

Note: `cargo-binstall` is not included in the base runtime image. Role images that need it use their own builder stage to keep the base image lean.

## Entrypoint Script

`tests/integration/docker/entrypoint.sh`

Runs as root at container startup:

1. Generate SSH host keys if not present (`ssh-keygen -A`)
2. Generate user SSH keypair if not present (`/home/flotilla/.ssh/id_ed25519`)
3. Copy public key to `/shared-keys/<hostname>.pub` (shared volume)
4. Build `/home/flotilla/.ssh/authorized_keys` from all keys currently in `/shared-keys/`
5. Start a background loop that periodically refreshes `authorized_keys` from `/shared-keys/` (handles containers that start later)
6. Fix ownership on `~flotilla/.ssh/`
7. Start `sshd` in the background
8. Drop to `flotilla` user and `exec "$@"` (allows compose `command:` override)

The `/shared-keys/` directory is a Docker volume shared across all containers in a topology, enabling mutual SSH trust without pre-baked keys.

The background refresh loop solves the startup race condition — containers that start later will have their keys picked up within a few seconds. Test fixtures should still retry SSH connections with a short timeout to handle the initial sync window.

## Role Images

All role images are `FROM flotilla-base` and add role-specific tools. Role images that need `cargo-binstall` use a multi-stage pattern:

```dockerfile
FROM rust:slim-bookworm AS installer
RUN cargo install cargo-binstall
RUN cargo binstall --no-confirm zellij shpool

FROM flotilla-base
COPY --from=installer /usr/local/cargo/bin/zellij /usr/local/bin/zellij
COPY --from=installer /usr/local/cargo/bin/shpool /usr/local/bin/shpool
```

This keeps `cargo-binstall` and the Rust toolchain out of the final image.

### workstation

Installs:
- `gh` (GitHub CLI) — from official apt repository
- `claude` CLI — official installer or npm
- `tmux` — apt
- `zellij` — cargo-binstall (via builder stage)
- `shpool` — cargo-binstall (via builder stage)

Full provider complement. Runs as leader (default mode).

### follower-codex

Installs:
- `codex` CLI — npm
- `shpool` — cargo-binstall (via builder stage)
- Placeholder `~/.codex/auth.json` — required for `CodexAuthDetector` to report codex as available (the detector checks for this file, not just the binary)

No `gh`, no `tmux`/`zellij`. Intended for follower mode (configured at runtime).

### follower-gemini

Installs:
- `gemini` CLI — npm or binary

No `shpool`, no `gh`. Intended for follower mode.

### jumpbox

No additional tools. Base image only — SSH relay. Intended for follower mode with no providers beyond SSH connectivity.

## Code Change: Gemini Detector

No gemini detector exists in the codebase. This issue includes adding one to `default_host_detectors()` in `crates/flotilla-core/src/providers/discovery/detectors/mod.rs`:

```rust
Box::new(CommandDetector::new("gemini", &["--version"], parse_first_dotted_version)),
```

This follows the existing `CommandDetector` pattern used for git, gh, zellij, and shpool. Without this, the `follower-gemini` image would have gemini installed but flotilla would not discover it.

## Runtime Configuration

**Follower mode is a runtime concern, not an image concern.** The `daemon.toml` setting (`follower = true`) and `hosts.toml` peer configuration are injected via:
- Mounted config files from the compose definition
- Or generated by the entrypoint based on environment variables

This keeps images reusable across topologies.

## Build

Build context is the repo root. Images are built with:

```bash
docker build -f tests/integration/docker/base/Dockerfile -t flotilla-base .
docker build -f tests/integration/docker/workstation/Dockerfile -t flotilla-workstation .
# etc.
```

Or via `docker compose build` when compose files are added (Issues 6-8).

### .dockerignore

At repo root. Uses negation to re-include the entrypoint script from within the excluded `tests/` tree:

```
target/
.git/
docs/
tests/
!tests/integration/docker/
*.md
.github/
.vscode/
```

### Layer Caching

`cargo-chef` separates dependency compilation from source compilation via a dedicated planner stage. The flow:

1. **Planner** stage copies only manifests, runs `cargo chef prepare` to generate a dependency recipe
2. **Builder** stage copies the recipe, runs `cargo chef cook` to build all dependencies (cached unless `Cargo.lock` changes)
3. **Builder** stage copies full source, runs `cargo build` (only this layer rebuilds on source changes)

This significantly speeds up iterative development — changing flotilla source does not recompile dependencies.

## Coding Agent Installation

Coding agents (claude, codex, gemini) are real installations, not stubs. This diverges from the parent spec (which describes stubs) — the decision was made to install real binaries so that future tests can exercise agent functionality beyond provider discovery. Auth configuration is handled at test runtime when needed — the images ensure the binaries are present and discoverable.

For codex specifically, a placeholder `~/.codex/auth.json` is created in the image because `CodexAuthDetector` checks for the auth file, not the binary.

## Provider Discovery Compatibility

Flotilla discovers tools via `HostDetector` implementations. The table below shows detectors relevant to the test images (omitting Cursor and cmux detectors which are not exercised by any role image):

| Detector | What it looks for | Role images with this tool |
|----------|-------------------|---------------------------|
| `CommandDetector("git")` | `git` on PATH | all (in base) |
| `CommandDetector("gh")` | `gh` on PATH | workstation |
| `ClaudeDetector` | `claude` on PATH or `~/.claude/local/claude` | workstation |
| `CodexAuthDetector` | `~/.codex/auth.json` file | follower-codex |
| `CommandDetector("gemini")` | `gemini` on PATH (new, added by this issue) | follower-gemini |
| `CommandDetector("shpool")` | `shpool` on PATH | workstation, follower-codex |
| `EnvVarDetector("TMUX")` | `$TMUX` set (inside tmux session) | workstation (runtime) |
| `EnvVarDetector("ZELLIJ")` | `$ZELLIJ` set (inside zellij session) | workstation (runtime) |
| `CommandDetector("zellij")` | `zellij` on PATH | workstation |

Role images ensure the right binaries/files are present so discovery reports the expected provider set per host. Session manager env vars (`TMUX`, `ZELLIJ`) are only set when actually inside a session, so detection depends on binary presence + runtime state.

## What This Issue Does NOT Cover

- Compose topology definitions (Issues 6-8)
- Pytest harness (Issue 6)
- CLI commands used by tests (Issues 1-4)
- Actual integration test cases
