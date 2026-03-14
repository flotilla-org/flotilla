# Phase 2 Bidirectional Flow Design

## Scope

This batch covers:

- `#267` Bidirectional peer data flow: followers write local state back to the leader
- `#268` Merge conflict resolution using `local_host`

It does not cover host metadata exchange or remote inventory/health (`#270`, `#271`).

## Current State

Phase 1 and the `#315` test-prep work are already on `main`.

- Followers and leaders both have the machinery to send local provider snapshots over the peer channel in `crates/flotilla-daemon/src/peer_networking.rs`.
- The central peer processor already stores inbound peer snapshots and rebuilds overlays / virtual repos from `PeerManager`.
- `InProcessDaemon` tests and daemon server tests can now run with injected fake discovery and do not need ambient CLI tools.

What is still incomplete is not the transport itself, but the semantics around merged state:

- `merge_provider_data()` in `crates/flotilla-core/src/merge.rs` still treats `local_host` as unused placeholder state.
- Once followers write back, leader and follower data can now legitimately coexist for the same logical repo, so conflict handling needs to become explicit rather than “last insert wins”.

## Goals

1. Followers write local-only provider state back to leaders using the existing peer replication channel.
2. Leaders ingest and surface that state without echo loops or accidental self-overwrite.
3. Merge behavior becomes host-aware and deterministic once multiple hosts contribute overlapping data.
4. Tests cover the transport path, daemon integration path, and merge rules.

## Non-Goals

- New peer message kinds
- Host-level metadata replication
- UI redesign or new host inventory surfaces
- Broad refactors outside peer replication and merge semantics

## Design

### 1. Bidirectional Data Flow

Treat the existing snapshot exchange as symmetric:

- Any host may publish its local-only provider snapshot for a repo.
- `send_local_to_peers()` and `send_local_to_peer()` remain the only outbound mechanisms.
- The central inbound processor continues to route all snapshots through `PeerManager::handle_inbound()`, then rebuilds merged overlays / virtual repos from stored peer state.

No protocol expansion is needed for `#267`. The required work is to confirm and tighten the invariant that only local provider state is sent, while merged peer state is only used for presentation.

Concretely:

- Outbound peer replication must continue to use `get_local_providers()` rather than any merged snapshot.
- Inbound snapshots must continue to be keyed by `origin_host` and `repo_identity`, so follower data is stored as follower-owned data rather than blended before storage.
- Relay behavior must keep using vector-clock dedup and `prepare_relay()` so reverse-direction follower snapshots propagate through intermediate peers without loops.

### 2. Conflict Resolution

`merge_provider_data(local, local_host, peers)` becomes authoritative for Phase 2 merge policy.

Rules:

- Checkouts:
  - `HostPath` ownership is authoritative.
  - If the same `HostPath` appears more than once, the checkout whose host matches `HostPath.host()` wins.
  - In practice this means a host’s own checkout state is authoritative for that checkout, whether it is local or peer-sourced.
- Managed terminals and workspaces:
  - Continue host-namespacing peer keys to avoid collisions.
  - Local un-namespaced entries remain the local host’s authoritative entries.
- Branches:
  - Keep local-first behavior for plain branch maps unless ownership can be proven more strongly.
  - This preserves current semantics and avoids inventing fake authority for shared branch names.
- Service-level data (`change_requests`, `issues`, `sessions`):
  - Leader/local data remains authoritative.
  - Peer entries only fill gaps; they do not overwrite local entries.

This keeps the strongest rule where ownership is explicit (`HostPath`) and avoids broad speculative changes where ownership is not explicit.

### 3. Echo / Reapplication Behavior

The intended invariant is:

- A host sends only its own local state.
- A host stores inbound peer state separately.
- Presentation merges local + peer state.
- Outbound refresh never republishes merged peer overlays.

That invariant already mostly exists. The batch should preserve it with tests instead of adding new suppression state unless the tests prove a real loop.

## Testing

### Peer transport / relay

Use `crates/flotilla-daemon/src/peer/channel_tests.rs` to prove:

- reverse-direction follower snapshots still propagate through relays
- bidirectional exchange remains symmetric
- duplicate clocks are dropped and do not create echo churn

### Merge

Extend `crates/flotilla-daemon/src/peer/merge.rs` tests to cover:

- host-owned checkout conflict resolution for duplicate `HostPath`
- local service-level data remains authoritative
- namespaced peer terminals/workspaces remain collision-free

### Daemon integration

Use:

- `crates/flotilla-daemon/tests/multi_host.rs`
- `crates/flotilla-daemon/src/server.rs` test module

to prove:

- leader-side daemon ingest of follower snapshots updates overlays correctly
- follower write-back does not depend on ambient host tools
- remote-only repos and local repos both rebuild correctly from peer updates

## Risks

- Over-correcting merge behavior for branch/service maps could silently change UI semantics unrelated to `#267/#268`.
- If any code path accidentally republishes merged peer state instead of local-only state, follower data could bounce indefinitely.

The mitigation is to keep the behavioral change focused on explicit ownership types and prove the no-echo invariant in tests rather than adding speculative runtime complexity.
