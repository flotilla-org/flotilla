# Phase 2 Host Info Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add static host-summary replication for peer connections, covering host system info plus remote tool inventory/provider health retention for `#270` and the narrowed transport/state portion of `#271`.

**Architecture:** Keep `Message::Hello` unchanged and introduce a dedicated `PeerWireMessage::HostSummary` payload. Build one local `HostSummary` from daemon discovery/runtime state, send it during initial peer synchronization, and store remote summaries separately from repo peer snapshots in `PeerManager` so disconnect/restart cleanup remains simple.

**Tech Stack:** Rust, Tokio, serde, existing peer transport/manager pipeline, existing discovery runtime and provider-health conversion helpers.

---

## File Structure

- Modify: `crates/flotilla-protocol/src/lib.rs`
  - Re-export the new host-summary types.
- Modify: `crates/flotilla-protocol/src/peer.rs`
  - Add the new peer wire variant and protocol tests.
- Create: `crates/flotilla-protocol/src/host_summary.rs`
  - Define serde-friendly host-summary protocol types and their tests.
- Modify: `crates/flotilla-core/src/convert.rs`
  - Add conversion helpers from discovery assertions and provider-health maps into protocol host-summary types.
- Create: `crates/flotilla-core/src/host_summary.rs`
  - Add best-effort host system info collection and local summary assembly.
- Modify: `crates/flotilla-core/src/in_process.rs`
  - Build/store the local host summary at daemon startup and expose internal accessors for networking/tests.
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
  - Store remote host summaries, update them on inbound messages, and clear them on disconnect/restart.
- Modify: `crates/flotilla-daemon/src/peer/mod.rs`
  - Re-export any new manager result/accessor types if needed.
- Modify: `crates/flotilla-daemon/src/peer_networking.rs`
  - Send the local host summary during initial peer synchronization and handle inbound host-summary messages.
- Modify: `crates/flotilla-daemon/src/server.rs`
  - Mirror the same host-summary handling in the socket-peer path and add focused tests.
- Test: `crates/flotilla-daemon/tests/multi_host.rs`
  - Add daemon-level verification that leader/follower exchange and retain host summaries.

## Chunk 1: Protocol Host Summary Types

### Task 1: Add failing protocol tests for host summary roundtrips

**Files:**
- Create: `crates/flotilla-protocol/src/host_summary.rs`
- Modify: `crates/flotilla-protocol/src/peer.rs`

- [x] **Step 1: Write the failing `HostSummary` roundtrip tests**

Add tests covering:

```rust
#[test]
fn host_summary_roundtrips_with_optional_fields() {
    let summary = HostSummary {
        host_name: HostName::new("desktop"),
        system: SystemInfo {
            home_dir: Some(PathBuf::from("/home/dev")),
            os: Some("linux".into()),
            arch: Some("aarch64".into()),
            cpu_count: Some(8),
            memory_total_mb: None,
            environment: HostEnvironment::Container,
        },
        inventory: ToolInventory::default(),
        providers: vec![HostProviderStatus { category: "vcs".into(), name: "Git".into(), healthy: true }],
    };
    crate::test_helpers::assert_roundtrip(&summary);
}
```

and:

```rust
#[test]
fn peer_wire_message_host_summary_roundtrips() {
    let msg = PeerWireMessage::HostSummary(sample_host_summary());
    let json = serde_json::to_string(&msg).expect("serialize");
    let decoded: PeerWireMessage = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, PeerWireMessage::HostSummary(_)));
}
```

- [x] **Step 2: Run protocol tests to verify failure**

Run: `cargo test -p flotilla-protocol --locked host_summary -- --nocapture`
Expected: FAIL because the new module/types/variant do not exist yet.

- [x] **Step 3: Add the protocol types and wire variant**

Implement:

- `HostSummary`
- `SystemInfo`
- `HostEnvironment`
- inventory/helper structs
- `PeerWireMessage::HostSummary`
- exports from `lib.rs`

Keep all system fields optional except the environment enum and `host_name`.

- [x] **Step 4: Run protocol tests to verify pass**

Run: `cargo test -p flotilla-protocol --locked host_summary -- --nocapture`
Expected: PASS

- [x] **Step 5: Commit protocol host summary types**

```bash
git add crates/flotilla-protocol/src/lib.rs crates/flotilla-protocol/src/peer.rs crates/flotilla-protocol/src/host_summary.rs
git commit -m "feat: add peer host summary protocol"
```

## Chunk 2: Local Host Summary Assembly

### Task 2: Build failing conversion and collector tests

**Files:**
- Create: `crates/flotilla-core/src/host_summary.rs`
- Modify: `crates/flotilla-core/src/convert.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`

- [x] **Step 1: Write failing tests for discovery-to-summary conversion**

Add tests for:

- binary assertions become inventory entries with version/path detail
- socket/auth/env assertions land in the correct inventory buckets
- provider health maps become flat `HostProviderStatus` entries
- environment classification returns `Unknown` when no marker is present

Example:

```rust
#[test]
fn host_inventory_includes_versioned_binaries() {
    let bag = EnvironmentBag::new().with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.49.0"));
    let inventory = inventory_from_bag(&bag);
    assert_eq!(inventory.binaries[0].name, "git");
}
```

- [x] **Step 2: Run the focused core tests to verify failure**

Run: `cargo test -p flotilla-core --locked --lib host_summary -- --nocapture`
Expected: FAIL because the collector/conversion helpers do not exist yet.

- [x] **Step 3: Implement host-summary assembly**

Add:

- a best-effort system-info collector in `crates/flotilla-core/src/host_summary.rs`
- conversion helpers in `convert.rs` for discovery assertions and provider health
- a `build_local_host_summary(...)` helper that combines:
  - daemon `host_name`
  - `host_bag`
  - current host-level provider health

Use cheap probes only; prefer `None`/`Unknown` over brittle detection.

- [x] **Step 4: Store the local host summary on `InProcessDaemon`**

Update `InProcessDaemon` to:

- build the local host summary during `new(...)`
- retain it for later networking use
- expose internal accessors such as:
  - `pub fn local_host_summary(&self) -> &HostSummary`
  - a helper returning the host-level provider status if the assembly logic needs it

- [x] **Step 5: Run the focused core tests to verify pass**

Run: `cargo test -p flotilla-core --locked --lib host_summary -- --nocapture`
Expected: PASS

- [x] **Step 6: Commit local host summary assembly**

```bash
git add crates/flotilla-core/src/convert.rs crates/flotilla-core/src/host_summary.rs crates/flotilla-core/src/in_process.rs
git commit -m "feat: build local host summaries"
```

## Chunk 3: Peer Manager Storage and Transport

### Task 3: Add failing peer-manager tests for remote host summary retention

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/peer/mod.rs`
- Modify: `crates/flotilla-daemon/src/peer_networking.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`

- [x] **Step 1: Write failing `PeerManager` tests for host summary storage and cleanup**

Add tests covering:

- inbound `HostSummary` stores under the connection peer host
- disconnect removes stored host summaries
- `clear_peer_data_for_restart()` also clears stored host summaries

Example:

```rust
#[test]
fn remove_peer_data_clears_host_summary() {
    let mut mgr = PeerManager::new(HostName::new("leader"));
    mgr.store_host_summary(sample_host_summary_for("follower"));
    mgr.remove_peer_data(&HostName::new("follower"));
    assert!(mgr.get_peer_host_summaries().is_empty());
}
```

- [x] **Step 2: Run the manager tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked peer::manager::tests::host_summary -- --nocapture`
Expected: FAIL because remote host-summary storage does not exist yet.

- [x] **Step 3: Implement remote host-summary storage on `PeerManager`**

Add:

- `peer_host_summaries: HashMap<HostName, HostSummary>`
- accessors for tests/networking
- cleanup in both `remove_peer_data()` and `clear_peer_data_for_restart()`
- inbound handling path for `PeerWireMessage::HostSummary`

Do not mix host summaries into per-repo peer state.

- [x] **Step 4: Send local host summaries during initial peer synchronization**

Update both peer transport entry points:

- `peer_networking.rs`
- `server.rs`

So that on connection activation they send `PeerWireMessage::HostSummary(daemon.local_host_summary().clone())` during the same synchronization window as the existing repo snapshot push.

- [x] **Step 5: Handle inbound host summaries without triggering repo overlay rebuilds**

Ensure the inbound message processor:

- stores/replaces the summary
- does not enqueue repo overlay updates for host-only changes
- preserves existing peer data routing/cleanup semantics

- [x] **Step 6: Run the focused daemon tests to verify pass**

Run:

- `cargo test -p flotilla-daemon --locked peer::manager::tests::host_summary -- --nocapture`
- `cargo test -p flotilla-daemon --locked handle_client_forwards_peer_data_and_registers_peer -- --nocapture`

Expected: PASS

- [x] **Step 7: Commit peer host-summary transport/storage**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/peer/mod.rs crates/flotilla-daemon/src/peer_networking.rs crates/flotilla-daemon/src/server.rs
git commit -m "feat: replicate peer host summaries"
```

## Chunk 4: End-to-End Verification and Scope Alignment

### Task 4: Add daemon integration tests and finish verification

**Files:**
- Test: `crates/flotilla-daemon/tests/multi_host.rs`
- Modify: `docs/superpowers/specs/2026-03-14-phase2-host-info-design.md`
- Modify: `docs/superpowers/plans/2026-03-14-phase2-host-info.md`

- [x] **Step 1: Write the failing multi-host integration test**

Add a test that:

- starts a leader and follower test topology
- causes peer synchronization
- verifies the leader retains the follower `HostSummary`
- verifies disconnect/restart cleanup removes stale remote summary

- [x] **Step 2: Run the focused integration test to verify failure**

Run: `cargo test -p flotilla-daemon --locked --test multi_host host_summary -- --nocapture`
Expected: the test should catch any missing transport/storage wiring. In this branch it passed immediately because the lower-level implementation was already complete.

- [x] **Step 3: Implement any minimal missing wiring surfaced by the integration test**

No additional wiring was needed beyond the completed lower-level transport/storage work.

- [x] **Step 4: Run the full verification set**

Run:

- `cargo +nightly fmt --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- `git diff --check`

Expected: all PASS

- [x] **Step 5: Mark docs complete**

Update:

- `docs/superpowers/specs/2026-03-14-phase2-host-info-design.md`
- `docs/superpowers/plans/2026-03-14-phase2-host-info.md`

to reflect any final naming or file-path adjustments made during implementation.

- [x] **Step 6: Commit final verification/docs**

```bash
git add crates/flotilla-daemon/tests/multi_host.rs docs/superpowers/specs/2026-03-14-phase2-host-info-design.md docs/superpowers/plans/2026-03-14-phase2-host-info.md
git commit -m "chore: finalize phase 2 host info exchange"
```

## Issue Surgery

- Narrow `#271` so it explicitly covers replicated remote host inventory/provider health storage, not the broader host-facing API/UI project.
- Create a new follow-up issue for:
  - dedicated daemon host query methods
  - CLI host inventory/status commands
  - richer TUI host summary views
- Link the new issue back to `#270` and `#271` so the scope split is visible from the tracker.
