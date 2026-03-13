# Peer Connection Ownership V2 Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace hostname-ordered peer initiation with config-driven ownership, duplicate arbitration, direct `Goodbye` retirement, reconnect suppression, and status updates based on the winning active connection.

**Architecture:** Keep `Hello` as the identity-establishing step, but move connection ownership decisions into `PeerManager::activate_connection(...)` with richer per-candidate metadata. `server.rs` and `ssh_transport.rs` keep the transport/session wiring, while `PeerManager` becomes the single owner of duplicate arbitration, reconnect suppression, and winner-aware status semantics.

**Tech Stack:** Rust, Tokio, serde, existing `flotilla-protocol`, `flotilla-daemon`, and in-process daemon event replay.

---

## File Structure

- `crates/flotilla-protocol/src/lib.rs`
  - Extend `PeerWireMessage` with direct `Goodbye`.
  - Add serde roundtrip coverage for the new variant.
- `crates/flotilla-daemon/src/peer/manager.rs`
  - Replace pairwise initiator gating with candidate-aware arbitration.
  - Track active connection metadata, reconnect suppression, and winner-aware disconnect behavior.
  - Add focused unit tests for arbitration and duplicate teardown.
- `crates/flotilla-daemon/src/peer/ssh_transport.rs`
  - Teach the peer transport to send/receive `Goodbye` and surface retirement closes distinctly from ordinary disconnects where needed.
- `crates/flotilla-daemon/src/server.rs`
  - Remove lexicographic outbound gating from startup/reconnect.
  - Route inbound `Goodbye`, apply winner-aware status updates, and avoid overview flapping on loser teardown.
  - Add integration-style daemon tests for simultaneous duplicate cases.
- `docs/superpowers/specs/2026-03-12-peer-connection-ownership-v2-design.md`
  - Update only if implementation reveals a material design correction.

## Chunk 1: Protocol Surface And Connection Metadata

### Task 1: Add direct `Goodbye` to the peer wire model

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Test: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Write the failing protocol roundtrip tests**

Add tests alongside the existing `Message::Peer` / routed message roundtrips:

```rust
#[test]
fn message_peer_goodbye_roundtrip() {
    let msg = Message::Peer(Box::new(PeerWireMessage::Goodbye {
        reason: GoodbyeReason::Superseded,
    }));
    let json = serde_json::to_string(&msg).expect("serialize");
    let decoded: Message = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(
        decoded,
        Message::Peer(inner)
            if matches!(*inner, PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded })
    ));
}
```

- [ ] **Step 2: Run the targeted protocol test to verify it fails**

Run: `cargo test -p flotilla-protocol --locked message_peer_goodbye_roundtrip`

Expected: FAIL because `PeerWireMessage::Goodbye` and `GoodbyeReason` do not exist yet.

- [ ] **Step 3: Implement the minimal protocol changes**

Add:

```rust
pub enum PeerWireMessage {
    Data(PeerDataMessage),
    Routed(RoutedPeerMessage),
    Goodbye { reason: GoodbyeReason },
}

pub enum GoodbyeReason {
    Superseded,
}
```

Use the same serde style already used for `PeerWireMessage`.

- [ ] **Step 4: Run the targeted protocol tests**

Run: `cargo test -p flotilla-protocol --locked message_peer_goodbye_roundtrip`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/lib.rs
git commit -m "feat: add peer goodbye control message"
```

### Task 2: Extend connection metadata to describe candidate ownership

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Test: `crates/flotilla-daemon/src/peer/manager.rs`

- [ ] **Step 1: Write the failing unit test for candidate-specific config backing**

Add a test that creates:
- one outbound candidate marked `config_backed = true`
- one inbound candidate for the same host marked `config_backed = false`

Then assert that the metadata can distinguish the locally-owned outbound
candidate from an inbound duplicate. Do not use this test to define the
live-socket winner rule.

- [ ] **Step 2: Run the targeted test**

Run: `cargo test -p flotilla-daemon --locked configured_outbound_beats_unsolicited_inbound`

Expected: FAIL because `ConnectionMeta` cannot currently express candidate-specific config backing.

- [ ] **Step 3: Add the new metadata fields**

Extend `ConnectionMeta` with fields along the lines of:

```rust
pub struct ConnectionMeta {
    pub direction: ConnectionDirection,
    pub config_label: Option<ConfigLabel>,
    pub expected_peer: Option<HostName>,
    pub config_backed: bool,
}
```

Only the specific locally-owned outbound candidate should set `config_backed = true`.

- [ ] **Step 4: Run the targeted daemon test**

Run: `cargo test -p flotilla-daemon --locked configured_outbound_beats_unsolicited_inbound`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs
git commit -m "refactor: add candidate ownership metadata for peers"
```

## Chunk 2: Duplicate Arbitration And Winner Selection

### Task 3: Replace pairwise initiator gating with runtime arbitration

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Test: `crates/flotilla-daemon/src/peer/manager.rs`

- [ ] **Step 1: Write failing tests for duplicate convergence**

Add tests for:
- simultaneous dual-outbound candidates converge to one physical winner
- stale winner teardown does not orphan the surviving connection
- losing duplicate close does not remove the surviving sender

Suggested shape:

```rust
#[tokio::test]
async fn simultaneous_dual_outbound_converges_to_one_winner() { /* ... */ }

#[tokio::test]
async fn losing_duplicate_disconnect_does_not_orphan_winner() { /* ... */ }
```

- [ ] **Step 2: Run the targeted tests**

Run: `cargo test -p flotilla-daemon --locked simultaneous_dual_outbound_converges_to_one_winner losing_duplicate_disconnect_does_not_orphan_winner`

Expected: FAIL because `activate_connection(...)` currently always bumps generation and replaces the sender.

- [ ] **Step 3: Implement winner-aware active connection tracking**

Add a focused internal type in `manager.rs`, for example:

```rust
struct ActiveConnection {
    generation: u64,
    meta: ConnectionMeta,
}
```

Keep sender storage and active ownership metadata aligned so that:
- only the winning connection becomes current generation
- losing duplicates are identifiable as non-current without evicting the winner
- disconnect handling can distinguish "winner closed" from "loser closed"

- [ ] **Step 4: Implement deterministic physical-connection selection**

Inside `activate_connection(...)`:
- compare the new candidate with the current active one
 - use `config_backed` only for long-term reconnect ownership / suppression
 - when two simultaneous physical connections exist, apply a deterministic
   pair-level physical-connection rule so both sides choose the same surviving
   live socket
- return an explicit activation result, not just a generation, so callers know whether this connection won or lost

Prefer a result type like:

```rust
enum ActivationResult {
    Accepted { generation: u64, displaced: Option<u64> },
    Rejected { reason: GoodbyeReason },
}
```

- [ ] **Step 5: Update disconnect handling to be winner-aware**

Ensure `disconnect_peer(...)` only tears down peer ownership when the closing connection still owns the active generation. If a loser closes later, it must become a no-op for sender ownership and route authority.

- [ ] **Step 6: Run the targeted tests**

Run: `cargo test -p flotilla-daemon --locked simultaneous_dual_outbound_converges_to_one_winner losing_duplicate_disconnect_does_not_orphan_winner activate_connection_supersedes_older_sender stale_generation_inbound_message_is_dropped`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs
git commit -m "feat: arbitrate duplicate peer connections"
```

### Task 4: Remove lexicographic initiation gating from outbound ownership

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Test: `crates/flotilla-daemon/src/peer/manager.rs`

- [ ] **Step 1: Write the failing tests**

Add tests that assert:
- `outbound_peer_names()` includes configured peers regardless of host ordering
- `reconnect_peer()` no longer returns the pairwise initiator rule error

- [ ] **Step 2: Run the targeted tests**

Run: `cargo test -p flotilla-daemon --locked connect_all_only_initiates_when_local_host_is_smaller`

Expected: FAIL after test rename/update because the old ordering rule still exists.

- [ ] **Step 3: Remove `should_initiate_peer(...)` gating**

Change:
- `outbound_peer_names()`
- `connect_all()`
- `reconnect_peer()`

so outbound eligibility is driven by actual configured/discovered peers, not lexicographic ordering.

- [ ] **Step 4: Run the updated targeted tests**

Run: `cargo test -p flotilla-daemon --locked outbound_peer_names_include_all_configured_peers reconnect_peer_allows_configured_peer_regardless_of_host_order`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server.rs
git commit -m "refactor: make outbound peer ownership config-driven"
```

## Chunk 3: Goodbye, Reconnect Suppression, And Status Semantics

### Task 5: Send and receive `Goodbye { Superseded }`

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/src/peer/ssh_transport.rs`
- Test: `crates/flotilla-daemon/src/server.rs`
- Test: `crates/flotilla-daemon/src/peer/ssh_transport.rs`

- [ ] **Step 1: Write failing tests for duplicate retirement signaling**

Add tests that assert:
- a rejected/superseded duplicate receives `PeerWireMessage::Goodbye`
- protocol mismatch still closes without requiring `Goodbye`

- [ ] **Step 2: Run the targeted tests**

Run: `cargo test -p flotilla-daemon --locked duplicate_peer_receives_goodbye_on_supersede`

Expected: FAIL because there is no `Goodbye` handling yet.

- [ ] **Step 3: Implement direct `Goodbye` handling**

In inbound/outbound peer loops:
- if arbitration rejects a candidate, send `Message::Peer(PeerWireMessage::Goodbye { ... })` before closing where possible
- add receive-side handling for `PeerWireMessage::Goodbye`
- do not treat protocol mismatch / unexpected peer identity as requiring a structured goodbye

- [ ] **Step 4: Run the targeted tests**

Run: `cargo test -p flotilla-daemon --locked duplicate_peer_receives_goodbye_on_supersede`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/peer/ssh_transport.rs crates/flotilla-protocol/src/lib.rs
git commit -m "feat: signal peer supersession with goodbye"
```

### Task 6: Suppress reconnect after duplicate loss

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Test: `crates/flotilla-daemon/src/peer/manager.rs`

- [ ] **Step 1: Write the failing tests**

Add tests for:
- loser receives `Goodbye` and enters a reconnect suppression window
- winner disconnect later clears suppression and allows reconnect

- [ ] **Step 2: Run the targeted tests**

Run: `cargo test -p flotilla-daemon --locked goodbye_superseded_suppresses_reconnect winner_disconnect_clears_reconnect_suppression`

Expected: FAIL because no suppression state exists.

- [ ] **Step 3: Implement bounded reconnect suppression**

Add a map in `PeerManager` keyed by canonical `HostName`, holding a suppression deadline.

Use it in reconnect paths:
- normal disconnect without `Goodbye` -> unchanged backoff
- `Goodbye { Superseded }` -> suppress reconnect until deadline
- winner loss -> clear suppression for that peer

- [ ] **Step 4: Run the targeted tests**

Run: `cargo test -p flotilla-daemon --locked goodbye_superseded_suppresses_reconnect winner_disconnect_clears_reconnect_suppression`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server.rs
git commit -m "feat: suppress reconnect after duplicate peer loss"
```

### Task 7: Make peer status reflect winning ownership, not raw socket churn

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs` (only if event ordering/model handling needs adjustment)
- Test: `crates/flotilla-daemon/src/server.rs`

- [ ] **Step 1: Write the failing tests**

Add tests for:
- losing duplicate teardown does not emit a visible `Disconnected` state while the winner remains active
- peer remains `Connected` if any winning active connection exists

- [ ] **Step 2: Run the targeted tests**

Run: `cargo test -p flotilla-daemon --locked duplicate_loser_close_does_not_emit_disconnected`

Expected: FAIL because `server.rs` currently emits `Disconnected` on raw socket close.

- [ ] **Step 3: Implement winner-aware status emission**

Before emitting `Disconnected` in outbound and inbound paths:
- ask the manager whether the closing generation still owns the peer
- emit `Disconnected` only when the winning active connection is actually gone
- keep configured peers visible in replay regardless of current state

- [ ] **Step 4: Run the targeted tests**

Run: `cargo test -p flotilla-daemon --locked duplicate_loser_close_does_not_emit_disconnected handle_client_forwards_peer_data_and_registers_peer daemon_server_replays_configured_hosts_as_disconnected`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-tui/src/app/mod.rs
git commit -m "fix: report peer status from winning connection ownership"
```

## Chunk 4: End-To-End Verification And Documentation

### Task 8: Update tests and docs to the new ownership model

**Files:**
- Modify: `docs/superpowers/specs/2026-03-12-peer-connection-ownership-v2-design.md` (only if implementation changed the design)
- Modify: `docs/superpowers/plans/2026-03-13-peer-connection-ownership-v2.md` (check off items only during execution, do not edit during planning)
- Test: `crates/flotilla-daemon/tests/multi_host.rs`

- [ ] **Step 1: Add one integration-style daemon test**

Cover a realistic dual-initiation case:
- both peers configured outbound
- one winning connection survives
- data still flows
- loser reconnect is suppressed after `Goodbye`

- [ ] **Step 2: Run focused integration verification**

Run: `cargo test -p flotilla-daemon --locked --test multi_host`

Expected: PASS.

- [ ] **Step 3: Run full verification**

Run: `cargo fmt --check`

Run: `cargo test --workspace --locked`

Expected: both PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-daemon/tests/multi_host.rs docs/superpowers/specs/2026-03-12-peer-connection-ownership-v2-design.md
git commit -m "test: verify peer ownership v2 end to end"
```
