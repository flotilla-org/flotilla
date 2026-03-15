# Repo Identity Artifact Cleanup Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove dead path-keyed repo-identity cleanup artifacts while preserving the live identity-keyed daemon, client, and TUI behavior.

**Architecture:** The live peer networking flow stays in `server.rs`; any shared types still needed there move with it, and the unused duplicate implementation in `peer_networking.rs` is removed. Remaining path/identity bridge helpers in `InProcessDaemon` are kept only where they still serve a real boundary between identity-keyed state and path-bearing protocol or filesystem execution.

**Tech Stack:** Rust, Tokio, cargo fmt, clippy, cargo test

---

## File Map

- Modify: `crates/flotilla-daemon/src/lib.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Delete: `crates/flotilla-daemon/src/peer_networking.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-daemon/src/cli.rs` only if module imports need adjustment
- Modify: `crates/flotilla-daemon/tests/*` and `crates/flotilla-core/tests/in_process_daemon.rs` only if helper names or module boundaries change
- Test: `cargo test -p flotilla-daemon --locked`
- Test: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`

## Chunk 1: Remove Dead Peer Networking Stack

### Task 1: Prove `peer_networking.rs` is dead and identify shared pieces

**Files:**
- Inspect: `crates/flotilla-daemon/src/lib.rs`
- Inspect: `crates/flotilla-daemon/src/server.rs`
- Inspect: `crates/flotilla-daemon/src/peer_networking.rs`

- [ ] **Step 1: Verify no live code constructs `PeerNetworkingTask`**

Run: `rg -n "PeerNetworkingTask|peer_networking::PeerNetworkingTask" crates/flotilla-daemon/src crates/flotilla-daemon/tests`

Expected: matches only in `peer_networking.rs`

- [ ] **Step 2: Identify still-used shared types from the dead module**

Run: `rg -n "PeerConnectedNotice" crates/flotilla-daemon/src`

Expected: `server.rs` imports or uses `PeerConnectedNotice`

- [ ] **Step 3: Record the dead-path stale logic that motivated cleanup**

Run: `rg -n "last_sent_versions: HashMap<PathBuf, u64>" crates/flotilla-daemon/src/peer_networking.rs`

Expected: one match in the dead outbound broadcaster

### Task 2: Move shared type(s) into the live server path and delete the dead module

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/src/lib.rs`
- Delete: `crates/flotilla-daemon/src/peer_networking.rs`

- [ ] **Step 1: Write or update the failing compile boundary**

Run: `cargo test -p flotilla-daemon --locked`

Expected: PASS before edits, giving a clean baseline

- [ ] **Step 2: Move `PeerConnectedNotice` to `server.rs` or another live shared location**

Implementation:
- define `PeerConnectedNotice` next to the live peer server task code
- remove the import from `peer_networking`

- [ ] **Step 3: Delete the unused `peer_networking.rs` module and module export**

Implementation:
- remove `pub mod peer_networking;` from `crates/flotilla-daemon/src/lib.rs`
- delete `crates/flotilla-daemon/src/peer_networking.rs`

- [ ] **Step 4: Run daemon tests after module removal**

Run: `cargo test -p flotilla-daemon --locked`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/lib.rs crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/peer_networking.rs
git commit -m "refactor: remove dead peer networking stack"
```

## Chunk 2: Tighten Remaining Path/Identity Bridges

### Task 3: Classify live bridge helpers as real boundary APIs vs leftovers

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`

- [ ] **Step 1: Enumerate current bridge helper call sites**

Run: `rg -n "find_identity_for_path|find_repo_by_identity" crates/flotilla-core crates/flotilla-daemon`

Expected: a concrete list of live uses to classify

- [ ] **Step 2: Separate required boundary uses from historical ones**

Implementation checklist:
- keep path-to-identity resolution where live protocol/events still arrive keyed by tracked path
- keep identity-to-preferred-path resolution where execution or overlay rebuild needs a concrete local root
- remove helpers or call sites that only reflect the pre-identity internal keying model

- [ ] **Step 3: Rename retained helpers if their names imply obsolete architecture**

Implementation examples:
- prefer names like `resolve_tracked_path_identity` or `preferred_local_path_for_identity` if the current names are ambiguous
- update call sites and comments to describe the boundary explicitly

- [ ] **Step 4: Add or adjust focused tests only if behavior changes**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`

Expected: PASS

### Task 4: Prove there is no remaining live path-keyed dedup logic

**Files:**
- Inspect: `crates/flotilla-daemon/src/server.rs`
- Inspect: `crates/flotilla-core/src/in_process.rs`

- [ ] **Step 1: Search for stale path-keyed dedup state in live modules**

Run: `rg -n "HashMap<PathBuf, u64>|last_sent_versions" crates/flotilla-daemon/src crates/flotilla-core/src`

Expected: no live peer replication dedup keyed by `PathBuf`

- [ ] **Step 2: Re-run daemon/server targeted tests**

Run: `cargo test -p flotilla-daemon --locked server::tests::should_send_local_version_dedupes_by_repo_identity -- --exact`

Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs crates/flotilla-daemon/src/server.rs crates/flotilla-core/tests/in_process_daemon.rs crates/flotilla-daemon/tests
git commit -m "refactor: tighten repo identity boundary helpers"
```

## Chunk 3: Full Verification

### Task 5: Run repository verification on the cleanup branch

**Files:**
- Verify only

- [ ] **Step 1: Run formatting**

Run: `cargo +nightly fmt --check`

Expected: PASS

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Expected: PASS

- [ ] **Step 3: Run full tests**

Run: `cargo test --workspace --locked`

Expected: PASS

- [ ] **Step 4: Summarize branch outcome**

Include:
- deleted dead module(s)
- retained or renamed boundary helpers
- verification results
