# In-Process Peer Transport Design

**Date:** 2026-03-13
**Issue:** #229 (test coverage phase 3)
**Status:** Approved

## Problem

The peer connection layer (`PeerManager`, relay, deduplication, reconnection) is one of the largest coverage gaps in the project (~427 uncovered lines in `server.rs`, ~193 in `manager.rs` at 67%). Testing this code currently requires SSH tunnels and Unix sockets, which makes tests slow, environment-dependent, and hard to write.

The `PeerTransport` and `PeerSender` traits are already clean seams. We need an in-process implementation that lets us test the full peer data flow without network infrastructure.

## Approach

Build a production-grade `ChannelTransport` that implements `PeerTransport` using `tokio::mpsc` channels. Two `ChannelTransport` instances are created as a pair — what one sends, the other receives. This follows the established `CommandRunner` pattern: a real implementation alongside a test-friendly implementation, both in production code.

This is Approach 2 from the brainstorming — not just a test double, but a real transport that could serve future in-process multi-host scenarios.

## Design

### Core: `ChannelTransport`

**File:** `crates/flotilla-daemon/src/peer/channel_transport.rs`

A factory function creates paired endpoints:

```rust
pub fn channel_transport_pair(
    local_name: HostName,
    remote_name: HostName,
) -> (ChannelTransport, ChannelTransport)
```

Each `ChannelTransport` implements `PeerTransport`:

- `connect()` — transitions through `Connecting` to `Connected` (channel is already wired, but matches `SshTransport` status lifecycle)
- `disconnect()` — drops the sender half, marks `Disconnected`
- `subscribe()` — returns the `mpsc::Receiver<PeerWireMessage>` for inbound messages. One-shot per connect cycle: a second call before reconnect returns an error, matching `SshTransport` semantics.
- `sender()` — returns `Option<Arc<dyn PeerSender>>`: `None` before `connect()`, `Some(ChannelSender)` after, `None` again after `disconnect()`
- `status()` — returns current `PeerConnectionStatus`

`ChannelSender` implements `PeerSender`:

- `send()` — writes to the paired endpoint's inbound channel (buffer size matches `SshTransport`'s `CHANNEL_BUFFER = 256`; tests must consume from receivers to avoid backpressure deadlocks)
- `retire(reason: GoodbyeReason)` — sends `Goodbye { reason }` then drops

The Hello handshake is handled by the caller (PeerManager/DaemonServer), not the transport. The transport only moves `PeerWireMessage` values.

### Relationship to existing test infrastructure

`manager.rs` already contains a `MockTransport` used in existing PeerManager tests. `ChannelTransport` differs in that it provides actual bidirectional message flow between paired endpoints, enabling multi-peer integration tests. `MockTransport` remains useful for simpler unit tests that don't need real channel wiring. The existing helpers in `test_support.rs` (`ensure_test_connection_generation`, `handle_test_peer_data`) coexist with the new `TestNetwork` harness — they serve Level A unit tests that work with individual senders.

### Integration

- **Production code, not `cfg(test)`** — a real transport alongside `SshTransport`
- **No changes to `PeerManager`** — already accepts `Box<dyn PeerTransport>`
- **No changes to `DaemonServer`** — tests bypass its construction and inject transports directly into `PeerManager`
- Exported from `peer/mod.rs`

### Test infrastructure

**Expanded `test_support.rs` with `TestNetwork` harness:**

```rust
let mut network = TestNetwork::new();
let host_a = network.add_peer("host-a");
let host_b = network.add_peer("host-b");
network.connect(host_a, host_b);
network.start().await;

network.send_snapshot(host_a, repo, snapshot).await;
let stored = network.peer_data(host_b, "host-a", repo);
assert_eq!(stored, expected);
```

`TestNetwork` manages N logical peers, each with their own `PeerManager`, connected via `ChannelTransport` pairs in configurable topologies. It models the **outbound connection path** — calling `connect()`, `sender()`, `subscribe()`, and `activate_connection()` per peer, then spawning forwarding tasks that replicate the relay-then-handle pattern from `server.rs` (i.e., calling `relay()` before `handle_inbound()` for each received message). The inbound socket path (where `DaemonServer` accepts connections and creates `SocketPeerSender` directly) is not modeled in this phase.

**Level A — PeerManager unit tests:**

- Connection activation (hello handshake)
- Snapshot/delta exchange and storage
- Vector clock deduplication (duplicate messages dropped)
- Relay flooding (3+ node mesh)
- Generation tracking (reconnect rejects old-generation messages)
- Goodbye/retirement flow

**Level B — Multi-peer integration tests:**

- 2-peer direct exchange
- 3-peer relay scenario (A→B→C, verify C receives A's data)

## Scope boundaries

### In scope

- `ChannelTransport` + `ChannelSender` in `peer/channel_transport.rs`
- `TestNetwork` harness in `peer/test_support.rs`
- PeerManager unit tests (Level A)
- Multi-peer topology tests (Level B) — 2-peer and 3-peer scenarios
- Fix any SSH assumptions shaken out along the way

### Out of scope (future layers)

- Failure injection wrapper (`FailureTransport`) — future phase
- Docker-based fault tests — separate track
- Refactoring `DaemonServer` construction to be transport-generic
- Daemon protocol record/replay
- TLA+ / deterministic simulation testing

## Success criteria

- `ChannelTransport` satisfies the same behavioral contract as `SshTransport`
- PeerManager tests cover: connection activation, snapshot exchange, vector clock dedup, relay, generation tracking, goodbye flow
- Multi-peer test demonstrates at least a 3-node relay scenario
- No production behavior changes — purely additive
