# Multi-Host Phase 2 Batch 1: Resilience Hardening

Addresses four peer-relay / connection-management issues:
[#259](https://github.com/rjwittams/flotilla/issues/259),
[#262](https://github.com/rjwittams/flotilla/issues/262),
[#263](https://github.com/rjwittams/flotilla/issues/263),
[#264](https://github.com/rjwittams/flotilla/issues/264).

This draft replaces the earlier patch-by-patch version with one coherent model:

- one peer wire message plane
- one connection activation path
- one routing model for targeted control traffic
- one authority model for state acceptance and cleanup

## Scope

Batch 1 introduces:

- protocol-version `Hello`
- unified peer sending for outbound SSH peers and inbound socket peers
- routed control traffic for resync
- generation-guarded connection ownership and cleanup
- route failover with stale-state retention
- concurrent fanout for flooded relay sends

Batch 1 does not introduce:

- cryptographic proof of third-party authorship
- arbitrary mesh trust promotion based only on fresher claims
- delta application beyond the existing resync fallback behavior

## Core Types

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConfigLabel(pub String);

pub const PROTOCOL_VERSION: u32 = 1;
```

`HostName` remains the canonical protocol identity of a daemon.

The user-facing configured transport label is `ConfigLabel`, not `HostName`.

## Message Model

### Top-level Wire Protocol

Add two new `Message` variants:

```rust
#[serde(tag = "type")]
pub enum Message {
    // existing variants...
    #[serde(rename = "hello")]
    Hello {
        protocol_version: u32,
        host_name: HostName,
    },
    #[serde(rename = "peer")]
    Peer(Box<PeerWireMessage>),
}
```

`Message::Peer` is the single peer-to-peer wire envelope.

### Peer Wire Messages

```rust
#[serde(tag = "peer_type")]
pub enum PeerWireMessage {
    Data(PeerDataMessage),
    Routed(RoutedPeerMessage),
}

#[serde(tag = "routed_type")]
pub enum RoutedPeerMessage {
    RequestResync {
        request_id: u64,
        requester_host: HostName,
        target_host: HostName,
        remaining_hops: u8,
        repo_identity: RepoIdentity,
        since_seq: u64,
    },
    ResyncSnapshot {
        request_id: u64,
        requester_host: HostName,
        responder_host: HostName,
        remaining_hops: u8,
        repo_identity: RepoIdentity,
        repo_path: PathBuf,
        clock: VectorClock,
        seq: u64,
        data: Box<ProviderData>,
    },
}
```

`PeerWireMessage` should use an explicit tagged serde representation so
`Message::Peer` round-trips deterministically; add an early protocol roundtrip
test for nested enum serialization/deserialization.

Semantics:

- `PeerDataMessage` is the existing flooded replication message type already defined in
  `crates/flotilla-protocol/src/peer.rs`; Batch 1 reuses it unchanged inside
  `PeerWireMessage::Data`.
- `PeerDataMessage.origin_host` means the host whose replicated state this message describes.
- `RequestResync.target_host` is the final destination of the request.
- `RequestResync.requester_host` is where the response must return.
- `ResyncSnapshot.responder_host` is the host that produced the snapshot.
- `ResyncSnapshot.requester_host` is the final return destination.
- `remaining_hops` is a routed-control loop guard. Each forwarding hop decrements it; messages
  are dropped when it reaches zero.

## Delivery Model

Batch 1 has two peer traffic classes:

- `PeerWireMessage::Data(PeerDataMessage)`
  - `Snapshot` and `Delta` use flooded relay with vector-clock dedup
  - forwarding and local acceptance are separate decisions
- `PeerWireMessage::Routed(RoutedPeerMessage)`
  - `RequestResync` and `ResyncSnapshot` are targeted control traffic
  - they are not flood-relayed

### Flood Forwarding vs Local Acceptance

For flooded state traffic:

- forwarding is governed by vector-clock relay rules
- local acceptance into `peer_data` is governed by route authority rules
- relay dedup state and accepted-state authority state are tracked separately

A node may forward a state message that it does not locally accept into `peer_data`.
This keeps flood topology simple while making route trust a local authority decision.

Concretely:

- relay dedup may advance when a flooded message is forwarded
- accepted-state clocks / provenance only advance when the message is authority-accepted
- a rejected-but-forwarded message must not update stored repo state, provenance, or
  accepted-state freshness for that `origin_host`

## #262: Unified Send Path

### Problem

`PeerManager::relay()` and `send_to()` currently only reach outbound SSH peers. Inbound socket peers are tracked separately in `server.rs`, so relay and resync responses can be lost.

### Design

Unify all peer sending behind one sender trait and one peer wire envelope.

```rust
#[async_trait]
pub trait PeerSender: Send + Sync {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String>;
}
```

`PeerTransport` keeps only lifecycle methods:

```rust
#[async_trait]
pub trait PeerTransport: Send + Sync {
    async fn connect(&mut self) -> Result<(), String>;
    async fn disconnect(&mut self) -> Result<(), String>;
    fn status(&self) -> PeerConnectionStatus;
    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String>;
}
```

Concrete senders:

- `ChannelPeerSender`
  - wraps `mpsc::Sender<PeerWireMessage>`
  - used by outbound SSH transports
- `SocketPeerSender`
  - wraps `mpsc::Sender<Message>`
  - converts `PeerWireMessage` into `Message::Peer`
  - used by inbound socket peers

### PeerManager State

| Map | Key | Value | Purpose |
|-----|-----|-------|---------|
| `transports` | `ConfigLabel` | `Box<dyn PeerTransport>` | lifecycle management for configured transports |
| `senders` | `HostName` | `Arc<dyn PeerSender>` | active sender for the canonical peer identity |
| `transport_peers` | `ConfigLabel` | `HostName` | current config-label -> canonical-host mapping for reconnect/status only |
| `routes` | `HostName` | `RouteState` | authoritative next-hop state for targeted control traffic |
| `reverse_paths` | `ReversePathKey` | `ReversePathHop` | transient request-scoped reply routing for `ResyncSnapshot` |
| `pending_resync_requests` | `ReversePathKey` | `PendingResyncRequest` | requester-owned timeout/cancel tracking for routed resync |

`transport_peers` is not the source of truth for retiring a live connection. A specific connection is retired using its captured `(HostName, generation)`.

### Activation Lifecycle

Both inbound and outbound peers go through the same method:

```rust
pub enum ConnectionDirection {
    Inbound,
    Outbound,
}

pub struct ConnectionMeta {
    pub direction: ConnectionDirection,
    pub config_label: Option<ConfigLabel>,
    pub expected_peer: Option<HostName>,
}

pub fn activate_connection(
    &mut self,
    host: HostName,
    sender: Arc<dyn PeerSender>,
    meta: ConnectionMeta,
) -> u64
```

`activate_connection(...)`:

1. applies the single-active-connection arbitration rule
2. retires any displaced connection
3. installs or updates the sender
4. increments the generation for that canonical host
5. returns the new generation

Single-active-connection arbitration rule:

- there is at most one active connection owner per canonical `HostName`
- the pairwise initiator rule determines which side is allowed to run the reconnect loop
- successful duplicate activation for the same canonical `HostName` supersedes the older live
  connection for that host
- superseding a connection always retires the displaced reader/writer tasks rather than leaving a
  stale-but-open standby path behind

There is no separate generation-returning `register_sender()` in the final design.

### `send_to()`

`send_to(target_host, msg)` is only for forward targeted peer traffic.

- if `target_host` is directly connected, send to that peer
- otherwise use `routes[target_host].primary`
- if neither a direct sender nor a route exists, return an error to the caller

`send_to()` does not send reverse-path replies. `ResyncSnapshot` uses `reverse_paths`.

### Files Changed

- `crates/flotilla-protocol/src/lib.rs`
  - add `ConfigLabel`, `PROTOCOL_VERSION`, `Message::Hello`, `Message::Peer`
  - add `PeerWireMessage`, `RoutedPeerMessage`
- `crates/flotilla-core/src/config/...`
  - transport config records expected canonical peer `HostName`
- `crates/flotilla-daemon/src/peer/transport.rs`
  - add `PeerSender`, remove transport send method
- `crates/flotilla-daemon/src/peer/manager.rs`
  - unified sender map
  - `activate_connection(...)`
  - `send_to(...)`
  - route / reverse-path state
- `crates/flotilla-daemon/src/peer/ssh_transport.rs`
  - sender channel emits `PeerWireMessage`
- `crates/flotilla-daemon/src/server.rs`
  - inbound socket peers use `SocketPeerSender`
  - route `Message::Peer`
  - remove `PeerClientMap`

Config migration:

- `hosts.<label>` remains the user-facing `ConfigLabel`
- existing `hostname` remains the network address / SSH host to dial
- add `expected_host_name` for the canonical daemon `HostName` used by the
  pairwise initiator rule, Hello validation, and status/routing identity

### Tests

- `Message::Peer(PeerWireMessage)` serde roundtrips for both `Data` and `Routed` variants
- relay reaches a registered inbound-only peer sender
- `send_to()` reaches a socket-only direct peer
- `send_to()` routes a `RequestResync` to a relayed target via `routes[target].primary`
- `ResyncSnapshot` returns using reverse-path state rather than ordinary route authority

## Routing and Authority

### Pairwise Initiator Rule

For peer pair `(A, B)`, only the lexicographically smaller canonical `HostName` initiates the transport connection.

That means:

- the smaller host runs the reconnect loop for the pair
- the larger host accepts inbound from that peer
- a config that would make the larger host initiate toward the smaller host is quiesced as misconfiguration

This replaces the earlier reliance on the narrower Phase 1 leader-hub topology.

### Identity Layers

| Layer | Type | Role |
|-------|------|------|
| connection config | `ConfigLabel` | user-facing transport label |
| expected peer identity | `HostName` | configured remote identity for initiation + Hello validation |
| confirmed peer identity | `HostName` | canonical peer identity after successful Hello |

User-facing semantics:

- transport config / status surfaces show `ConfigLabel`
- peer state, routing, provenance, and protocol identity use canonical `HostName`

### Route State

```rust
pub struct RouteHop {
    pub next_hop: HostName,
    pub next_hop_generation: u64,
    pub learned_epoch: u64,
}

pub struct RouteState {
    pub primary: RouteHop,
    pub fallbacks: Vec<RouteHop>,
    pub candidates: Vec<RouteHop>,
}
```

`RouteState` exists only while at least one viable route exists for that `origin_host`.
If all routes are lost, remove the whole entry rather than keeping a `None` primary.

`learned_epoch` comes from a single monotonic counter in `PeerManager`:

```rust
route_epoch: u64
```

Every accepted route update increments `route_epoch` and stamps the new `RouteHop`.
Because `routes` is keyed by canonical `HostName`, route ranking is also host-scoped.
It must not depend on repo-local `seq` values from unrelated repos on that host.
Route replacement is ordered by:

1. direct-route preference over relayed paths
2. greater `learned_epoch` as the deterministic recency signal for host-level route observations

Direct-route priority:

- a live direct route to `origin_host` is always `primary`
- relay paths are only fallbacks or candidates while the direct route exists
- relay-path ordering uses host-level route observation recency, not repo-level data freshness

### Reverse-Path State

```rust
pub struct ReversePathKey {
    pub request_id: u64,
    pub requester_host: HostName,
    pub target_host: HostName,
    pub repo_identity: RepoIdentity,
}

pub struct ReversePathHop {
    pub next_hop: HostName,
    pub next_hop_generation: u64,
    pub learned_at: u64,
}

pub struct PendingResyncRequest {
    pub deadline_at: Instant,
}
```

`request_id` is generated by the requester from a per-process monotonic `u64` counter.
Generation is stored in `ReversePathHop` so reply routing obeys the same authority model as normal route ownership.

Reverse-path entries expire on:

- successful `ResyncSnapshot` delivery
- local TTL expiry aligned with the request timeout budget
- disconnect of the stored next hop
- generation mismatch for the stored next hop

Requester-owned timeout/cancel tracking lives in `pending_resync_requests`. The requester
creates that entry when it emits `RequestResync`; intermediate hops only create
`reverse_paths`. Requester timeout/cancel removes the local pending request immediately;
intermediate hops lazily evict their reverse-path entry once its TTL has expired.

Intermediate forwarding rules:

- on receiving `RequestResync` where `target_host != local_host`:
  - require `remaining_hops > 0`
  - record `reverse_paths[key]` pointing at the upstream sender and its captured generation
  - decrement `remaining_hops`
  - forward the request with `send_to(target_host, PeerWireMessage::Routed(...))`
- on receiving `RequestResync` where `target_host == local_host`:
  - produce `ResyncSnapshot`
- on receiving `ResyncSnapshot` where `requester_host != local_host`:
  - require `remaining_hops > 0`
  - look up `reverse_paths[key]`
  - require the stored `next_hop_generation` to still be current
  - decrement `remaining_hops`
  - forward by direct sender lookup to that `next_hop`
- on receiving `ResyncSnapshot` where `requester_host == local_host`:
  - require a matching `pending_resync_requests` entry to still exist
  - clear the matching `pending_resync_requests` entry
  - apply normal failover-resync acceptance rules

Late or abandoned `ResyncSnapshot` replies are dropped if the matching requester-owned
`pending_resync_requests` entry no longer exists.

### Trust Model for Third-Party State

Batch 1 has no cryptographic proof for third-party authorship. Therefore:

- direct self-claims are accepted normally
- first discovery of an unknown `origin_host` may come from any currently authenticated direct peer
- once `origin_host` is known, only the current `primary` or `fallbacks` may refresh active state or route ownership
- unrelated claimants do not become authoritative immediately

Unrelated but valid observations may be stored as `candidates` only.

An eligible candidate is one learned from:

- a clock-accepted `Snapshot` / `Delta` observation, or
- a gap-detecting `Delta`

and additionally:

- its `next_hop_generation` is still current
- its next hop is still connected
- it does not conflict with a live direct route
- it has been validated by one of:
  - a later authority-accepted state-bearing update through the active route set
  - a direct connection from `origin_host` itself

Candidates do not refresh active state while the current route set is still healthy.
Successful routed resync through an untrusted intermediary does not, by itself, promote that
intermediary to authoritative route ownership.

## #259: Protocol Version Handshake

### Problem

Different protocol versions can silently exchange incompatible data.

### Session Model

The daemon socket is shared between TUI clients and peer connections.

The first inbound message decides the session role:

- `Message::Hello`
  - peer mode
- `Message::Request`
  - normal TUI client
- anything else
  - warn and close

No server-originated traffic may be written on that socket until this role switch completes.
In particular, the daemon must not start event streaming or any other background writer task
until it has classified the connection as:

- TUI client mode after an initial `Message::Request`, or
- peer mode after a successful `Message::Hello` handshake

### Handshake

Outbound transport:

1. open raw stream
2. send `Message::Hello`
3. read one `Message::Hello`
4. require matching protocol version
5. require advertised `host_name` to match the configured expected peer `HostName`
6. call `activate_connection(...)`
7. then split the stream and spawn background tasks using the returned generation

Inbound socket peer:

1. read first message
2. require `Message::Hello`
3. require matching version
4. reply with local `Message::Hello`
5. call `activate_connection(...)`
6. enter peer-message loop

### Files Changed

- `crates/flotilla-protocol/src/lib.rs`
  - `Message::Hello`
- `crates/flotilla-daemon/src/peer/ssh_transport.rs`
  - pre-split Hello handshake
- `crates/flotilla-daemon/src/server.rs`
  - first-message role switch
  - delay event-stream task startup until the socket has been classified as TUI-client mode

Pairwise initiator enforcement happens before reconnect-loop startup for configured outbound
transports. If `local_host` is lexicographically larger than `expected_host_name`, keep the
transport quiesced and warn; do not attempt outbound dialing for that pair.

### Tests

- Hello roundtrip
- version mismatch rejects connection
- expected peer `HostName` mismatch rejects connection
- invalid first-message type closes connection
- no daemon `Event` is emitted on a fresh socket before Hello/Request classification completes

## #263: Generation-Guarded Ownership and Cleanup

### Problem

Rapid reconnect can let stale cleanup remove the live sender or wipe fresh data. A superseded connection may also keep reading and forwarding messages after it should no longer be authoritative.

### Design

`PeerManager` tracks:

```rust
generations: HashMap<HostName, u64>,
request_id_counter: u64,
route_epoch: u64,
pending_resync_requests: HashMap<ReversePathKey, PendingResyncRequest>,
```

All peer messages arriving from a connection are tagged with:

```rust
struct InboundPeerEnvelope {
    msg: PeerWireMessage,
    connection_generation: u64,
    connection_peer: HostName,
}
```

Generation gating applies to both:

- flooded state messages
- routed control messages

If the connection generation is stale, drop the message.

`InboundPeerEnvelope` is constructed by the connection-specific reader loop that captured the
generation returned from `activate_connection(...)`:

- outbound SSH subscriber tasks wrap every received `PeerWireMessage`
- inbound socket peer loops wrap every decoded `Message::Peer`

The captured generation is immutable per reader task; it must not be looked up dynamically for
each message.

### Per-Repo State

`PerRepoPeerState` extends the existing struct with provenance and stale tracking:

```rust
pub struct PerRepoPeerState {
    pub provider_data: ProviderData,
    pub repo_path: PathBuf,
    pub seq: u64,
    pub via_peer: HostName,
    pub via_generation: u64,
    pub stale: bool,
}
```

### State Acceptance

Authority-accepted `Snapshot` / `Delta` messages:

- update repo state for `msg.origin_host`
- record provenance from the forwarding connection
- may update route state if the claim passes the trust rules
- create `routes[msg.origin_host]` on first authority-accepted discovery when no route exists yet

Gap-detecting delta:

- may create or refresh a route candidate
- may trigger `RequestResync`
- does not update stored repo state or provenance

Failover-triggered `ResyncSnapshot`:

- may clear `stale`
- may rebind provenance
- may do so even when payload and clock match the retained stale snapshot exactly

This exception exists only for explicit failover resync replies.

### Disconnect Cleanup

```rust
pub struct DisconnectPlan {
    pub affected_repos: Vec<RepoIdentity>,
    pub resync_requests: Vec<RoutedPeerMessage>,
}

pub fn disconnect_peer(&mut self, name: &HostName, generation: u64) -> DisconnectPlan
```

`disconnect_peer()` returns:

- `affected_repos`: repo identities whose overlays / peer-provider views must be rebuilt by the
  caller after cleanup and/or route promotion
- `resync_requests`: concrete routed `RequestResync` messages that the caller must dispatch with
  `send_to(target_host, PeerWireMessage::Routed(...))`

Generation rules:

- generations start at `1`
- `0` is invalid and never issued
- stale disconnects are complete no-ops

Cleanup behavior is driven by remaining reachability of each `origin_host`, not by whether the lost path was direct or relayed.

If a path to `origin_host` is lost:

1. prefer any remaining live direct route
2. otherwise promote the fallback with greatest `observed_seq`, using `learned_epoch` only as a
   tie-break among equally fresh paths
3. otherwise promote the eligible candidate with greatest `observed_seq`, using
   `learned_epoch` only as a tie-break among equally fresh paths
4. if a replacement path exists:
   - retain current snapshot as `stale`
   - allocate a new `request_id`
   - record `pending_resync_requests`
   - return a routed `RequestResync` in `DisconnectPlan.resync_requests`
5. if no replacement path exists:
   - remove route
   - remove active state for that `origin_host`

### Files Changed

- `crates/flotilla-daemon/src/peer/manager.rs`
  - generations
  - `request_id_counter`
  - `route_epoch`
  - `pending_resync_requests`
  - `activate_connection(...)`
  - `disconnect_peer()`
  - reverse-path cleanup
- `crates/flotilla-daemon/src/server.rs`
  - `InboundPeerEnvelope`
  - generation-tagged forwarding for all peer wire messages

### Tests

- stale disconnect does not remove live sender or data
- stale-generation inbound `PeerWireMessage` is dropped
- direct sender ownership is replaced only through `activate_connection(...)`
- `disconnect_peer()` returns both overlay rebuilds and routed resync requests
- failover keeps snapshot `stale` while replacement path exists
- failover resync may rebind provenance with identical payload/clock
- generations start at `1`

## #264: Head-of-Line Blocking

### Problem

Flooded relay currently sends sequentially, so one slow peer blocks later peers.

### Design

`prepare_relay()` becomes a pure collection step for flooded state messages only:

```rust
pub fn prepare_relay(&self, origin: &HostName, msg: &PeerDataMessage)
    -> Vec<(HostName, Arc<dyn PeerSender>, PeerWireMessage)>
```

It:

- clones sender refs while holding the lock
- stamps the local host into the vector clock
- applies only vector-clock relay filtering

Route trust does not affect flood forwarding. It only affects local acceptance.

Callers:

1. collect under lock
2. drop lock
3. `join_all` concurrent sends

Apply the same pattern to local flooded fanout.

### Files Changed

- `crates/flotilla-daemon/src/peer/manager.rs`
  - `prepare_relay()`
- `crates/flotilla-daemon/src/server.rs`
  - concurrent flood dispatch
- `crates/flotilla-daemon/Cargo.toml`
  - `futures` / `futures-util`

### Tests

- flooded relay includes / excludes the right peers by clock
- slow peer does not block later peers
- node may relay a flooded state message that it does not locally accept
