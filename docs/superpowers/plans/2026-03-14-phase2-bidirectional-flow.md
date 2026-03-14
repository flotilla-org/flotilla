# Phase 2 Bidirectional Flow Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete `#267` and `#268` by making follower-to-leader peer state flow explicitly verified and by implementing host-aware merge conflict resolution.

**Architecture:** Keep the existing symmetric peer snapshot transport and central inbound processing model. Focus code changes on `merge_provider_data()` semantics and on tests that prove outbound local-only replication, leader ingest, relay behavior, and no-echo/dedup invariants.

**Tech Stack:** Rust, Tokio, existing peer protocol/types in `flotilla-protocol`, daemon peer manager/networking in `flotilla-daemon`, merge logic in `flotilla-core`

---

## Chunk 1: Merge Semantics

### Task 1: Tighten checkout conflict semantics in `merge_provider_data`

**Files:**
- Modify: `crates/flotilla-core/src/merge.rs`
- Test: `crates/flotilla-daemon/src/peer/merge.rs`

- [x] **Step 1: Write failing merge tests for host-owned precedence**

Add tests in `crates/flotilla-daemon/src/peer/merge.rs` covering:
- local checkout for `HostPath(local_host, "/repo")` is not overwritten by peer data for the same host path
- peer checkout for `HostPath(peer_host, "/repo")` overwrites stale local data for that same peer-owned host path
- service-level data remains local-first when peers provide duplicates

- [x] **Step 2: Run the targeted merge tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked peer::merge::tests:: -- --nocapture`

Expected: New assertions fail because `merge_provider_data()` still treats `local_host` as unused placeholder state.

- [x] **Step 3: Implement host-aware merge rules**

Update `crates/flotilla-core/src/merge.rs` so that:
- `local_host` is actively used
- checkout merge prefers the provider data whose source host matches `HostPath.host()`
- local/service-level maps (`change_requests`, `issues`, `sessions`) remain local-first via `or_insert`
- existing namespacing for peer terminals and workspaces stays unchanged
- branch behavior remains conservative/local-first

- [x] **Step 4: Run the targeted merge tests to verify pass**

Run: `cargo test -p flotilla-daemon --locked peer::merge::tests:: -- --nocapture`

Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/merge.rs crates/flotilla-daemon/src/peer/merge.rs
git commit -m "feat: add host-aware peer merge semantics"
```

## Chunk 2: Bidirectional Flow Verification

### Task 2: Prove reverse-direction peer replication and dedup invariants

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/channel_tests.rs`

- [x] **Step 1: Add failing channel/relay tests for reverse-direction behavior if gaps remain**

Review existing channel tests and add only missing cases needed to prove `#267`, for example:
- follower-origin snapshot relays through an intermediate peer and is stored by the leader
- duplicate follower snapshot clocks do not create repeat application after relay
- leader and follower can each originate snapshots for the same repo identity without transport-level breakage

- [x] **Step 2: Run the targeted channel tests**

Run: `cargo test -p flotilla-daemon --locked peer::channel_tests:: -- --nocapture`

Expected: Either PASS immediately (evidence that transport was already correct) or fail on the newly added coverage.

- [x] **Step 3: Implement only the minimal transport/peer-manager glue if the tests reveal a real gap**

Possible touch points if required:
- `crates/flotilla-daemon/src/peer_networking.rs`
- `crates/flotilla-daemon/src/peer/manager.rs`

Do not add new protocol types. Only fix concrete gaps proved by tests.

- [x] **Step 4: Re-run the targeted channel tests**

Run: `cargo test -p flotilla-daemon --locked peer::channel_tests:: -- --nocapture`

Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer/channel_tests.rs crates/flotilla-daemon/src/peer_networking.rs crates/flotilla-daemon/src/peer/manager.rs
git commit -m "test: prove bidirectional peer snapshot flow"
```

## Chunk 3: Daemon Integration

### Task 3: Verify leader ingest and overlay rebuilds for follower data

**Files:**
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Optional Modify: `crates/flotilla-daemon/src/peer_networking.rs`

- [x] **Step 1: Add failing daemon-level tests for follower-to-leader ingestion**

In `crates/flotilla-daemon/tests/multi_host.rs` and/or `crates/flotilla-daemon/src/server.rs`, cover:
- leader overlay rebuild when follower snapshot updates an existing local repo identity
- remote-only repo rebuild remains correct after follower-origin updates
- no accidental dependence on ambient tool detection in these daemon tests

- [x] **Step 2: Run the affected daemon tests to verify current behavior**

Run:
- `cargo test -p flotilla-daemon --locked --test multi_host -- --nocapture`
- `cargo test -p flotilla-daemon --locked --features skip-no-sandbox-tests server::tests:: -- --test-threads=1 --nocapture`

Expected: Any missing integration behavior shows up here.

- [x] **Step 3: Implement only the minimal daemon-side fixes if needed**

Likely areas:
- `crates/flotilla-daemon/src/peer_networking.rs` for overlay rebuild/update behavior
- `crates/flotilla-daemon/src/server.rs` for server-side peer test setup or forwarding paths

Do not broaden scope into host metadata or inventory work.

- [x] **Step 4: Re-run the affected daemon tests**

Run:
- `cargo test -p flotilla-daemon --locked --test multi_host -- --nocapture`
- `cargo test -p flotilla-daemon --locked --features skip-no-sandbox-tests server::tests:: -- --test-threads=1 --nocapture`

Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/tests/multi_host.rs crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/peer_networking.rs
git commit -m "feat: verify leader ingest of follower peer data"
```

## Chunk 4: Final Verification

### Task 4: Verify the whole batch

**Files:**
- Modify: none expected

- [x] **Step 1: Run formatting/lint verification on touched files**

Run:
- `cargo +nightly fmt --check`
- `cargo clippy --all-targets --locked -- -D warnings`

Expected: PASS

- [x] **Step 2: Run the full workspace tests**

Run: `cargo test --workspace --locked`

Expected: PASS

- [x] **Step 3: Run diff sanity checks**

Run:
- `git diff --check`
- `git status --short`

Expected: No whitespace errors; only intended files modified.

- [x] **Step 4: Final commit if verification required follow-up changes**

```bash
git add <touched-files>
git commit -m "chore: finalize phase 2 bidirectional flow"
```
