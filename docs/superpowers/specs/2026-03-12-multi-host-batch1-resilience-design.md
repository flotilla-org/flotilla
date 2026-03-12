# Multi-Host Phase 2 Batch 1: Resilience Hardening

Addresses four independent issues in the peer relay/connection infrastructure:
[#259](https://github.com/rjwittams/flotilla/issues/259),
[#262](https://github.com/rjwittams/flotilla/issues/262),
[#263](https://github.com/rjwittams/flotilla/issues/263),
[#264](https://github.com/rjwittams/flotilla/issues/264).

## #262: Unify peer send path with `PeerSender` trait

### Problem

`PeerManager::relay()` only iterates SSH transports. Peers that connected to our socket (inbound connections) never receive relayed messages. The root cause: two separate data structures track peers — `PeerManager.peers` for outbound SSH connections and `PeerClientMap` in `server.rs` for inbound socket connections.

A secondary consequence: `send_to()` (used for resync responses) also only reaches SSH transports. If the requesting peer is an inbound socket peer, the resync response is lost.

### Design

Extract a `PeerSender` trait that captures the one capability relay needs — sending a message:

```rust
#[async_trait]
pub trait PeerSender: Send + Sync {
    async fn send(&self, msg: PeerDataMessage) -> Result<(), String>;
}
```

Two concrete implementations:

- **`SocketPeerSender`** — wraps an `mpsc::Sender<Message>`, converting `PeerDataMessage` to `Message::PeerData` before sending. Used for inbound socket peers.
- For SSH transports, `PeerTransport` gains a blanket or explicit `PeerSender` impl — no separate `SshPeerSender` wrapper. `PeerTransport::send()` already has the right signature. Remove `send()` from `PeerTransport` and make it a `PeerSender` impl instead, keeping `PeerTransport` focused on lifecycle (connect, disconnect, subscribe).

`PeerManager` gains a unified senders map and renames the existing map:

| Map | Type | Purpose |
|-----|------|---------|
| `transports` | `HashMap<HostName, Box<dyn PeerTransport>>` | Lifecycle management (connect, disconnect, subscribe). SSH-specific today; future transports later. |
| `senders` | `HashMap<HostName, Arc<dyn PeerSender>>` | Messaging. All peers, regardless of transport. Used by `relay()` and `send_to()`. |

Lifecycle:

- **SSH peer connects:** wrap its outbound `mpsc::Sender` in an `Arc<dyn PeerSender>`, register via `register_sender()`.
- **Socket peer connects:** wrap its `mpsc::Sender<Message>` in `Arc<SocketPeerSender>`, register via `register_sender()`.
- **Either disconnects:** call `unregister_sender()`.

`send_local_to_peers()` in `server.rs` simplifies: remove the `peer_clients` parameter and the separate `PeerClientMap` send loop. All sends go through `pm.senders()`. The `PeerClientMap` type is removed — connection ID tracking moves into PeerManager's generation counter (#263).

### Files changed

- `crates/flotilla-daemon/src/peer/transport.rs` — add `PeerSender` trait; remove `send()` from `PeerTransport`
- `crates/flotilla-daemon/src/peer/manager.rs` — rename `peers` to `transports`; add `senders` map, `register_sender()`, `unregister_sender()`, `senders()` accessor; `relay()` and `send_to()` use `senders`
- `crates/flotilla-daemon/src/peer/ssh_transport.rs` — implement `PeerSender` for the outbound channel wrapper
- `crates/flotilla-daemon/src/server.rs` — add `SocketPeerSender`; register/unregister senders on peer connect/disconnect; simplify `send_local_to_peers()` (remove `peer_clients` parameter and send loop); remove `PeerClientMap` type

### Tests

- Existing relay tests in `manager.rs` must be updated: `MockTransport` implements `PeerSender`; tests call `register_sender()` alongside `add_peer()` so relay finds senders.
- New test: register a mock sender via `register_sender()` (without a transport), verify relay reaches it.
- New test: unregister a sender, verify relay skips it.
- New test: `send_to()` reaches a socket-only peer registered via `register_sender()`.

---

## #264: Head-of-line blocking in relay

### Problem

`relay()` sends to peers sequentially. A slow peer blocks all subsequent peers.

### Design

Collect sender references and clone messages while holding `&self`, then drop the borrow and send concurrently. This matters because `PeerManager` sits behind `Arc<Mutex<PeerManager>>` — holding the lock across async sends would block other tasks.

Pattern for `relay()`:

```rust
pub fn prepare_relay(&self, origin: &HostName, msg: &PeerDataMessage)
    -> Vec<(HostName, Arc<dyn PeerSender>, PeerDataMessage)>
{
    let mut relayed_msg = msg.clone();
    relayed_msg.clock.tick(&self.local_host);

    self.senders.iter()
        .filter(|(name, _)| {
            *name != origin
                && *name != &self.local_host
                && msg.clock.get(name) == 0
        })
        .map(|(name, sender)| {
            (name.clone(), Arc::clone(sender), relayed_msg.clone())
        })
        .collect()
}
```

The caller in `server.rs` calls `prepare_relay()` under the lock, drops the lock, then fans out sends concurrently:

```rust
let relays = pm.prepare_relay(&origin, &msg);
drop(pm); // release Mutex before async sends

let futures = relays.into_iter().map(|(name, sender, msg)| async move {
    if let Err(e) = sender.send(msg).await {
        warn!(to = %name, err = %e, "failed to relay peer data");
    }
});
futures::future::join_all(futures).await;
```

Apply the same collect-then-send pattern to `send_local_to_peers()`.

### Dependency

Add `futures` (or `futures-util`) to `flotilla-daemon/Cargo.toml`.

### Files changed

- `crates/flotilla-daemon/src/peer/manager.rs` — add `prepare_relay()` method
- `crates/flotilla-daemon/src/server.rs` — concurrent relay dispatch; concurrent `send_local_to_peers()`
- `crates/flotilla-daemon/Cargo.toml` — add `futures`

### Tests

Existing relay tests need updating (they called `relay()` directly). The new `prepare_relay()` returns data to assert on — test that the correct peers are included/excluded. Sending behavior is tested via mock senders.

---

## #259: Protocol version handshake

### Problem

Daemons at different protocol versions silently exchange incompatible data, producing undefined behavior.

### Design

Add a constant and a new `Message` variant:

```rust
// flotilla-protocol/src/lib.rs
pub const PROTOCOL_VERSION: u32 = 1;

// In the Message enum:
#[serde(rename = "hello")]
Hello {
    protocol_version: u32,
    host_name: HostName,
}
```

**Outbound (SSH transport):** The Hello handshake happens *before* spawning reader/writer tasks. After `connect_socket()` opens the `UnixStream` but before splitting it into read/write halves and spawning tasks:
1. Write `Message::Hello` as a JSON line to the stream.
2. Read one JSON line from the stream. If it's a `Hello` with a matching version, proceed to spawn reader/writer tasks. Otherwise, close the stream and return an error.

This requires restructuring `connect_socket()`: do the handshake on the raw stream first, then split and spawn tasks.

**Inbound (socket peer):** In `handle_client`, the first message from a connecting client must be `Hello`. The server responds with its own `Hello`. On version mismatch, log a warning and close the connection. This replaces the current implicit peer identification (extracting `origin_host` from the first `PeerData` message in lines 830-846 of `server.rs`) — `Hello` carries the `host_name` explicitly upfront.

Bump `PROTOCOL_VERSION` whenever the wire format changes incompatibly. No migration logic needed (per project conventions).

### Files changed

- `crates/flotilla-protocol/src/lib.rs` — `PROTOCOL_VERSION` constant, `Message::Hello` variant
- `crates/flotilla-daemon/src/peer/ssh_transport.rs` — restructure `connect_socket()` to handshake before spawning tasks
- `crates/flotilla-daemon/src/server.rs` — handle `Hello` from inbound peers; replace implicit `origin_host` identification with explicit `Hello` exchange

### Tests

- Serde roundtrip test for `Message::Hello`.
- Unit test: version mismatch produces error.
- Integration test in `multi_host.rs`: mock transport that sends wrong version, verify connection fails.

---

## #263: Cleanup race on rapid reconnect

### Problem

When a peer disconnects, `clear_peer_data()` removes its stored data and rebuilds overlays. If the peer reconnects quickly and sends new data before cleanup runs, the stale cleanup wipes fresh data.

### Design

Add a generation counter to `PeerManager`:

```rust
generations: HashMap<HostName, u64>,
```

`register_sender()` increments the generation for that host and returns the new value. The caller captures this generation at connect time.

`remove_peer_data()` takes the generation it's cleaning up for:

```rust
pub fn remove_peer_data(&mut self, name: &HostName, generation: u64) -> Vec<RepoIdentity> {
    if self.generations.get(name).copied().unwrap_or(0) != generation {
        debug!(peer = %name, "skipping stale cleanup (generation mismatch)");
        return vec![];
    }
    // ... existing cleanup logic
}
```

Both peer connection paths capture and use generations:

- **SSH outbound peers:** The reconnect loop in `server.rs` captures the generation from `register_sender()` when the connection establishes. On disconnect, passes that generation to `clear_peer_data()`. A stale cleanup (from an earlier generation) becomes a no-op.
- **Inbound socket peers:** When `handle_client` registers a socket peer sender via `register_sender()`, it captures the generation. On client disconnect, it calls `clear_peer_data()` with that generation. This replaces the current `PeerClientMap` connection ID scheme — the generation counter on `PeerManager` serves the same purpose.

### Files changed

- `crates/flotilla-daemon/src/peer/manager.rs` — `generations` map, generation-aware `register_sender()` and `remove_peer_data()`
- `crates/flotilla-daemon/src/server.rs` — capture generation on both SSH and socket peer connect; pass to cleanup on disconnect

### Tests

- Register a sender (generation 1), register again (generation 2, simulating reconnect), call `remove_peer_data` with generation 1 — verify no data is removed.
- Register, remove with matching generation — verify data is removed (existing behavior preserved).
- Test both paths: SSH reconnect scenario and socket peer reconnect scenario.
