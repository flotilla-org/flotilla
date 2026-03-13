# Docker Integration Test Images Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build Docker base image + four role images for multi-host integration testing.

**Architecture:** Multi-stage Rust build (cargo-chef for layer caching) produces a `flotilla-base` image with the flotilla binary, SSH server, and git. Four role images layer on top: `workstation` (full tools), `follower-codex` (codex + shpool), `follower-gemini` (gemini), `jumpbox` (bare SSH). A shared entrypoint handles SSH key exchange via a shared volume.

**Tech Stack:** Docker multi-stage builds, cargo-chef, Debian bookworm-slim, cargo-binstall, bash

**Spec:** `docs/superpowers/specs/2026-03-13-docker-integration-test-images-design.md`

---

## Chunk 1: Gemini Detector + Infrastructure Files

### Task 1: Add gemini CommandDetector

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/detectors/mod.rs`

- [ ] **Step 1: Add gemini to the table-driven command detector test**

In the `simple_command_detectors_are_table_driven` test in `crates/flotilla-core/src/providers/discovery/detectors/mod.rs`, add a new entry to the `cases` array:

```rust
            ("gemini", "gemini", "gemini 1.0.0\n", Some("1.0.0")),
```

Add this after the `("shpool", ...)` line (line 75).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core simple_command_detectors_are_table_driven`
Expected: FAIL — the test now has 6 cases but the detector list still has 5 command detectors. The test iterates cases independently so the gemini case will pass (it constructs its own detector), but we should verify it runs.

Actually — this test constructs detectors directly per case, so it will pass. The real validation is that the detector is registered in `default_host_detectors()`. Instead, verify the test passes (confirming the pattern works for gemini), then add the detector to the default list.

Run: `cargo test -p flotilla-core simple_command_detectors_are_table_driven`
Expected: PASS (each case constructs its own detector)

- [ ] **Step 3: Add gemini detector to default_host_detectors()**

In `crates/flotilla-core/src/providers/discovery/detectors/mod.rs`, in the `default_host_detectors()` function, add after the shpool line (line 23):

```rust
        Box::new(CommandDetector::new("gemini", &["--version"], parse_first_dotted_version)),
```

- [ ] **Step 4: Run all detector tests**

Run: `cargo test -p flotilla-core -- detectors`
Expected: PASS

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo +nightly fmt && cargo clippy -p flotilla-core --all-targets --locked -- -D warnings`
Expected: Clean

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/detectors/mod.rs
git commit -m "feat: add gemini CommandDetector to default host detectors"
```

### Task 2: Create .dockerignore

**Files:**
- Create: `.dockerignore`

- [ ] **Step 1: Create .dockerignore at repo root**

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

- [ ] **Step 2: Verify the negation pattern is correct**

Run: `docker build --check -f /dev/null . 2>&1 | head -5 || echo "docker check not available, manual verify"`

Visual check: the file should exclude `tests/` but re-include `tests/integration/docker/` for the entrypoint script.

- [ ] **Step 3: Commit**

```bash
git add .dockerignore
git commit -m "chore: add .dockerignore for integration test image builds"
```

### Task 3: Create entrypoint script

**Files:**
- Create: `tests/integration/docker/entrypoint.sh`

- [ ] **Step 1: Create the entrypoint script**

```bash
#!/usr/bin/env bash
set -euo pipefail

FLOTILLA_USER="flotilla"
FLOTILLA_HOME="/home/${FLOTILLA_USER}"
SHARED_KEYS_DIR="/shared-keys"
SSH_DIR="${FLOTILLA_HOME}/.ssh"
HOSTNAME=$(hostname)

# --- SSH host keys ---
ssh-keygen -A

# --- User keypair ---
if [ ! -f "${SSH_DIR}/id_ed25519" ]; then
    ssh-keygen -t ed25519 -f "${SSH_DIR}/id_ed25519" -N "" -q
fi

# --- Share public key ---
if [ -d "${SHARED_KEYS_DIR}" ]; then
    cp "${SSH_DIR}/id_ed25519.pub" "${SHARED_KEYS_DIR}/${HOSTNAME}.pub"
fi

# --- Build authorized_keys from shared keys ---
refresh_authorized_keys() {
    if [ -d "${SHARED_KEYS_DIR}" ]; then
        cat "${SHARED_KEYS_DIR}"/*.pub > "${SSH_DIR}/authorized_keys" 2>/dev/null || true
        chmod 600 "${SSH_DIR}/authorized_keys"
    fi
}

refresh_authorized_keys

# --- Background refresh loop (pick up late-starting peers) ---
(
    while true; do
        sleep 5
        refresh_authorized_keys
    done
) &

# --- Fix ownership ---
chown -R "${FLOTILLA_USER}:${FLOTILLA_USER}" "${SSH_DIR}"

# --- Start sshd ---
/usr/sbin/sshd

# --- Drop to flotilla user and exec CMD ---
exec gosu "${FLOTILLA_USER}" "$@"
```

- [ ] **Step 2: Verify script is syntactically valid**

Run: `bash -n tests/integration/docker/entrypoint.sh`
Expected: No output (valid syntax)

- [ ] **Step 3: Commit**

```bash
git add tests/integration/docker/entrypoint.sh
git commit -m "feat: add shared entrypoint script for SSH key exchange"
```

## Chunk 2: Base Image

### Task 4: Create base Dockerfile

**Files:**
- Create: `tests/integration/docker/base/Dockerfile`

- [ ] **Step 1: Create the base Dockerfile**

```dockerfile
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

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    openssh-client \
    git \
    ca-certificates \
    libssl3 \
    gosu \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir /run/sshd

# Permit pubkey auth, disable password auth
RUN sed -i 's/#PubkeyAuthentication yes/PubkeyAuthentication yes/' /etc/ssh/sshd_config \
    && sed -i 's/#PasswordAuthentication yes/PasswordAuthentication no/' /etc/ssh/sshd_config

RUN useradd -m -s /bin/bash flotilla \
    && mkdir -p /home/flotilla/.ssh \
    && chmod 700 /home/flotilla/.ssh \
    && chown flotilla:flotilla /home/flotilla/.ssh

COPY --from=builder /app/target/release/flotilla /usr/local/bin/flotilla
COPY tests/integration/docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22
ENTRYPOINT ["/entrypoint.sh"]
CMD ["sleep", "infinity"]
```

- [ ] **Step 2: Build the base image**

Run from repo root:

```bash
docker build -f tests/integration/docker/base/Dockerfile -t flotilla-base .
```

Expected: Successful build. This will take a while on first run (compiling Rust deps). Subsequent builds with only source changes should reuse the `cook` layer.

- [ ] **Step 3: Smoke-test the base image**

Run: `docker run --rm flotilla-base flotilla --help`
Expected: Flotilla help output (confirms binary is installed and runnable)

Run: `docker run --rm flotilla-base git --version`
Expected: `git version ...`

Run: `docker run --rm flotilla-base id flotilla`
Expected: `uid=...(flotilla) gid=...`

- [ ] **Step 4: Commit**

```bash
git add tests/integration/docker/base/Dockerfile
git commit -m "feat: add base Dockerfile with multi-stage cargo-chef build"
```

## Chunk 3: Role Images

### Task 5: Create workstation Dockerfile

**Files:**
- Create: `tests/integration/docker/workstation/Dockerfile`

- [ ] **Step 1: Create the workstation Dockerfile**

```dockerfile
# --- Builder stage: install tools via cargo-binstall ---
FROM rust:slim-bookworm AS installer
RUN cargo install cargo-binstall
RUN cargo binstall --no-confirm zellij shpool

# --- Runtime: layer on flotilla-base ---
FROM flotilla-base

# tmux via apt
RUN apt-get update && apt-get install -y --no-install-recommends \
    tmux \
    curl \
    gnupg \
    && rm -rf /var/lib/apt/lists/*

# gh (GitHub CLI) — official apt repo
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    | tee /etc/apt/sources.list.d/github-cli.list > /dev/null \
    && apt-get update \
    && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# claude CLI — install via npm
RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
    && npm install -g @anthropic-ai/claude-code \
    && rm -rf /var/lib/apt/lists/*

# zellij + shpool from builder stage
COPY --from=installer /usr/local/cargo/bin/zellij /usr/local/bin/zellij
COPY --from=installer /usr/local/cargo/bin/shpool /usr/local/bin/shpool
```

- [ ] **Step 2: Build the workstation image**

Run from repo root:

```bash
docker build -f tests/integration/docker/workstation/Dockerfile -t flotilla-workstation .
```

Expected: Successful build.

- [ ] **Step 3: Smoke-test tool availability**

```bash
docker run --rm flotilla-workstation gh --version
docker run --rm flotilla-workstation claude --version
docker run --rm flotilla-workstation tmux -V
docker run --rm flotilla-workstation zellij --version
docker run --rm flotilla-workstation shpool version
```

Expected: Each prints a version string without error.

- [ ] **Step 4: Commit**

```bash
git add tests/integration/docker/workstation/Dockerfile
git commit -m "feat: add workstation role Dockerfile"
```

### Task 6: Create follower-codex Dockerfile

**Files:**
- Create: `tests/integration/docker/follower-codex/Dockerfile`

- [ ] **Step 1: Create the follower-codex Dockerfile**

```dockerfile
# --- Builder stage: install shpool via cargo-binstall ---
FROM rust:slim-bookworm AS installer
RUN cargo install cargo-binstall
RUN cargo binstall --no-confirm shpool

# --- Runtime: layer on flotilla-base ---
FROM flotilla-base

# codex CLI via npm
RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
    && npm install -g @openai/codex \
    && rm -rf /var/lib/apt/lists/*

# Placeholder codex auth file for CodexAuthDetector
RUN mkdir -p /home/flotilla/.codex \
    && echo '{"auth_mode":"placeholder"}' > /home/flotilla/.codex/auth.json \
    && chown -R flotilla:flotilla /home/flotilla/.codex

# shpool from builder stage
COPY --from=installer /usr/local/cargo/bin/shpool /usr/local/bin/shpool
```

- [ ] **Step 2: Build the follower-codex image**

Run from repo root:

```bash
docker build -f tests/integration/docker/follower-codex/Dockerfile -t flotilla-follower-codex .
```

Expected: Successful build.

- [ ] **Step 3: Smoke-test tool availability**

```bash
docker run --rm flotilla-follower-codex codex --version
docker run --rm flotilla-follower-codex shpool version
docker run --rm flotilla-follower-codex sh -c 'cat /home/flotilla/.codex/auth.json'
```

Expected: codex prints version, shpool prints version, auth.json contents shown.

- [ ] **Step 4: Verify gh is NOT available (not installed in this role)**

```bash
docker run --rm flotilla-follower-codex which gh
```

Expected: Non-zero exit code (not found).

- [ ] **Step 5: Commit**

```bash
git add tests/integration/docker/follower-codex/Dockerfile
git commit -m "feat: add follower-codex role Dockerfile"
```

### Task 7: Create follower-gemini Dockerfile

**Files:**
- Create: `tests/integration/docker/follower-gemini/Dockerfile`

- [ ] **Step 1: Create the follower-gemini Dockerfile**

```dockerfile
FROM flotilla-base

# gemini CLI via npm
RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
    && npm install -g @google/gemini-cli \
    && rm -rf /var/lib/apt/lists/*

- [ ] **Step 2: Build the follower-gemini image**

Run from repo root:

```bash
docker build -f tests/integration/docker/follower-gemini/Dockerfile -t flotilla-follower-gemini .
```

Expected: Successful build.

- [ ] **Step 3: Smoke-test tool availability**

```bash
docker run --rm flotilla-follower-gemini gemini --version
```

Expected: Prints version string.

- [ ] **Step 4: Verify gh and shpool are NOT available**

```bash
docker run --rm flotilla-follower-gemini which gh || true
docker run --rm flotilla-follower-gemini which shpool || true
```

Expected: Both not found.

- [ ] **Step 5: Commit**

```bash
git add tests/integration/docker/follower-gemini/Dockerfile
git commit -m "feat: add follower-gemini role Dockerfile"
```

### Task 8: Create jumpbox Dockerfile

**Files:**
- Create: `tests/integration/docker/jumpbox/Dockerfile`

- [ ] **Step 1: Create the jumpbox Dockerfile**

```dockerfile
FROM flotilla-base

# Jumpbox: no additional tools beyond base (SSH + git + flotilla)
```

- [ ] **Step 2: Build the jumpbox image**

Run from repo root:

```bash
docker build -f tests/integration/docker/jumpbox/Dockerfile -t flotilla-jumpbox .
```

Expected: Successful build (trivial — just the base image).

- [ ] **Step 3: Smoke-test**

```bash
docker run --rm flotilla-jumpbox flotilla --help
docker run --rm flotilla-jumpbox which gh || echo "not found (expected)"
docker run --rm flotilla-jumpbox which claude || echo "not found (expected)"
```

Expected: flotilla help works, gh and claude not found.

- [ ] **Step 4: Commit**

```bash
git add tests/integration/docker/jumpbox/Dockerfile
git commit -m "feat: add jumpbox role Dockerfile"
```

## Chunk 4: End-to-End Verification

### Task 9: SSH key exchange integration test

- [ ] **Step 1: Create a minimal docker-compose for SSH testing**

Create `tests/integration/docker-compose.yml`:

```yaml
# Placeholder compose file. Full topologies defined in Issues 6-8.
# This minimal setup validates base image SSH key exchange works.
services:
  node-a:
    build:
      context: ../..
      dockerfile: tests/integration/docker/jumpbox/Dockerfile
    hostname: node-a
    volumes:
      - shared-keys:/shared-keys

  node-b:
    build:
      context: ../..
      dockerfile: tests/integration/docker/jumpbox/Dockerfile
    hostname: node-b
    volumes:
      - shared-keys:/shared-keys

volumes:
  shared-keys:
```

- [ ] **Step 2: Build and start the two-node cluster**

```bash
cd tests/integration && docker compose up -d --build
```

Expected: Both containers start.

- [ ] **Step 3: Wait for SSH readiness and test connectivity**

All commands run from repo root.

```bash
# Wait for keys to sync
sleep 10

# Test SSH from node-a to node-b
docker compose -f tests/integration/docker-compose.yml exec node-a \
  ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 flotilla@node-b "hostname"
```

Expected: Prints `node-b`.

- [ ] **Step 4: Test reverse direction**

```bash
docker compose -f tests/integration/docker-compose.yml exec node-b \
  ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 flotilla@node-a "hostname"
```

Expected: Prints `node-a`.

- [ ] **Step 5: Test flotilla binary is available via SSH**

```bash
docker compose -f tests/integration/docker-compose.yml exec node-a \
  ssh -o StrictHostKeyChecking=no flotilla@node-b "flotilla --help"
```

Expected: Flotilla help output.

- [ ] **Step 6: Tear down**

```bash
docker compose -f tests/integration/docker-compose.yml down -v
```

- [ ] **Step 7: Commit compose file**

```bash
git add tests/integration/docker-compose.yml
git commit -m "feat: add placeholder docker-compose with SSH key exchange validation"
```

### Task 10: Final verification

- [ ] **Step 1: Run cargo tests to confirm gemini detector doesn't break anything**

```bash
cargo test --locked
```

Expected: All tests pass.

- [ ] **Step 2: Run clippy and fmt**

```bash
cargo +nightly fmt && cargo clippy --all-targets --locked -- -D warnings
```

Expected: Clean.

- [ ] **Step 3: Verify all images build cleanly from scratch**

```bash
docker build -f tests/integration/docker/base/Dockerfile -t flotilla-base .
docker build -f tests/integration/docker/workstation/Dockerfile -t flotilla-workstation .
docker build -f tests/integration/docker/follower-codex/Dockerfile -t flotilla-follower-codex .
docker build -f tests/integration/docker/follower-gemini/Dockerfile -t flotilla-follower-gemini .
docker build -f tests/integration/docker/jumpbox/Dockerfile -t flotilla-jumpbox .
```

Expected: All build successfully.
