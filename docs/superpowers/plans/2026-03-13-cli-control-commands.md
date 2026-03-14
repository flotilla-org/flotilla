# CLI Control Commands Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement shared routed control commands for CLI/TUI/frontends, covering refresh, repo add/remove, and checkout create/remove with host-aware routing and streamed command lifecycle events.

**Architecture:** Evolve `flotilla_protocol::Command` into a host-aware routed envelope with structured selectors and checkout targets. Route every mutating command through `DaemonHandle::execute`, letting the daemon default the host, resolve repo/checkout selectors, execute locally or forward to peers, and emit one consistent lifecycle (`CommandStarted` / `CommandStepUpdate` / `CommandFinished`) for every frontend.

**Tech Stack:** Rust 2024, Tokio, serde, clap, async-trait, existing Flotilla daemon/client/peer protocol crates

---

## File Map

- Modify: `crates/flotilla-protocol/src/commands.rs`
  Shared routed command envelope, selectors, checkout target, command result variants.
- Modify: `crates/flotilla-protocol/src/lib.rs`
  `DaemonEvent` command lifecycle payloads and protocol re-exports.
- Modify: `crates/flotilla-protocol/src/peer.rs`
  Routed peer command request/response messages for remote forwarding.
- Modify: `crates/flotilla-core/src/daemon.rs`
  Unify daemon API around `execute(command)` instead of special mutating RPC methods.
- Modify: `crates/flotilla-core/src/resolve.rs`
  Reuse and extend selector resolution for repos and checkouts.
- Modify: `crates/flotilla-core/src/executor.rs`
  Convert routed commands into resolved local execution, including `CheckoutTarget::Branch` and `CheckoutTarget::FreshBranch`.
- Modify: `crates/flotilla-core/src/in_process.rs`
  Local command routing, refresh-all support, event emission, and result propagation.
- Modify: `crates/flotilla-client/src/lib.rs`
  Socket client request/response path for routed commands.
- Modify: `crates/flotilla-daemon/src/server.rs`
  Socket dispatch for the unified execute path and remote command forwarding integration.
- Modify: `crates/flotilla-daemon/src/peer/transport.rs`
  Transport traits if command forwarding needs sender/request helpers beyond generic `PeerWireMessage`.
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
  Route peer command requests and responses, maintain reverse paths, and surface replies back to the server.
- Modify: `crates/flotilla-daemon/src/peer/channel_tests.rs`
  Coverage for routed command forwarding over the in-memory peer network.
- Modify: `src/main.rs`
  CLI grammar for control commands and `host <host>` prefix.
- Modify: `crates/flotilla-tui/src/cli.rs`
  Control-command execution helpers, progress rendering, and final human/JSON output.
- Modify: `crates/flotilla-tui/src/app/executor.rs`
  TUI command dispatch adapted to the routed command envelope.
- Modify: `crates/flotilla-tui/src/app/intent.rs`
  Existing TUI command builders updated to the new command shape.
- Test: `crates/flotilla-core/tests/in_process_daemon.rs`
  Local daemon lifecycle, selector resolution, refresh-all, and simple-command execution tests.
- Test: `crates/flotilla-daemon/tests/socket_roundtrip.rs`
  Socket round-trip coverage for the unified execute path.

## Chunk 1: Protocol Shape

### Task 1: Add failing protocol tests for routed commands

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs`
- Modify: `crates/flotilla-protocol/src/peer.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Write failing serde tests for the new command envelope**

Add tests covering:

```rust
Command {
    host: Some(HostName::new("feta")),
    action: CommandAction::Refresh { repo: Some(RepoSelector::Query("flotilla".into())) },
}

Command {
    host: None,
    action: CommandAction::Checkout {
        repo: RepoSelector::Path(PathBuf::from("/repo")),
        target: CheckoutTarget::FreshBranch("feat-x".into()),
    },
}
```

Also add result tests for `RepoAdded`, `RepoRemoved`, `Refreshed`, and `CheckoutRemoved`.

- [ ] **Step 2: Write failing peer-wire tests for routed command request/response**

Add tests for a new `RoutedPeerMessage` pair that carries:

```rust
CommandRequest { request_id, requester_host, target_host, remaining_hops, command }
CommandResponse { request_id, requester_host, responder_host, remaining_hops, result }
```

- [ ] **Step 3: Run targeted protocol tests to verify failure**

Run: `cargo test -p flotilla-protocol --locked command_roundtrip`
Expected: FAIL because the new command shape and peer messages do not exist yet.

- [ ] **Step 4: Implement the protocol types**

Make these changes:

- convert `Command` from a plain enum to a routed envelope
- introduce `CommandAction`, `RepoSelector`, `CheckoutSelector`, and `CheckoutTarget`
- remove CLI-facing `create_branch` from the shared protocol
- add new `CommandResult` variants for repo/refresh/checkout removal success
- extend `RoutedPeerMessage` with command request/response variants
- update `DaemonEvent` command payloads to include target host context

- [ ] **Step 5: Run targeted protocol tests to verify pass**

Run: `cargo test -p flotilla-protocol --locked`
Expected: PASS for command, peer, and daemon-event round-trip coverage.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/commands.rs crates/flotilla-protocol/src/lib.rs crates/flotilla-protocol/src/peer.rs
git commit -m "refactor: route protocol commands through host-aware envelope"
```

## Chunk 2: Local Daemon Routing

### Task 2: Unify local control execution under `execute(command)`

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs`
- Modify: `crates/flotilla-core/src/resolve.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-client/src/lib.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Test: `crates/flotilla-core/tests/in_process_daemon.rs`
- Test: `crates/flotilla-daemon/tests/socket_roundtrip.rs`

- [ ] **Step 1: Write failing local-daemon tests for the unified execution path**

Add tests covering:

- `execute(Command { action: Refresh { repo: None }, .. })` refreshes all tracked repos
- `execute(Command { action: AddRepo { .. }, .. })` emits `CommandStarted` and `CommandFinished`
- `execute(Command { action: RemoveRepo { repo: RepoSelector::Query(...) }, .. })` resolves by query
- `execute(Command { action: Checkout { target: CheckoutTarget::Branch(..) }, .. })` fails if branch does not exist
- `execute(Command { action: Checkout { target: CheckoutTarget::FreshBranch(..) }, .. })` fails if branch already exists

- [ ] **Step 2: Run the failing core tests**

Run: `cargo test -p flotilla-core --test in_process_daemon --locked`
Expected: FAIL because `DaemonHandle::execute` still requires a separate repo path and simple commands bypass lifecycle events.

- [ ] **Step 3: Implement daemon API and local resolver changes**

Make these changes:

- change `DaemonHandle::execute` to accept only the routed `Command`
- remove CLI use of special `refresh`, `add_repo`, and `remove_repo` RPC methods
- extend `resolve.rs` with checkout resolution helpers
- add a resolved local-execution form inside core for concrete repo/checkout context
- teach `InProcessDaemon` to default host, resolve selectors, and execute all control commands through one lifecycle
- keep fast internal behavior where useful, but always emit the shared command events
- update socket client/server request handling to use only the unified execute path for control commands

- [ ] **Step 4: Run targeted daemon tests to verify pass**

Run: `cargo test -p flotilla-core --test in_process_daemon --locked`
Expected: PASS for lifecycle, resolution, and local execution coverage.

- [ ] **Step 5: Extend socket round-trip coverage**

Add a socket test proving a simple command such as `repo add` or `refresh all` uses:

- `execute(command)`
- `CommandStarted`
- `CommandFinished`

- [ ] **Step 6: Run socket tests**

Run: `cargo test -p flotilla-daemon --test socket_roundtrip --locked`
Sandbox alternative:
`mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests socket_roundtrip`

Expected: PASS, or skip cleanly in sandbox environments that forbid socket bind.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/daemon.rs crates/flotilla-core/src/resolve.rs crates/flotilla-core/src/executor.rs crates/flotilla-core/src/in_process.rs crates/flotilla-client/src/lib.rs crates/flotilla-daemon/src/server.rs crates/flotilla-core/tests/in_process_daemon.rs crates/flotilla-daemon/tests/socket_roundtrip.rs
git commit -m "refactor: unify local control commands under execute"
```

## Chunk 3: Peer Command Forwarding

### Task 3: Forward routed commands across peer daemons

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/transport.rs`
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-protocol/src/peer.rs`
- Test: `crates/flotilla-daemon/src/peer/channel_tests.rs`

- [ ] **Step 1: Write failing channel-transport tests for command forwarding**

Add tests for:

- routed command request from `host-a` to `host-c` through `host-b`
- multi-step progress and final `CommandResponse` reaching the requester
- reverse-path cleanup on response delivery
- clear error when target host is unknown or disconnected

- [ ] **Step 2: Run peer channel tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked channel_tests`
Expected: FAIL because the peer protocol does not forward commands yet.

- [ ] **Step 3: Implement peer routing for command request/response**

Make these changes:

- extend the peer manager to route command requests like other routed peer messages
- preserve reverse-path information so command responses return to the originator
- integrate forwarded command execution with the local daemon/server execution path
- proxy final results and any lifecycle updates back to the requester

- [ ] **Step 4: Run peer channel tests to verify pass**

Run: `cargo test -p flotilla-daemon --locked channel_tests`
Expected: PASS for direct and relayed command forwarding.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer/transport.rs crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server.rs crates/flotilla-protocol/src/peer.rs crates/flotilla-daemon/src/peer/channel_tests.rs
git commit -m "feat: forward routed commands across peer daemons"
```

## Chunk 4: CLI And TUI Integration

### Task 4: Expose the routed control grammar and wait for completion

**Files:**
- Modify: `src/main.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`
- Modify: `crates/flotilla-tui/src/app/executor.rs`
- Modify: `crates/flotilla-tui/src/app/intent.rs`
- Test: `crates/flotilla-daemon/tests/socket_roundtrip.rs`

- [ ] **Step 1: Write failing parsing/output tests for the new control grammar**

Add coverage for:

- `flotilla refresh`
- `flotilla refresh my-repo`
- `flotilla repo add /tmp/repo`
- `flotilla repo remove owner/repo`
- `flotilla repo owner/repo checkout feature/x`
- `flotilla repo owner/repo checkout --fresh feature/x`
- `flotilla checkout feature/x remove`
- `flotilla host feta repo add /tmp/repo`

Also add CLI output tests for:

- human progress lines for checkout operations
- final success summary in human mode
- final structured payload in `--json`

- [ ] **Step 2: Run targeted CLI tests to verify failure**

Run: `cargo test -p flotilla-tui --locked cli`
Expected: FAIL because the clap grammar and CLI execution helpers do not exist yet.

- [ ] **Step 3: Implement the CLI and TUI command builders**

Make these changes:

- add the control-command clap grammar in `src/main.rs`
- introduce a shared CLI helper in `crates/flotilla-tui/src/cli.rs` that:
  - sends `execute(command)`
  - subscribes to daemon events
  - filters by `command_id`
  - renders human progress or JSON output
- adapt TUI command construction to the new `CommandAction` shape
- remove direct TUI use of legacy `refresh` / `add_repo` / `remove_repo` methods

- [ ] **Step 4: Run targeted CLI tests to verify pass**

Run: `cargo test -p flotilla-tui --locked cli`
Expected: PASS for parser and output coverage.

- [ ] **Step 5: Run a focused workspace verification pass**

Run: `cargo test --workspace --locked`
Sandbox alternative:
`mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`

Expected: PASS, with socket-bind tests skipped only in sandbox mode.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs crates/flotilla-tui/src/cli.rs crates/flotilla-tui/src/app/executor.rs crates/flotilla-tui/src/app/intent.rs crates/flotilla-daemon/tests/socket_roundtrip.rs
git commit -m "feat: add routed cli control commands"
```

## Chunk 5: Final Validation And Cleanup

### Task 5: Verify behavior and document residual follow-ups

**Files:**
- Modify: `docs/superpowers/specs/2026-03-13-cli-control-commands-design.md`
- Modify: `docs/superpowers/plans/2026-03-13-cli-control-commands.md`

- [ ] **Step 1: Run formatter and lint checks**

Run: `cargo fmt --check`
Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: PASS, or only intentionally accepted pre-existing issues documented before commit.

- [ ] **Step 2: Manually smoke-test the CLI locally**

Run examples:

```bash
cargo run -- status
cargo run -- refresh
cargo run -- repo add /path/to/repo
cargo run -- repo owner/repo checkout feature/x
```

Expected: commands parse, execute, and print sensible progress / completion output.

- [ ] **Step 3: Update docs if implementation diverges from the approved spec**

Only touch the spec/plan if the code proves a design assumption wrong. Keep changes narrow and factual.

- [ ] **Step 4: Commit any final doc corrections**

```bash
git add docs/superpowers/specs/2026-03-13-cli-control-commands-design.md docs/superpowers/plans/2026-03-13-cli-control-commands.md
git commit -m "docs: align cli control command docs with implementation"
```
