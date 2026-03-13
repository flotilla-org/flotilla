# Peer Connect/Reconnect Bug Fixes (#290, #263)

Fixes two related peer connection lifecycle bugs: missing local state send on connect/reconnect (#290) and a cleanup race on rapid disconnect/reconnect (#263).

## Problem

### #290: No local state sent on connect/reconnect

When a peer connects or reconnects, the daemon does not send its current local state to the new peer. The outbound task (server.rs:538) only sends data in response to snapshot events where `local_data_version` has increased since the last send. A newly connected peer receives nothing until the next local provider refresh.

The `last_sent_versions` guard (server.rs:567-570) compounds this: if data was previously sent to other peers, the version is recorded as "sent" even though the new peer never received it. If no local change occurs after reconnect, the new peer never gets current state.

The existing mitigation — only recording `last_sent_versions` when `sent == true` (server.rs:584) — helps when *no* peers are connected, but doesn't help when a new peer joins while others are already connected.

### #263: Cleanup race on rapid disconnect/reconnect

`disconnect_peer_and_rebuild` (server.rs:728) acquires the PeerManager lock to call `disconnect_peer`, drops it, then `rebuild_peer_overlays` re-acquires it per repo to read `peer_data` and build the overlay. Between these two lock acquisitions, a new connection's inbound data can arrive via `handle_inbound`, inserting fresh data that `rebuild_peer_overlays` then overwrites with stale state.

The generation check in `disconnect_peer` (manager.rs:868) and `handle_inbound` (manager.rs:465) protects PeerManager's own state, but `InProcessDaemon.peer_providers` has no such guard — it's written by `set_peer_providers` based on whatever `peer_data` snapshot is read under a separate lock acquisition.

## Design

### Peer-connected notification channel (#290)

A new `tokio::mpsc::unbounded_channel<PeerConnectedNotice>` carries connection notifications from the sites that establish peer connections to the outbound task.

```rust
struct PeerConnectedNotice {
    peer: HostName,
    generation: u64,
}
```

**Senders:** The channel's sender is cloned to:
1. SSH per-peer reconnect tasks — sends after `PeerStatusChanged::Connected` at server.rs:300-303
2. Inbound socket peer handler — sends after `PeerStatusChanged::Connected` at server.rs:990
3. Initial SSH connect path — sends inside the per-peer spawn loop (server.rs:249) where `(generation, inbound_rx)` is destructured from `initial_rx`, not at the status-event emission site (lines 235-238) which lacks generation access

**Receiver:** The outbound task (server.rs:538) changes its loop from `event_rx.recv()` to a `tokio::select!` over both `event_rx.recv()` and `peer_connected_rx.recv()`. On receiving a `PeerConnectedNotice`, it calls `send_local_to_peer` (new helper, see below) which iterates all repos and sends current local state to the specific peer.

### Targeted peer send (#290)

New method on `PeerManager`:

```rust
pub fn get_sender_if_current(&self, peer: &HostName, generation: u64) -> Option<Arc<dyn PeerSender>>
```

Returns the sender only if the peer's current generation matches. This ensures we don't send to a connection that has already been superseded between the notice being sent and the outbound task processing it.

New helper in server.rs:

```rust
async fn send_local_to_peer(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    host_name: &HostName,
    clock: &mut VectorClock,
    peer: &HostName,
    generation: u64,
) -> bool
```

Iterates all repos via `daemon.tracked_repo_paths()`. For each repo, calls `get_local_providers` to get the provider data and `find_identity_for_path` to get the `RepoIdentity` needed to construct the `PeerDataMessage`. Sends each message to the single peer via `get_sender_if_current`. Returns whether any data was sent. The generation guard makes this a no-op if the connection was already replaced.

This bypasses `last_sent_versions` entirely — we're sending to a specific peer that has no state, not broadcasting. Each repo sent ticks the outbound task's `VectorClock` (owned exclusively by this task, passed by `&mut`), which is correct — each message needs a unique, increasing clock value regardless of whether it targets one peer or all peers.

If a snapshot event and a connect notice arrive close together, the outbound task processes them sequentially (single task, `tokio::select!`). The new peer may receive the same data twice — once from the targeted send and once from the broadcast — but this is harmless since the data is identical. The second send will have a newer clock and won't be deduplicated by `last_seen_clocks`, but accepting identical data at a newer clock is a no-op from the peer's perspective.

### New method on InProcessDaemon (#290)

```rust
pub async fn tracked_repo_paths(&self) -> Vec<PathBuf>
```

Returns the keys of `self.repos` — only local repo paths, not remote/virtual ones. This is the correct source for `send_local_to_peer` since we only want to send local data to a newly connected peer. Simple read lock, collect keys.

### Atomic disconnect + overlay snapshot (#263)

Change `disconnect_peer_and_rebuild` so the disconnect and overlay data collection happen under a single PeerManager lock acquisition, eliminating the race window.

Extend `DisconnectPlan` (or add fields) to include the pre-computed overlay data for each affected repo:

```rust
pub struct DisconnectPlan {
    pub was_active: bool,
    pub affected_repos: Vec<RepoIdentity>,
    pub resync_requests: Vec<RoutedPeerMessage>,
    /// Pre-computed overlay state for each affected repo, captured atomically
    /// with the disconnect under the same lock.
    pub overlay_updates: Vec<OverlayUpdate>,
}

pub enum OverlayUpdate {
    /// Update peer_providers for a local or remote repo with remaining peer data.
    SetProviders { path: PathBuf, peers: Vec<(HostName, ProviderData)> },
    /// Remove a virtual repo — no peers remain.
    RemoveRepo { identity: RepoIdentity, path: PathBuf },
}
```

`disconnect_peer` computes these while holding `&mut self`, using the same logic currently in `rebuild_peer_overlays` — reading remaining `peer_data` for affected repos, checking `known_remote_repos`, etc.

`disconnect_peer_and_rebuild` then drops the lock and applies the pre-computed updates to `InProcessDaemon` via `set_peer_providers` / `remove_repo`. No second PeerManager lock acquisition needed.

`rebuild_peer_overlays` still exists for the resync-sweep timer path (server.rs:524), which calls it for expired resync requests. That path has the same lock-gap pattern but lower risk — resync sweeps handle timeout cases, not rapid reconnect. Addressing it is out of scope for this fix but noted for future hardening. `disconnect_peer_and_rebuild` no longer calls `rebuild_peer_overlays`.

The key insight: `disconnect_peer` already has `&mut self` on PeerManager. It can read `peer_data` for the affected repos and compute the overlay state atomically with the disconnect. By the time the lock is released, the overlay data is captured. Any new connection data arriving after lock release is processed normally and won't be overwritten.

For `find_repo_by_identity` (needed to map `RepoIdentity` → local path during overlay computation), this currently lives on `InProcessDaemon`. `PerRepoPeerState.repo_path` stores the *origin peer's* filesystem path, not the local path, so it cannot be used for this mapping. PeerManager needs the local identity-to-path mapping passed in.

`disconnect_peer` gains an additional parameter: `local_repo_paths: &HashMap<RepoIdentity, PathBuf>`. The caller (`disconnect_peer_and_rebuild`) obtains this from `InProcessDaemon` before acquiring the PeerManager lock. This mapping is stable during disconnect — local repos aren't removed by peer disconnect, so there's no race on the mapping itself.

For remote-only repos, `known_remote_repos` provides the synthetic path. `disconnect_peer` calls `unregister_remote_repo` for remote repos with no remaining peers while holding `&mut self`, removing the entry from `known_remote_repos` and returning the synthetic path in the `RemoveRepo` variant for the caller to apply after releasing the lock.

## Testing

**Unit tests (peer/manager.rs):**
- `disconnect_peer` with stale generation returns empty plan (verify existing)
- `disconnect_peer` returns correct `overlay_updates` for affected repos with remaining peers
- `disconnect_peer` returns `RemoveRepo` for remote-only repos with no remaining peers
- Overlay data reflects only remaining peers, not the disconnected peer's data

**Integration tests (server.rs or new test module):**
- Connect → verify local state is sent to peer
- Disconnect → reconnect → verify local state is re-sent on reconnect
- Rapid disconnect/reconnect: disconnect generation N, connect generation N+1, verify generation-N cleanup doesn't affect generation-N+1 data

The existing `MockPeerTransport` / `MockPeerSender` test infrastructure supports all of these.

## Out of scope

- Re-keying `InProcessDaemon` internals from `PathBuf` to `RepoIdentity` (#298) — related but independent refactor
- Delta-based peer replication — current design sends full snapshots, which is correct for initial state send
