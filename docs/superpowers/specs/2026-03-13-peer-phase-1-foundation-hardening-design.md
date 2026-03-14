# Peer Phase 1 — Foundation Hardening

**Issues**: #292, #259, #264, #258 (closed #262 — already resolved)
**Date**: 2026-03-13

## Overview

Four changes to the peer networking layer, implemented sequentially. Each builds on the previous.

1. Extract peer networking into a reusable component (#292)
2. Add session identity to the protocol handshake (#259)
3. Eliminate head-of-line blocking in relay (#264)
4. Detect remote daemon restarts and add keepalive (#258)

## 1. PeerNetworkingTask Extraction (#292)

### Problem

`DaemonServer::run()` contains ~400 lines of inline peer networking (lines 215-624): SSH connection loops, inbound message processing, and outbound snapshot broadcasting. Supporting free functions (`disconnect_peer_and_rebuild`, `rebuild_peer_overlays`, `dispatch_resync_requests`, `forward_until_closed`, `send_local_to_peers`, `send_local_to_peer`, `PeerConnectedNotice`) add another ~200 lines, bringing the total extraction to ~600 lines of `server.rs`. Embedded mode (`--embedded`) bypasses `DaemonServer` entirely, so it has no peer networking.

### Design

Extract a `PeerNetworkingTask` struct in `crates/flotilla-daemon/src/peer_networking.rs`.

```rust
pub struct PeerNetworkingTask {
    daemon: Arc<InProcessDaemon>,
    config: Arc<ConfigStore>,
    peer_manager: Arc<Mutex<PeerManager>>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_data_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
}
```

**Constructor** loads `hosts.toml`, creates `PeerManager` with the daemon's host name, and registers SSH transports for each configured peer. Returns `(PeerNetworkingTask, Arc<Mutex<PeerManager>>, mpsc::Sender<InboundPeerEnvelope>)` so `DaemonServer` can pass them to socket client handlers.

**`spawn(self) -> JoinHandle<()>`** moves the three task groups from `DaemonServer::run()`:

1. **Per-peer SSH connection loops** — connect, forward inbound messages, reconnect with exponential backoff, emit `PeerStatusChanged` events.
2. **Inbound processor** — consume inbound messages, relay to other peers, handle locally (store snapshots, update overlays, process resyncs).
3. **Outbound broadcaster** — subscribe to daemon events, send local provider data to peers when it changes, with version gating to prevent feedback loops.

### Integration

**DaemonServer**: Creates `PeerNetworkingTask`, calls `spawn()`, keeps the returned `peer_manager` and `peer_data_tx` for socket client handling. `DaemonServer` continues to accept inbound peer connections via the Unix socket (`handle_client` Hello branch) and feeds messages into `peer_data_tx`. `PeerNetworkingTask` consumes from `peer_data_rx` and processes all inbound peer messages regardless of whether they arrived via SSH or socket. The `run()` method shrinks to socket listening/acceptance, idle timeout, and SIGTERM handling.

**Embedded mode** (`src/main.rs`): Loads `daemon.toml` for the host name *before* constructing the daemon, then calls `InProcessDaemon::new_with_options()` with the configured host name (not `::new()` which hardcodes `HostName::local()`). Then spawns `PeerNetworkingTask`.

**TUI**: No changes — already handles `PeerStatusChanged`, `RepoAdded`, and snapshot events.

### Host name in embedded mode

`InProcessDaemon::new()` hardcodes `HostName::local()`. Embedded mode must load `~/.config/flotilla/daemon.toml` *before* constructing the daemon and pass the configured host name to `InProcessDaemon::new_with_options()`, matching what `DaemonServer::new()` does. Without this, peers cannot identify the embedded instance.

## 2. Protocol Version Handshake + Session ID (#259)

### Problem

Peers exchange a `Hello` message with `protocol_version` and `host_name`, but have no way to detect a remote daemon restart through a surviving SSH tunnel.

### Design

**Hello message gains a session ID:**

```rust
Message::Hello {
    protocol_version: u32,
    host_name: HostName,
    session_id: Uuid,        // random, generated once at daemon startup
}
```

`session_id` is a `uuid::Uuid` created at `InProcessDaemon` construction and stored as a field. It serves both version handshake (#259) and restart detection (#258).

**PROTOCOL_VERSION** bumps from 1 to 2. Strict equality — mismatches reject immediately. No negotiation or backwards compatibility (we are in a no-backwards-compat phase).

**New dependency**: `uuid` crate with `v4` feature.

**Validation**: Both `ssh_transport.rs` (outbound) and `handle_client()` (inbound socket) validate protocol version during Hello exchange. On mismatch, the connection closes with a log warning.

**Session ID return path**: `connect_socket()` currently returns `Result<mpsc::Receiver<PeerWireMessage>, String>`. It will return `Result<(mpsc::Receiver<PeerWireMessage>, Uuid), String>` to surface the remote session ID. This change ripples through the `PeerTransport` trait: `connect()` and/or `subscribe()` gain a way to expose the session ID. `ChannelTransport` adapts accordingly (it can use a fixed/provided UUID since channels don't perform Hello exchange).

**New peer state variant** for TUI visibility:

```rust
PeerConnectionState::Rejected { reason: String }
```

Surfaces in the peer status display as e.g. "protocol mismatch (local=2, remote=1)". Note: `PeerConnectionState` currently derives `Copy`; adding a `String` field removes `Copy` (retains `Clone`).

**Session ID storage**: `ActiveConnection` gains a `session_id: Uuid` field so reconnect logic can compare old vs new.

## 3. Head-of-Line Blocking Fix (#264)

### Problem

`relay()` in `PeerManager` calls `sender.send().await` sequentially for each peer while holding the `PeerManager` lock. A slow peer blocks relay to all others.

### Design

Split relay into two steps: snapshot targets under the lock, send concurrently outside it. The key mechanism: `prepare_relay()` is synchronous (no `.await`), unlike the old `relay()` which was async because it called `sender.send().await` inside the lock. Moving the async sends outside the lock is what eliminates HOL blocking.

**New method** on `PeerManager`:

```rust
pub fn prepare_relay(
    &self,
    origin: &HostName,
    msg: &PeerDataMessage,
) -> Vec<(HostName, Arc<dyn PeerSender>, PeerDataMessage)> {
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

**Calling pattern** in the inbound processor (inside `PeerNetworkingTask`):

```rust
let relay_targets = {
    let pm = peer_manager.lock().await;
    pm.prepare_relay(&origin, msg)
};
// Lock released — send concurrently with per-peer timeout
let sends = relay_targets.into_iter().map(|(name, sender, msg)| {
    async move {
        match tokio::time::timeout(
            Duration::from_secs(5),
            sender.send(PeerWireMessage::Data(msg)),
        ).await {
            Ok(Ok(())) => { /* relayed */ }
            Ok(Err(e)) => warn!(to = %name, err = %e, "relay send failed"),
            Err(_) => warn!(to = %name, "relay send timed out"),
        }
    }
});
futures::future::join_all(sends).await;
```

**Old `relay()` removed**, replaced by `prepare_relay()`.

**`TestNetwork`** updates to use `prepare_relay()` + direct sends in `process_peer()`. The adaptation is simpler than production code since `TestNetwork` owns managers directly (no `Arc<Mutex<>>` lock to release).

**Per-peer timeout**: 5 seconds. Timed-out messages are dropped — the peer will receive the next snapshot. A warning is logged.

**New dependency**: `futures` crate (for `join_all`).

## 4. Restart Detection + Keepalive (#258)

### Problem

When a remote daemon restarts, the SSH tunnel often survives. The local side sees no EOF and continues sending to a dead connection. Even when reconnection succeeds, stale data from the old daemon instance may persist.

### Session ID comparison

On reconnect, compare the received `session_id` against the stored one:

- **Same session ID**: Network blip. Resume with resync.
- **Different session ID**: Remote daemon restarted. Clear all peer data for that host (it is stale), then activate the new connection and request full resync.

The reconnect task in `PeerNetworkingTask` checks old vs new session ID and calls `disconnect_peer_and_rebuild` before activating the new connection when the session ID differs.

### Application-level keepalive

New wire messages:

```rust
PeerWireMessage::Ping { timestamp: u64 }
PeerWireMessage::Pong { timestamp: u64 }
```

**Sending**: `PeerNetworkingTask` sends `Ping` to each connected peer every 30 seconds.

**Receiving**: Pings are handled directly in the reader/transport layer, not routed through the inbound processor. A `Pong` is sent immediately on receiving a `Ping`. This requires the reader task to have access to the outbound sender (it does not today), so the reader task gains an `outbound_tx` parameter for Pong responses. This minimizes Pong latency, which matters for keepalive accuracy.

**Liveness tracking**: The per-peer reconnect task tracks `last_message_at`, updated on any inbound message (not just Pongs). If no message arrives within 90 seconds, the connection is dead — trigger disconnect + reconnect cycle.

**Timeout**: 90 seconds (3 missed pings). Active data flow suppresses unnecessary ping cycles since `last_message_at` updates on every message. A keepalive timeout resets the backoff counter (`attempt = 1`) since it is a fresh detection, not a repeated failure.

## Implementation Order

| Step | Issue | Scope |
|------|-------|-------|
| 1 | #292 | Extract `PeerNetworkingTask`, integrate in `DaemonServer` and embedded mode |
| 2 | #259 | Session ID in Hello, protocol version bump, `Rejected` state |
| 3 | #264 | `prepare_relay()` + concurrent sends outside lock |
| 4 | #258 | Session ID comparison on reconnect, Ping/Pong keepalive |

Each step produces a working system. No step depends on a future step.

## Files Changed

| File | Changes |
|------|---------|
| `crates/flotilla-daemon/src/peer_networking.rs` | **New** — `PeerNetworkingTask` struct and `spawn()` |
| `crates/flotilla-daemon/src/server.rs` | Remove inline peer tasks, delegate to `PeerNetworkingTask` |
| `crates/flotilla-daemon/src/peer/manager.rs` | Add `prepare_relay()`, remove `relay()`, store `session_id` in `ActiveConnection` |
| `crates/flotilla-daemon/src/peer/ssh_transport.rs` | Return session ID from handshake, handle Ping/Pong in reader |
| `crates/flotilla-daemon/src/peer/transport.rs` | `PeerTransport` trait gains session ID on connect/subscribe |
| `crates/flotilla-daemon/src/peer/channel_transport.rs` | Conform to `PeerTransport` trait changes (fixed UUID) |
| `crates/flotilla-daemon/src/peer/mod.rs` | Re-export new types |
| `crates/flotilla-daemon/src/peer/test_support.rs` | Update `TestNetwork` for `prepare_relay()` pattern |
| `crates/flotilla-daemon/src/lib.rs` | Re-export `PeerNetworkingTask` from new module |
| `crates/flotilla-protocol/src/lib.rs` | Bump `PROTOCOL_VERSION`, add `session_id` to Hello, add `Rejected` state |
| `crates/flotilla-protocol/src/peer.rs` | Add `Ping`/`Pong` variants to `PeerWireMessage` |
| `crates/flotilla-core/src/in_process.rs` | Store `session_id: Uuid` field |
| `src/main.rs` | Load daemon config, spawn `PeerNetworkingTask` in embedded mode |
| `Cargo.toml` (multiple) | Add `uuid`, `futures` dependencies |

## Not in scope

- Version negotiation or backwards compatibility
- Delta-based peer data sync (still snapshot-only)
- Embedded mode accepting inbound peer connections (socket listening stays in `DaemonServer`)
- TUI rendering changes beyond surfacing `Rejected` state
