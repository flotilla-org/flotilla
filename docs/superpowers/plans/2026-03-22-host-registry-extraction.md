# Host Registry Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract host state management from `InProcessDaemon` into a focused `HostRegistry` struct, reducing the file from 3,268 lines to ~2,920 and from 20 fields to 17.

**Architecture:** Move `hosts`, `configured_peer_names`, `topology_routes`, and `local_host_summary` plus their associated free functions and the `host_queries` module into a new `HostRegistry` struct in `host_registry.rs`. Mutation methods take `emit: impl Fn(DaemonEvent)` closures. InProcessDaemon delegates to `self.host_registry`.

**Tech Stack:** Rust, tokio (async RwLock), flotilla-protocol types

**Spec:** `docs/superpowers/specs/2026-03-22-host-registry-extraction-design.md`

---

### Task 1: Create `host_registry.rs` with struct, constructor, and private types

Move `HostState`, the 8 free functions, and `host_queries` content into the new module. Wire up in `lib.rs`.

**Files:**
- Create: `crates/flotilla-core/src/host_registry.rs`
- Modify: `crates/flotilla-core/src/lib.rs:9` (replace `mod host_queries` with `pub(crate) mod host_registry`)

- [ ] **Step 1: Create `host_registry.rs` with struct, constructor, `HostCounts`, and private helpers**

The file should contain:
- `HostCounts` (moved from `host_queries.rs`, `pub(crate)`)
- `HostState` (moved from `in_process.rs:474-480`, private)
- `HostRegistry` struct with the 5 fields from the spec
- `HostRegistry::new(host_name, local_host_summary) -> Self` — initializes `hosts` with local host entry (`Connected`, summary present, seq 1)
- All 8 free functions from `in_process.rs:482-574` as private functions in this module: `default_host_summary`, `ensure_remote_host_state`, `build_host_snapshot`, `update_host_status`, `update_host_summary`, `clear_host_summary`, `should_present_host_state`, `mark_host_removed`
- All 6 functions from `host_queries.rs` as private functions: `known_hosts`, `connection_status`, `build_host_list_entry`, `build_host_status`, `build_host_providers`, `build_topology`
- Simple accessors: `host_name()`, `local_host_summary()`

The free functions keep the same signatures but reference `HostCounts` from this module instead of `crate::host_queries::HostCounts`.

- [ ] **Step 2: Update `lib.rs` module registration**

Add `pub(crate) mod host_registry;` alongside the existing `mod host_queries;`. Keep both for now — `host_queries` is removed in Task 4 after all callers are rewired.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p flotilla-core 2>&1 | head -30`
Expected: clean compile. Both modules coexist temporarily.

- [ ] **Step 4: Commit**

```
git add crates/flotilla-core/src/host_registry.rs crates/flotilla-core/src/lib.rs
git commit -m "refactor: add host_registry module with struct, constructor, and private helpers"
```

---

### Task 2: Add mutation methods to `HostRegistry`

Implement the 6 mutation methods from the spec. These encapsulate the logic currently spread across `InProcessDaemon` methods and free functions.

**Files:**
- Modify: `crates/flotilla-core/src/host_registry.rs`

- [ ] **Step 1: Add `sync_host_membership` as a private async method**

Move logic from `InProcessDaemon::sync_host_membership` (`in_process.rs:838-882`). Key changes: instead of calling `self.remote_host_counts()`, it takes `remote_counts: &HashMap<HostName, HostCounts>` as a parameter. Instead of returning `Vec<DaemonEvent>`, it calls `emit` directly.

Note: this method reads `configured_peer_names` then acquires the `hosts` write lock. Callers that modify `hosts` first must release that lock before calling `sync_host_membership`, which re-acquires it. This deliberate unlock-relock is safe because the method is idempotent.

- [ ] **Step 2: Add `publish_peer_connection_status`**

Move logic from `InProcessDaemon::publish_peer_connection_status` (`in_process.rs:812-823`). Emits `PeerStatusChanged` and `HostSnapshot` via the closure. Calls `sync_host_membership` internally. Returns `Option<HostSnapshot>`.

- [ ] **Step 3: Add `publish_peer_summary`**

Move logic from `InProcessDaemon::publish_peer_summary` (`in_process.rs:825-836`). Emits `HostSnapshot` via closure. Returns `Option<HostSnapshot>`. Does NOT call `sync_host_membership` (matches current behavior).

- [ ] **Step 4: Add `set_configured_peer_names`**

Move logic from `InProcessDaemon::set_configured_peer_names` (`in_process.rs:776-782`). Writes `configured_peer_names`, then calls `sync_host_membership`.

- [ ] **Step 5: Add `set_peer_host_summaries`**

Move logic from `InProcessDaemon::set_peer_host_summaries` (`in_process.rs:784-810`). Normalizes host_name fields, clears/updates summaries, emits events, then calls `sync_host_membership`.

- [ ] **Step 6: Add `set_topology_routes`**

Move logic from `InProcessDaemon::set_topology_routes` (`in_process.rs:890-896`). Sorts defensively, writes lock.

- [ ] **Step 7: Add `apply_event`**

Move the host-state mirroring logic from `InProcessDaemon::send_event` (`in_process.rs:1708-1756`). Handles `PeerStatusChanged`, `HostSnapshot`, `HostRemoved`. Uses `try_write` — best-effort semantics.

- [ ] **Step 8: Verify it compiles**

Run: `cargo build -p flotilla-core 2>&1 | head -30`

- [ ] **Step 9: Commit**

```
git add crates/flotilla-core/src/host_registry.rs
git commit -m "refactor: add mutation methods to HostRegistry"
```

---

### Task 3: Add query methods to `HostRegistry`

Implement the 5 query methods and `replay_host_events`.

**Files:**
- Modify: `crates/flotilla-core/src/host_registry.rs`

- [ ] **Step 1: Add `peer_connection_status`**

Move logic from `InProcessDaemon::peer_connection_status` (`in_process.rs:766-774`). Reads `hosts` lock, filters by `!state.removed`.

- [ ] **Step 2: Add `list_hosts`**

Move logic from `InProcessDaemon::list_hosts` (`in_process.rs:2579-2603`). Takes `local_counts` and `remote_counts` as parameters. Reads `configured_peer_names` and `hosts` internally. Calls the absorbed `known_hosts` and `build_host_list_entry` helpers.

- [ ] **Step 3: Add `get_host_status`**

Move logic from `InProcessDaemon::get_host_status` (`in_process.rs:2606-2628`). Uses `self.local_host_summary` for the local host fallback.

- [ ] **Step 4: Add `get_host_providers`**

Move logic from `InProcessDaemon::get_host_providers` (`in_process.rs:2630-2647`). Takes `remote_counts: &HashMap<HostName, HostCounts>` because the current implementation calls `known_hosts()` for host resolution, which needs remote counts. Uses `self.local_host_summary` for the local host fallback.

- [ ] **Step 5: Add `get_topology`**

Move logic from `InProcessDaemon::get_topology` (`in_process.rs:2649-2653`). Reads `topology_routes` and `configured_peer_names`.

- [ ] **Step 6: Add `replay_host_events`**

Move the host-event replay block from `InProcessDaemon::replay_since` (`in_process.rs:2346-2359`). Iterates `hosts`, builds `HostSnapshot` / `HostRemoved` events based on `last_seen` seqs.

- [ ] **Step 7: Verify it compiles**

Run: `cargo build -p flotilla-core 2>&1 | head -30`

- [ ] **Step 8: Commit**

```
git add crates/flotilla-core/src/host_registry.rs
git commit -m "refactor: add query methods and replay_host_events to HostRegistry"
```

---

### Task 4: Wire `InProcessDaemon` to use `HostRegistry`

Replace the 4 host fields with `host_registry`, update all call sites, delete the moved free functions and `host_queries.rs`.

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`
- Delete: `crates/flotilla-core/src/host_queries.rs`

- [ ] **Step 1: Replace fields in `InProcessDaemon` struct**

Remove `hosts`, `configured_peer_names`, `topology_routes`, `local_host_summary` fields (`in_process.rs:601-607,620`). Add `host_registry: HostRegistry`. Update the `use` imports at the top to add `use crate::host_registry::{HostCounts, HostRegistry};`.

- [ ] **Step 2: Update `InProcessDaemon::new()` constructor**

Replace the inline `hosts` HashMap, `configured_peer_names`, `topology_routes`, and `local_host_summary` initialization (`in_process.rs:690-716`) with `HostRegistry::new(host_name.clone(), local_host_summary.clone())`. The `local_host_summary` variable is already computed at line 681 — pass it into both `HostRegistry::new` and keep a clone for the daemon's own `local_host_summary` field... wait, the daemon no longer has that field. It's in the registry now. So just pass it to `HostRegistry::new`.

- [ ] **Step 3: Update simple accessors**

- `host_name()` — keep on InProcessDaemon (used widely for non-host purposes), no change needed
- `local_host_summary()` — delegate to `self.host_registry.local_host_summary()`
- `session_id()` — no change
- `peer_connection_status()` — delegate to `self.host_registry.peer_connection_status()`

- [ ] **Step 4: Update mutation method delegates**

Replace bodies of:
- `set_configured_peer_names` — compute `remote_counts`, delegate to `self.host_registry.set_configured_peer_names(peers, &remote_counts, |e| { let _ = self.event_tx.send(e); })`
- `set_peer_host_summaries` — compute `remote_counts`, delegate similarly
- `publish_peer_connection_status` — compute `remote_counts`, delegate. Capture return value for caller.
- `publish_peer_summary` — delegate with emit closure
- `set_topology_routes` — delegate directly

**Important**: The emit closure must call `self.event_tx.send(e)` directly — NOT `self.send_event(e)`. HostRegistry mutation methods handle state updates internally; `send_event` would double-apply them via `apply_event`.

Remove `emit_host_membership_events` and `sync_host_membership` methods.

- [ ] **Step 5: Update `send_event`**

Replace the `PeerStatusChanged`, `HostSnapshot`, `HostRemoved` match arms (`in_process.rs:1709-1756`) with a single call to `self.host_registry.apply_event(&event)`. Keep the `let _ = self.event_tx.send(event)` at the end.

- [ ] **Step 6: Update `replay_since`**

Replace the host-event loop (`in_process.rs:2346-2359`) with:
```rust
let mut events = self.host_registry.replay_host_events(last_seen).await;
```
The repo-event loop stays unchanged.

- [ ] **Step 7: Update DaemonHandle query implementations**

Replace bodies of `list_hosts`, `get_host_status`, `get_host_providers`, `get_topology` with delegates that compute counts then call into `self.host_registry`.

- [ ] **Step 8: Update `local_host_counts` and `remote_host_counts` return types**

Change `crate::host_queries::HostCounts` → `HostCounts` (already imported from `crate::host_registry`).

- [ ] **Step 9: Delete free functions, `host_queries.rs`, and clean up `lib.rs`**

Delete `HostState`, `default_host_summary`, `ensure_remote_host_state`, `build_host_snapshot`, `update_host_status`, `update_host_summary`, `clear_host_summary`, `should_present_host_state`, `mark_host_removed` from `in_process.rs` (lines 474-574).

Delete `crates/flotilla-core/src/host_queries.rs`.

Remove `mod host_queries;` from `crates/flotilla-core/src/lib.rs` (kept temporarily since Task 1).

- [ ] **Step 10: Verify it compiles**

Run: `cargo build -p flotilla-core 2>&1 | head -30`
Expected: clean compile

- [ ] **Step 11: Commit**

```
git add -A
git commit -m "refactor: wire InProcessDaemon to use HostRegistry, delete host_queries"
```

---

### Task 5: Fix downstream crates and run full test suite

Other crates in the workspace may reference `host_queries` or the moved types.

**Files:**
- Possibly modify: any file importing from `flotilla_core::host_queries` (grep found none, but verify)
- Possibly modify: `crates/flotilla-daemon/src/server/` files that call InProcessDaemon host methods

- [ ] **Step 1: Build the full workspace**

Run: `cargo build --workspace --all-targets --locked 2>&1 | head -50`
Fix any compilation errors in downstream crates (daemon server calling InProcessDaemon host methods with changed signatures).

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | head -50`
Fix any warnings.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test --workspace --locked 2>&1 | tail -30`
Expected: all tests pass. The integration tests in `in_process_daemon` exercise the DaemonHandle trait and should work unchanged.

- [ ] **Step 4: Run format check**

Run: `cargo +nightly-2026-03-12 fmt --check`
Fix if needed.

- [ ] **Step 5: Commit any fixes**

```
git add -A
git commit -m "fix: resolve downstream compilation and lint issues from host registry extraction"
```

---

### Task 6: Add unit tests for `HostRegistry`

Add tests exercising the mutation methods with captured emit closures.

**Files:**
- Modify: `crates/flotilla-core/src/host_registry.rs` (add `#[cfg(test)] mod tests`)

- [ ] **Step 1: Add test for constructor**

Test that `HostRegistry::new` initializes the local host entry as Connected with the provided summary.

- [ ] **Step 2: Add test for `publish_peer_connection_status`**

Create registry, publish a connection status, verify the emit closure receives `PeerStatusChanged` and `HostSnapshot` events, verify return value is `Some(snapshot)`. Publish same status again, verify `None` return (no-op).

- [ ] **Step 3: Add test for `publish_peer_summary`**

Create registry, publish a summary, verify emit receives `HostSnapshot`, verify return value. Publish identical summary, verify `None`.

- [ ] **Step 4: Add test for `set_configured_peer_names`**

Create registry, set configured names, verify `HostSnapshot` events emitted for new peers. Set empty, verify `HostRemoved` events.

- [ ] **Step 5: Add test for `apply_event`**

Create registry, apply `PeerStatusChanged` event, verify state updated via `peer_connection_status` query. Apply `HostRemoved`, verify host is gone from `peer_connection_status`.

- [ ] **Step 6: Add test for `replay_host_events`**

Create registry with some host state, replay with empty `last_seen` (should get all hosts), replay with current seqs (should get nothing), replay with stale seq (should get updated snapshot).

- [ ] **Step 7: Run tests**

Run: `cargo test -p flotilla-core --locked 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 8: Commit**

```
git add crates/flotilla-core/src/host_registry.rs
git commit -m "test: add unit tests for HostRegistry mutation and query methods"
```

---

### Task 7: Final verification and cleanup

- [ ] **Step 1: Run the full CI gate**

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

All three must pass.

- [ ] **Step 2: Verify line counts**

Run: `wc -l crates/flotilla-core/src/in_process.rs crates/flotilla-core/src/host_registry.rs`
Expected: `in_process.rs` ~2,920 lines, `host_registry.rs` ~500 lines.

- [ ] **Step 3: Verify field count**

Grep for fields in InProcessDaemon struct — should be 17 (was 20).

- [ ] **Step 4: Commit any final cleanup**

If needed.
