# Remote Repo Identity And Command Affinity Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-key remote repo handling around `RepoIdentity`, fix cross-host routed repo resolution, preserve multi-root local discovery for shared identities, and restore correct command affinity for provider-backed actions and async terminal preparation.

**Architecture:** Introduce repo identity as the stable key across protocol, daemon state, client replay, and TUI tabs. Keep per-host paths and tracked local roots as daemon-local metadata for execution, discovery, and display, and tighten command routing so only genuinely execution-host-owned actions are sent remotely. Where one concrete local path is required, resolve an explicit preferred local instance instead of relying on last-writer-wins path maps.

**Tech Stack:** Rust, Tokio, serde protocol types, existing `DaemonHandle` / socket client / in-process daemon, ratatui TUI state, multi-host daemon tests.

---

## File Structure

- Modify: `crates/flotilla-protocol/src/commands.rs`
  - Add `RepoSelector::Identity` and extend terminal-prep result metadata.
- Modify: `crates/flotilla-protocol/src/snapshot.rs`
  - Add `RepoIdentity` to repo-bearing snapshot/list types.
- Modify: `crates/flotilla-protocol/src/lib.rs`
  - Add repo identity to daemon events and snapshot delta types.
- Modify: `crates/flotilla-core/src/daemon.rs`
  - Change replay bookkeeping trait signatures from path-keyed to identity-keyed.
- Modify: `crates/flotilla-core/src/in_process.rs`
  - Re-key daemon repo state, peer overlays, replay handling, and command resolution by `RepoIdentity` while preserving multiple tracked local roots per identity.
- Modify: `crates/flotilla-core/src/executor.rs`
  - Preserve repo identity through terminal-prep results and local workspace follow-up.
- Modify: `crates/flotilla-daemon/src/server.rs`
  - Ensure routed execution / lifecycle events carry and preserve repo identity.
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
  - Add different-root multi-host coverage for remote checkout replication.
- Modify: `crates/flotilla-client/src/lib.rs`
  - Re-key replay seq tracking by repo identity.
- Modify: `crates/flotilla-tui/src/app/mod.rs`
  - Re-key TUI repo state and in-flight tracking by repo identity.
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
  - Re-key per-tab UI state by repo identity.
- Modify: `crates/flotilla-tui/src/app/intent.rs`
  - Use identity-based selectors for remote execution and restore local routing for provider-backed item actions.
- Modify: `crates/flotilla-tui/src/app/executor.rs`
  - Preserve originating repo identity when handling `TerminalPrepared`.
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
  - Update any direct repo/path command construction to the new selector model.
- Modify: `crates/flotilla-tui/src/cli.rs`
  - Render updated event/result shapes if needed.
- Modify: `crates/flotilla-tui/src/ui.rs`
  - Update repo-order access and any snapshot output assumptions.
- Modify: `crates/flotilla-tui/tests/snapshots/*.snap`
  - Refresh only if intentional UI output changes remain.
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
  - Update replay / event tests for identity-keyed behavior.
- Modify: `crates/flotilla-daemon/tests/socket_roundtrip.rs`
  - Update socket/replay tests for identity-keyed events.
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
  - Update stub repo/event builders to include repo identity.

## Chunk 1: Protocol Identity Plumbing

### Task 1: Add failing protocol tests for identity-bearing repo types

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs`
- Modify: `crates/flotilla-protocol/src/snapshot.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Test: `crates/flotilla-protocol/src/commands.rs`
- Test: `crates/flotilla-protocol/src/snapshot.rs`
- Test: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Write failing tests for `RepoSelector::Identity` and identity-bearing event/snapshot roundtrips**

Cover:
- `RepoSelector::Identity(RepoIdentity)` JSON roundtrip
- `RepoInfo`, `Snapshot`, and `SnapshotDelta` roundtrip with identity present
- `DaemonEvent::{RepoRemoved, CommandStarted, CommandFinished, CommandStepUpdate}` roundtrip with repo identity
- `CommandResult::TerminalPrepared` roundtrip with originating repo identity

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-protocol --locked identity -- --nocapture`
Expected: FAIL because identity-bearing variants/fields do not exist yet.

- [ ] **Step 3: Implement minimal protocol changes**

Add:
- `RepoSelector::Identity`
- `identity: RepoIdentity` fields on repo-bearing structs
- repo identity fields on daemon lifecycle events
- repo identity on `TerminalPrepared`

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-protocol --locked commands::tests:: snapshot::tests:: tests:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/commands.rs crates/flotilla-protocol/src/snapshot.rs crates/flotilla-protocol/src/lib.rs
git commit -m "feat: add repo identity to protocol events and selectors"
```

## Chunk 2: Daemon Re-Keying To RepoIdentity

### Task 2: Add failing core tests for identity-keyed repo resolution and replay

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/daemon.rs`

- [ ] **Step 1: Write failing tests for identity-based repo resolution**

Cover:
- `list_repos()` includes repo identity
- `replay_since()` uses identity-keyed last-seen maps
- `resolve_repo_for_command()` accepts `RepoSelector::Identity`
- command lifecycle events report repo identity consistently
- multiple tracked local clones with the same identity remain discoverable instead of collapsing to one stored path

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-core --locked --features test-support in_process_daemon -- --nocapture`
Expected: FAIL because daemon/client traits and repo state are still path-keyed.

- [ ] **Step 3: Re-key `InProcessDaemon`**

Change:
- `repos`, `repo_order`, and `peer_providers` to `RepoIdentity` keys
- store all daemon-local roots inside `RepoState`, plus an explicit preferred execution instance when one path is required
- make replay bookkeeping and event emission identity-based
- keep path accessors for local execution and display without reintroducing path-keyed indexing

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/daemon.rs crates/flotilla-core/src/in_process.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "refactor: key in-process daemon repos by identity"
```

### Task 3: Add failing daemon routing tests for different local/remote repo roots

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
- Possibly modify: `crates/flotilla-daemon/tests/socket_roundtrip.rs`

- [ ] **Step 1: Write failing tests for routed repo resolution across different roots**

Cover:
- remote checkout command addressed by repo identity works when local and remote repo roots differ
- remote terminal prepare addressed by repo identity works with different roots
- routed lifecycle events preserve repo identity on the way back

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked remote_checkout execute_forwarded_prepare_terminal -- --nocapture`
Expected: FAIL because routed commands still depend on path-based repo lookup.

- [ ] **Step 3: Update server routing and daemon interactions**

Ensure:
- routed commands use `RepoSelector::Identity`
- pending remote command bookkeeping preserves repo identity
- lifecycle events and responses surface the correct repo identity to clients

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-daemon --locked --test multi_host -- --nocapture`
Run: `cargo test -p flotilla-daemon --locked server::tests:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/tests/multi_host.rs crates/flotilla-daemon/tests/socket_roundtrip.rs
git commit -m "fix: route remote repo commands by identity"
```

## Chunk 3: Client And TUI Identity Re-Keying

### Task 4: Add failing client tests for identity-keyed replay bookkeeping

**Files:**
- Modify: `crates/flotilla-client/src/lib.rs`

- [ ] **Step 1: Write failing tests for identity-keyed local seq tracking**

Cover:
- full snapshots seed seq by repo identity
- matching deltas advance seq by repo identity
- gap recovery sends identity-keyed `last_seen`

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-client --locked replay_since handle_event -- --nocapture`
Expected: FAIL because seq maps are still keyed by `PathBuf`.

- [ ] **Step 3: Implement identity-keyed client replay tracking**

Update:
- `SeqMap`
- recovery bookkeeping
- any helper/test builders that fabricate repo-bearing events

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-client --locked -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-client/src/lib.rs
git commit -m "refactor: key client replay state by repo identity"
```

### Task 5: Add failing TUI tests for identity-keyed tabs and terminal-prep affinity

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
- Modify: `crates/flotilla-tui/src/app/executor.rs`
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
- Test: `crates/flotilla-tui/src/app/mod.rs`
- Test: `crates/flotilla-tui/src/app/executor.rs`

- [ ] **Step 1: Write failing tests for identity-stable repo/tab behavior**

Cover:
- repo collections and tab ordering keyed by repo identity
- daemon events update the correct tab even if path differs
- `TerminalPrepared` queues follow-up workspace creation for the initiating repo after tab switch

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-tui --locked terminal_prepared repo_added repo_removed handle_daemon_event -- --nocapture`
Expected: FAIL because TUI state and in-flight tracking are still path-keyed.

- [ ] **Step 3: Re-key TUI model and async result handling**

Update:
- `TuiModel.repos`, `repo_order`, provider status maps, and `UiState.repo_ui` to use `RepoIdentity`
- `TuiRepoModel` to preserve enough local-instance metadata for deterministic display when multiple local roots share one identity
- `InFlightCommand` to store repo identity (and path only if still needed for display)
- `handle_result()` to queue `CreateWorkspaceFromPreparedTerminal` against the originating identity

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-tui --locked app::tests:: app::executor::tests:: app::ui_state::tests:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/ui_state.rs crates/flotilla-tui/src/app/executor.rs crates/flotilla-tui/src/app/test_support.rs
git commit -m "refactor: key tui repo state by identity"
```

## Chunk 4: Command Affinity Fixes

### Task 6: Add failing TUI intent tests for provider-backed action routing

**Files:**
- Modify: `crates/flotilla-tui/src/app/intent.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`

- [ ] **Step 1: Write failing tests for provider-backed actions staying local**

Cover:
- open/close PR stays local when selected item is anchored to a remote checkout
- open issue stays local
- link issues stays local
- archive session stays local
- checkout creation and terminal preparation remain remotely targeted where appropriate

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-tui --locked intent::tests::resolve_open intent::tests::resolve_link intent::tests::resolve_archive -- --nocapture`
Expected: FAIL because these actions are currently routed to `item.host`.

- [ ] **Step 3: Implement command affinity split**

Make:
- provider-backed browser/API actions use presentation-host repo commands
- only execution-host-owned actions use target host or item host routing

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-tui --locked intent::tests:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/intent.rs crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/key_handlers.rs
git commit -m "fix: keep provider-backed item actions on presentation host"
```

## Chunk 5: Final Verification And Snapshot Updates

### Task 7: Update snapshot/test fixtures and run full verification

**Files:**
- Modify: `crates/flotilla-tui/tests/snapshots/*.snap`
- Modify: any repo/event test fixtures affected by identity-bearing protocol changes

- [ ] **Step 1: Refresh snapshots only after logic is stable**

Run: `INSTA_UPDATE=always cargo test -p flotilla-tui --locked --test snapshots`
Expected: PASS with intentional snapshot updates if output changed.

- [ ] **Step 2: Run focused changed-crate verification**

Run: `cargo test -p flotilla-protocol --locked -- --nocapture`
Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`
Run: `cargo test -p flotilla-daemon --locked --test multi_host -- --nocapture`
Run: `cargo test -p flotilla-client --locked -- --nocapture`
Run: `cargo test -p flotilla-tui --locked -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Run repository-wide verification**

Run: `cargo +nightly fmt --check`
Run: `cargo clippy --all-targets --locked -- -D warnings`
Run: `cargo test --workspace --locked`
Run: `git diff --check`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/tests/snapshots docs/superpowers/specs/2026-03-14-remote-repo-identity-and-command-affinity-design.md docs/superpowers/plans/2026-03-14-remote-repo-identity-and-command-affinity.md
git commit -m "docs: capture repo identity routing refactor plan"
```
