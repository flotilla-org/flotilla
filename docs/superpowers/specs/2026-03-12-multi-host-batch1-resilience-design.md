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

`PeerTransport` loses its `send()` method — it keeps only lifecycle methods (`connect`, `disconnect`, `subscribe`). Sending is now exclusively through `PeerSender`.

Two concrete `PeerSender` implementations:

- **`ChannelPeerSender`** — wraps an `mpsc::Sender<PeerDataMessage>`. Used for SSH transports: `SshTransport::connect()` creates the outbound channel internally, then exposes a method (e.g. `sender()`) that returns a cloneable `mpsc::Sender<PeerDataMessage>`. The caller wraps it in `ChannelPeerSender` and registers it. `SshTransport` itself does not implement `PeerSender`.
- **`SocketPeerSender`** — wraps an `mpsc::Sender<Message>`, converting `PeerDataMessage` to `Message::PeerData` before sending. Used for inbound socket peers.

Both are thin wrappers around cloneable channel senders. The transport owns the channel; the `PeerSender` holds a clone.

`PeerManager` gains a unified senders map and renames the existing map:

| Map | Key | Type | Purpose |
|-----|-----|------|---------|
| `transports` | Config label | `HashMap<String, Box<dyn PeerTransport>>` | Lifecycle management (connect, disconnect, subscribe). Keyed by hosts.toml label, not peer identity. SSH-specific today; future transports later. |
| `senders` | Canonical name | `HashMap<HostName, Arc<dyn PeerSender>>` | Messaging. All peers, regardless of transport. Keyed by `Hello.host_name`. Used by `prepare_relay()` and `send_to()`. |
| `transport_peers` | Config label → canonical name | `HashMap<String, HostName>` | Mapping established after Hello. Lets the reconnect loop find the canonical name to clean up when a transport-managed connection drops. |

Lifecycle:

- **SSH peer connects:** transport does Hello handshake, returns canonical name. Caller wraps the outbound channel in `Arc<ChannelPeerSender>`, registers via `register_sender(canonical_name)`, and stores the config-label → canonical-name mapping.
- **Socket peer connects (after Hello handshake — see #259):** wrap its `mpsc::Sender<Message>` in `Arc<SocketPeerSender>`, register via `register_sender(hello.host_name)`.
- **Either disconnects:** call `disconnect_peer(canonical_name, generation)` (#263).

`send_local_to_peers()` in `server.rs` simplifies: remove the `peer_clients` parameter and the separate `PeerClientMap` send loop. All sends go through `pm.senders()`. The `PeerClientMap` type is removed — connection tracking moves into PeerManager's generation counter (#263).

### Files changed

- `crates/flotilla-daemon/src/peer/transport.rs` — add `PeerSender` trait; remove `send()` from `PeerTransport`
- `crates/flotilla-daemon/src/peer/manager.rs` — rename `peers` to `transports`; add `senders` map, `register_sender()`, `senders()` accessor; `send_to()` uses `senders`
- `crates/flotilla-daemon/src/peer/ssh_transport.rs` — add `sender()` method; add `ChannelPeerSender` implementing `PeerSender`
- `crates/flotilla-daemon/src/server.rs` — add `SocketPeerSender`; register/unregister senders on peer connect/disconnect; simplify `send_local_to_peers()` (remove `peer_clients` parameter and send loop); remove `PeerClientMap` type

### Tests

- Existing relay tests in `manager.rs` must be updated: `MockTransport` no longer has `send()`; tests register a mock `PeerSender` via `register_sender()` so relay finds senders.
- New test: register a mock sender via `register_sender()` (without a transport), verify relay reaches it.
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

### Session model

The daemon socket is shared between TUI clients and peer connections. `Hello` acts as a **mode switch**: the first message a client sends determines its role.

- If the first message is `Message::Hello` → peer mode. Server responds with its own `Hello`, checks version compatibility, and enters the peer data exchange loop. No non-peer traffic is sent until the `Hello` exchange completes.
- If the first message is `Message::Request` → normal TUI client. Handled as today, no `Hello` required.

This means `handle_client` reads one message, branches on its type, and enters the appropriate handler loop.

### Identity rule

Each host owns its identity. A host's canonical name comes from its `daemon.toml` `host_name` setting (or OS hostname as fallback), advertised via `Hello.host_name`. This is the name used for peer messaging state: senders, generations, peer data, dedup clocks, UI display.

The `hosts.toml` key is a **connection config label** — it names the SSH connection config, not the peer's identity. Once connected, the remote's self-advertised `Hello.host_name` becomes the canonical key for that peer. If you want a host to appear as "build-box," configure `host_name = "build-box"` in that host's `daemon.toml`.

This creates two identity layers:

| Layer | Key | Scope | Lifetime |
|-------|-----|-------|----------|
| **Connection config** | hosts.toml label | `transports` map, reconnect loop | Static — exists before Hello |
| **Peer identity** | `Hello.host_name` | `senders`, `peer_data`, `generations`, `last_seen_clocks` | Established at Hello time |

For outbound SSH, `PeerManager` stores a mapping from config label → canonical name after Hello completes. The reconnect loop uses the config label to find the transport, performs the handshake, learns the canonical name, and registers the sender under it. If the canonical name changes on reconnect (remote reconfigured), the old-generation cleanup removes state under the old name, and the new connection registers under the new name.

For inbound socket peers, there is no config label — only the canonical name from Hello.

**Duplicate identity — supersede, not reject:** If a Hello arrives with a `host_name` that's already registered (from a different connection), the new connection **supersedes** the old one. This is consistent with #263's generation model — `register_sender()` bumps the generation, and the old connection's eventual cleanup no-ops because its generation is stale. This handles the rapid-reconnect case correctly: the replacement connection is accepted, not refused.

**PeerData messages:** No origin_host validation or rewriting. `PeerDataMessage.origin_host` is accepted as-is on all messages. Direct messages carry the connection peer's name; relayed messages carry a third party's name. Both are legitimate. Vector clock dedup already prevents replays and loops. This keeps the system simple and works correctly in mesh topologies where the first time you hear about a peer may be through relay.

### Handshake mechanics

**Outbound (SSH transport):** The Hello handshake happens *before* spawning reader/writer tasks. After `connect_socket()` opens the `UnixStream` but before splitting it into read/write halves and spawning tasks:
1. Write `Message::Hello` as a JSON line to the stream.
2. Read one JSON line from the stream. If it's a `Hello` with a matching version, proceed to spawn reader/writer tasks. Otherwise, close the stream and return an error.
3. Return the remote's `Hello.host_name` to the caller. The caller uses this (not the hosts.toml config key) to register the sender and key all peer state.

This requires restructuring `connect_socket()`: do the handshake on the raw stream first, then split and spawn tasks.

**Inbound (socket peer):** In `handle_client`, after reading the first message and identifying it as `Hello`:
1. Check version. On mismatch, log a warning and close the connection (no error message — the remote sees EOF and will reconnect with backoff).
2. Respond with the server's own `Hello`.
3. Register the peer sender (#262) using the advertised `host_name`. If the name is already registered, the new connection supersedes the old one (generation bump per #263).
4. Enter the peer data forwarding loop.

This replaces the current implicit peer identification (extracting `origin_host` from the first `PeerData` message).

### Files changed

- `crates/flotilla-protocol/src/lib.rs` — `PROTOCOL_VERSION` constant, `Message::Hello` variant
- `crates/flotilla-daemon/src/peer/ssh_transport.rs` — restructure `connect_socket()` to handshake before spawning tasks
- `crates/flotilla-daemon/src/server.rs` — `handle_client` branches on first message type; `Hello` path does version check, responds, registers peer sender, enters peer loop

### Tests

- Serde roundtrip test for `Message::Hello`.
- Unit test: version mismatch produces error.
- Integration test in `multi_host.rs`: mock transport that sends wrong version, verify connection fails.

---

## #263: Cleanup race on rapid reconnect

### Problem

When a peer disconnects, `clear_peer_data()` removes its stored data and rebuilds overlays. If the peer reconnects quickly and sends new data before cleanup runs, the stale cleanup wipes fresh data. The same race applies to sender cleanup — `unregister_sender()` from an old connection would remove the live sender registered by the new connection.

### Design

Add a generation counter to `PeerManager`:

```rust
generations: HashMap<HostName, u64>,
```

`register_sender()` increments the generation for that host and returns the new value. The caller captures this generation at connect time.

Combine sender and data cleanup into a single generation-guarded method:

```rust
pub fn disconnect_peer(&mut self, name: &HostName, generation: u64) -> Vec<RepoIdentity> {
    if self.generations.get(name).copied().unwrap_or(0) != generation {
        debug!(peer = %name, "skipping stale disconnect (generation mismatch)");
        return vec![];
    }
    // Remove sender
    self.senders.remove(name);
    // Remove peer data and last-seen clocks
    // ... existing cleanup logic from remove_peer_data
}
```

Both sender removal and data removal happen atomically under the same generation check. A stale disconnect (from an earlier generation) is a complete no-op — it touches neither the sender nor the data.

Both peer connection paths capture and use generations:

- **SSH outbound peers:** The reconnect loop in `server.rs` captures the generation from `register_sender()` when the connection establishes. On disconnect, passes that generation to `disconnect_peer()`. A stale cleanup becomes a no-op.
- **Inbound socket peers:** When `handle_client` registers a socket peer sender via `register_sender()`, it captures the generation. On client disconnect, it calls `disconnect_peer()` with that generation. This replaces the current `PeerClientMap` connection ID scheme — the generation counter on `PeerManager` serves the same purpose.

### Files changed

- `crates/flotilla-daemon/src/peer/manager.rs` — `generations` map, `disconnect_peer()` replacing `remove_peer_data()` and `unregister_sender()`
- `crates/flotilla-daemon/src/server.rs` — capture generation on both SSH and socket peer connect; pass to `disconnect_peer()` on disconnect; update `clear_peer_data()` to use `disconnect_peer()`

### Tests

- Register a sender (generation 1), register again (generation 2, simulating reconnect), call `disconnect_peer` with generation 1 — verify neither sender nor data is removed.
- Register, disconnect with matching generation — verify both sender and data are removed (existing behavior preserved).
- Verify the sender registered at generation 2 is still functional after stale generation 1 disconnect.
- Test both paths: SSH reconnect scenario and socket peer reconnect scenario.
