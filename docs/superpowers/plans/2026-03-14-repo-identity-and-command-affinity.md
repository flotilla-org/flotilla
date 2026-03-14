# Repo Identity And Command Affinity Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-key routed repo execution around `RepoIdentity`, fix provider-backed action routing, and preserve repo affinity through remote terminal-prep follow-up.

**Architecture:** Carry `RepoIdentity` through protocol selectors and daemon events, make daemon and TUI repo state identity-keyed while retaining host-local paths as metadata, restrict remote routing to true execution-host actions, and include originating repo identity in terminal-prep results so async follow-up commands stay bound to the right repo.

**Tech Stack:** Rust, Tokio, serde protocol types, existing daemon/client split, ratatui TUI model state, multi-host daemon integration tests.

---

## File Structure

- Modify: `crates/flotilla-protocol/src/commands.rs`
  - Add `RepoSelector::Identity` and repo-identity-bearing `CommandResult::TerminalPrepared`.
- Modify: `crates/flotilla-protocol/src/lib.rs`
  - Add repo identity to daemon event variants and snapshot/delta/repo metadata as needed.
- Modify: `crates/flotilla-protocol/src/snapshot.rs`
  - Re-key snapshot payloads around repo identity while retaining path metadata.
- Modify: `crates/flotilla-core/src/daemon.rs`
  - Re-key replay API to use `RepoIdentity`.
- Modify: `crates/flotilla-core/src/in_process.rs`
  - Make tracked repo state and command resolution identity-first.
- Modify: `crates/flotilla-core/src/executor.rs`
  - Emit terminal-prep results with originating repo identity.
- Modify: `crates/flotilla-daemon/src/server.rs`
  - Preserve repo identity through routed command lifecycle and remote responses.
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
  - Add differing-root routed integration coverage.
- Modify: `crates/flotilla-daemon/tests/socket_roundtrip.rs`
  - Update event assertions for repo identity payloads.
- Modify: `crates/flotilla-tui/src/app/mod.rs`
  - Re-key repo/tab/in-flight state and restrict item-host routing.
- Modify: `crates/flotilla-tui/src/app/executor.rs`
  - Consume identity-based terminal-prep results and queue correct follow-up commands.
- Modify: `crates/flotilla-tui/src/app/intent.rs`
  - Build routed repo commands with repo identity, not active local path.
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
  - Re-key per-repo UI state by repo identity.
- Modify: `crates/flotilla-tui/src/cli.rs`
  - Update command-event rendering for repo identity-bearing lifecycle events.
- Modify: `crates/flotilla-daemon/src/client.rs`
  - Re-key replay sequence tracking by repo identity.
- Modify: affected tests under `crates/flotilla-core/tests`, `crates/flotilla-protocol/src`, and `crates/flotilla-tui/tests`
  - Cover identity selection, routing semantics, and repo-affinity preservation.

## Chunk 1: Protocol Identity Plumbing

### Task 1: Add failing protocol tests for identity selectors and lifecycle events

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-protocol/src/snapshot.rs`

- [ ] **Step 1: Write failing protocol roundtrip tests**

Cover:
- `RepoSelector::Identity(RepoIdentity)` serde roundtrip
- `CommandResult::TerminalPrepared` carrying repo identity
- daemon events roundtripping repo identity alongside path metadata
- repo snapshots and deltas retaining both identity and path

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-protocol --locked repo_identity -- --nocapture`
Expected: FAIL because the selector and event payloads do not carry identity yet.

- [ ] **Step 3: Implement minimal protocol changes**

Add:
- `RepoSelector::Identity`
- repo identity fields on repo-bearing protocol types
- terminal-prep result repo identity

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-protocol --locked repo_identity -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/commands.rs crates/flotilla-protocol/src/lib.rs crates/flotilla-protocol/src/snapshot.rs
git commit -m "feat: carry repo identity through protocol"
```

## Chunk 2: Daemon Identity-First Resolution

### Task 2: Add failing daemon tests for identity-based repo resolution

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/daemon.rs`

- [ ] **Step 1: Write failing daemon tests**

Cover:
- `execute()` accepts `RepoSelector::Identity`
- command lifecycle events emit repo identity with path metadata
- replay bookkeeping keys off repo identity rather than path
- local path/query selectors continue to resolve for local-only flows

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon repo_identity -- --nocapture`
Expected: FAIL because repo tracking and lifecycle events are still path-keyed.

- [ ] **Step 3: Implement minimal daemon re-keying**

Update:
- tracked repo maps
- repo order
- replay-since API / bookkeeping
- command resolution helpers
- repo removal / refresh / lifecycle emission

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon repo_identity -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/daemon.rs crates/flotilla-core/src/in_process.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "refactor: key daemon repo state by identity"
```

### Task 3: Add failing multi-host tests for differing repo roots

**Files:**
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`

- [ ] **Step 1: Write failing multi-host routed tests**

Cover:
- leader and follower track the same repo identity at different absolute roots
- remote checkout creation still succeeds
- remote branch-name generation still succeeds
- remote terminal preparation still succeeds

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked differing_repo_roots -- --nocapture`
Expected: FAIL because routed commands still rely on matching paths.

- [ ] **Step 3: Implement minimal routed identity support**

Update routed command handling so:
- remote commands preserve `RepoSelector::Identity`
- responder lifecycle events include identity and local responder path metadata
- requester-side result forwarding preserves identity

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-daemon --locked differing_repo_roots -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/tests/multi_host.rs
git commit -m "feat: route multi-host repo commands by identity"
```

## Chunk 3: TUI Re-Keying And Command Affinity

### Task 4: Add failing TUI tests for identity-keyed repo state

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`

- [ ] **Step 1: Write failing TUI model tests**

Cover:
- snapshots for the same repo identity but different local paths update one repo model
- repo tabs and per-repo UI state remain keyed by identity
- command lifecycle display uses event identity/path payloads correctly

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-tui --locked repo_identity -- --nocapture`
Expected: FAIL because repo and UI maps are still keyed by `PathBuf`.

- [ ] **Step 3: Implement minimal TUI re-keying**

Update:
- `TuiModel.repos`
- `repo_order`
- `UiState.repo_ui`
- active repo helpers
- client replay tracking
- repo add/remove/snapshot/delta handlers

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-tui --locked repo_identity -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/ui_state.rs crates/flotilla-tui/src/cli.rs crates/flotilla-daemon/src/client.rs
git commit -m "refactor: key tui repo state by identity"
```

### Task 5: Add failing TUI tests for command-affinity fixes

**Files:**
- Modify: `crates/flotilla-tui/src/app/intent.rs`
- Modify: `crates/flotilla-tui/src/app/executor.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`

- [ ] **Step 1: Write failing tests for routing semantics**

Cover:
- routed checkout / branch-name / terminal-prep commands use repo identity selectors
- provider-backed item actions stay on the presentation host
- execution-host item actions still route remotely when appropriate
- terminal-prep follow-up uses originating repo identity, not current active repo

- [ ] **Step 2: Run targeted tests to verify failure**

Run: `cargo test -p flotilla-tui --locked command_affinity -- --nocapture`
Expected: FAIL because current item routing and terminal follow-up still use the wrong host/repo source.

- [ ] **Step 3: Implement minimal routing fixes**

Update:
- intent builders to use identity selectors
- item-host helpers to only remote-route true execution-host actions
- in-flight command tracking / result handling so `TerminalPrepared` queues follow-up for the original repo

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p flotilla-tui --locked command_affinity -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/intent.rs crates/flotilla-tui/src/app/executor.rs crates/flotilla-tui/src/app/mod.rs
git commit -m "fix: preserve repo and host affinity for routed commands"
```

## Chunk 4: Branch Verification And PR Update

### Task 6: Run focused verification for the corrected branch

**Files:**
- No code changes expected

- [ ] **Step 1: Run focused crate tests**

Run:
- `cargo test -p flotilla-protocol --locked`
- `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`
- `cargo test -p flotilla-daemon --locked --test multi_host`
- `cargo test -p flotilla-tui --locked`

Expected: PASS.

- [ ] **Step 2: Run repo-wide verification**

Run:
- `cargo +nightly fmt --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- `git diff --check`

Expected: PASS.

- [ ] **Step 3: Commit final cleanup if needed**

```bash
git add -A
git commit -m "chore: finalize repo identity routing fixes"
```

- [ ] **Step 4: Update PR #334**

Push the branch and update the PR description to call out:
- `#298` folded into the branch
- cross-host repo-root mismatch fixed
- provider-backed item routing corrected
- terminal-prep repo-affinity corrected
