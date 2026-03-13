# Multi-Host Batch 1 Resilience Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the Batch 1 multi-host resilience design: peer `Hello` handshake, unified peer wire messaging, generation-guarded connection ownership, routed resync control traffic, and concurrent flooded relay fanout.

**Architecture:** Extend the protocol crate with a single peer wire envelope, then refactor daemon peer infrastructure so every connection activates through one manager path and every inbound peer message is generation-tagged before reaching state management. Keep flooded replication and targeted routed control separate in the manager/server split so routing, cleanup, and failover stay local to `PeerManager` while `server.rs` owns socket/session wiring and daemon overlay rebuilds.

**Tech Stack:** Rust, Tokio, serde, Unix sockets, async_trait, existing `flotilla-protocol` / `flotilla-daemon` integration tests.

---

## Chunk 1: Protocol And Config Surface

### Task 1: Add Batch 1 wire protocol types

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-protocol/src/peer.rs`
- Test: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Write failing protocol roundtrip tests**

Add tests for:
- `Message::Hello { protocol_version, host_name }`
- `Message::Peer(Box::new(PeerWireMessage::Data(...)))`
- `Message::Peer(Box::new(PeerWireMessage::Routed(RoutedPeerMessage::RequestResync { ... })))`
- `Message::Peer(Box::new(PeerWireMessage::Routed(RoutedPeerMessage::ResyncSnapshot { ... })))`

Use the existing `test_helpers::assert_roundtrip` / `assert_json_roundtrip`.

- [ ] **Step 2: Run the focused protocol tests to verify failure**

Run: `cargo test -p flotilla-protocol message_request_roundtrip -- --nocapture`

Expected: compile failure or missing-variant errors for `Hello`, `PeerWireMessage`, or `RoutedPeerMessage`.

- [ ] **Step 3: Implement the protocol types**

Add to `crates/flotilla-protocol/src/lib.rs`:

```rust
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConfigLabel(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    // existing variants...
    #[serde(rename = "hello")]
    Hello { protocol_version: u32, host_name: HostName },
    #[serde(rename = "peer")]
    Peer(Box<PeerWireMessage>),
}
```

Add peer-envelope types in `peer.rs` or `lib.rs` and re-export them from `lib.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "peer_type")]
pub enum PeerWireMessage {
    Data(PeerDataMessage),
    Routed(RoutedPeerMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "routed_type")]
pub enum RoutedPeerMessage {
    RequestResync { request_id: u64, requester_host: HostName, target_host: HostName, remaining_hops: u8, repo_identity: RepoIdentity, since_seq: u64 },
    ResyncSnapshot { request_id: u64, requester_host: HostName, responder_host: HostName, remaining_hops: u8, repo_identity: RepoIdentity, repo_path: PathBuf, clock: VectorClock, seq: u64, data: Box<ProviderData> },
}
```

Keep the existing `PeerDataMessage` wire shape from `crates/flotilla-protocol/src/peer.rs`
intact inside `PeerWireMessage::Data`.

- [ ] **Step 4: Run the focused protocol tests to verify they pass**

Run: `cargo test -p flotilla-protocol --locked`

Expected: protocol crate tests pass, including the new roundtrip coverage.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/lib.rs crates/flotilla-protocol/src/peer.rs
git commit -m "feat: add batch1 peer wire protocol"
```

### Task 2: Extend host config with expected canonical identity

**Files:**
- Modify: `crates/flotilla-core/src/config.rs`
- Test: `crates/flotilla-core/src/config.rs`
- Test: `crates/flotilla-daemon/tests/socket_roundtrip.rs`

- [ ] **Step 1: Write failing config parse tests**

Add a focused unit test in `crates/flotilla-core/src/config.rs` that parses:

```toml
[hosts.desktop]
hostname = "desktop.local"
expected_host_name = "desktop"
daemon_socket = "/tmp/flotilla.sock"
```

and asserts the new field is present.

- [ ] **Step 2: Run the config test to verify failure**

Run: `cargo test -p flotilla-core --locked hosts_config`

Expected: parse failure or missing-field compile errors.

- [ ] **Step 3: Implement the config change**

Extend `RemoteHostConfig` with:

```rust
pub struct RemoteHostConfig {
    pub hostname: String,
    pub expected_host_name: String,
    pub user: Option<String>,
    pub daemon_socket: String,
}
```

Update any existing test fixtures or constructors that instantiate `RemoteHostConfig`.

- [ ] **Step 4: Run the config test to verify it passes**

Run: `cargo test -p flotilla-core --locked hosts_config`

Expected: the new config test passes.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/config.rs
git commit -m "feat: add expected peer identity to host config"
```

## Chunk 2: Transport And Activation Refactor

### Task 3: Replace transport send with sender abstraction

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/transport.rs`
- Modify: `crates/flotilla-daemon/src/peer/mod.rs`
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Test: `crates/flotilla-daemon/src/peer/manager.rs`

- [ ] **Step 1: Write failing manager tests against `PeerSender`**

Add or rewrite manager tests so they can register a sender without a transport, e.g.:
- inbound-only sender receives a relay
- direct `send_to()` reaches a registered sender

Use a `MockPeerSender` that records `PeerWireMessage`.

- [ ] **Step 2: Run the focused manager tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked relay_sends_to_all_except_origin -- --nocapture`

Expected: compile errors because `PeerSender` / sender registration do not exist yet.

- [ ] **Step 3: Implement the abstraction split**

In `transport.rs`:

```rust
#[async_trait]
pub trait PeerSender: Send + Sync {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String>;
}

#[async_trait]
pub trait PeerTransport: Send + Sync {
    async fn connect(&mut self) -> Result<(), String>;
    async fn disconnect(&mut self) -> Result<(), String>;
    fn status(&self) -> PeerConnectionStatus;
    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String>;
}
```

Re-export `PeerSender` from `peer/mod.rs`. Remove `send()` from `PeerTransport`.

- [ ] **Step 4: Refactor test doubles to the new abstraction**

Update `MockTransport` in both:
- `crates/flotilla-daemon/src/peer/manager.rs`
- `crates/flotilla-daemon/tests/multi_host.rs`

so transport tests only exercise lifecycle/subscription, while send-path tests use `MockPeerSender`.

- [ ] **Step 5: Run focused manager tests to verify pass**

Run: `cargo test -p flotilla-daemon --locked peer::manager -- --nocapture`

Expected: manager unit tests compile against `PeerWireMessage` and pass.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-daemon/src/peer/transport.rs crates/flotilla-daemon/src/peer/mod.rs crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/tests/multi_host.rs
git commit -m "refactor: split peer transport from peer sender"
```

### Task 4: Refactor SSH transport to handshake and emit peer wire messages

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/ssh_transport.rs`
- Test: `crates/flotilla-daemon/src/peer/ssh_transport.rs`
- Test: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Write failing SSH transport tests**

Add targeted tests for:
- local socket path still uses configured label host key as today or updated constructor input, whichever remains intended
- handshake requires `Message::Hello`
- handshake rejects wrong `protocol_version`
- handshake rejects unexpected `host_name`
- pairwise initiator enforcement quiesces the lexicographically larger side before reconnect attempts

Prefer focused helper tests around parsing/validation if full socket handshake tests are too heavy.

- [ ] **Step 2: Run the SSH transport tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked peer::ssh_transport -- --nocapture`

Expected: missing handshake helpers or mismatched message-type compile failures.

- [ ] **Step 3: Update `SshTransport` data model**

Change `SshTransport` to store:
- `config_label: ConfigLabel`
- `expected_host_name: HostName`
- outbound `mpsc::Sender<PeerWireMessage>`
- inbound `mpsc::Receiver<PeerWireMessage>`

Constructor should use `RemoteHostConfig.expected_host_name` instead of inferring peer identity from the config key.

- [ ] **Step 4: Implement pre-split Hello handshake**

In `connect_socket()`:
- connect raw `UnixStream`
- write local `Message::Hello`
- read exactly one `Message::Hello`
- validate `protocol_version == PROTOCOL_VERSION`
- validate `host_name == expected_host_name`
- call `activate_connection(...)`
- only then split the stream and spawn reader/writer tasks with the captured generation

Reader task should accept only `Message::Peer`, decode to `PeerWireMessage`, and forward it.
Writer task should wrap outbound `PeerWireMessage` as `Message::Peer`.

- [ ] **Step 5: Add a sender accessor**

Expose a way for the reconnect/activation wiring to obtain an `Arc<dyn PeerSender>` backed by the outbound channel, e.g. `ChannelPeerSender`.

- [ ] **Step 6: Run the SSH transport tests to verify pass**

Run: `cargo test -p flotilla-daemon --locked peer::ssh_transport -- --nocapture`

Expected: transport tests pass with the new handshake path.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-daemon/src/peer/ssh_transport.rs
git commit -m "feat: add hello handshake to ssh peer transport"
```

## Chunk 3: PeerManager Routing, Authority, And Cleanup

### Task 5: Introduce connection activation, generations, and route state

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/peer/mod.rs`
- Test: `crates/flotilla-daemon/src/peer/manager.rs`
- Test: `crates/flotilla-daemon/tests/multi_host.rs`

- [ ] **Step 1: Write failing manager tests for activation and stale-message dropping**

Add tests for:
- `activate_connection()` supersedes an older sender for the same host
- stale-generation inbound envelope is dropped
- `send_to()` uses direct sender first, then routed primary
- no-route `send_to()` returns an error
- late `ResyncSnapshot` is dropped if its `pending_resync_requests` entry no longer exists
- routed control forwarding drops on exhausted hop budget

- [ ] **Step 2: Run the focused manager tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked activate_connection -- --nocapture`

Expected: missing generation/routing APIs.

- [ ] **Step 3: Replace `PeerManager` core state**

Refactor `PeerManager` fields from the Phase 1 shape:

```rust
peers: HashMap<HostName, Box<dyn PeerTransport>>
```

to Batch 1 state:

```rust
transports: HashMap<ConfigLabel, Box<dyn PeerTransport>>,
senders: HashMap<HostName, Arc<dyn PeerSender>>,
transport_peers: HashMap<ConfigLabel, HostName>,
generations: HashMap<HostName, u64>,
routes: HashMap<HostName, RouteState>,
reverse_paths: HashMap<ReversePathKey, ReversePathHop>,
pending_resync_requests: HashMap<ReversePathKey, PendingResyncRequest>,
route_epoch: u64,
request_id_counter: u64,
```

Add the new support types from the spec:
- `ConnectionDirection`
- `ConnectionMeta`
- `InboundPeerEnvelope`
- `RouteHop`
- `RouteState`
- `ReversePathKey`
- `ReversePathHop`
- `PendingResyncRequest`
- `DisconnectPlan`

- [ ] **Step 4: Implement `activate_connection()`**

Behavior:
- apply one-owner-per-canonical-host arbitration
- remove any superseded sender
- bump generation starting at `1`
- update `transport_peers` when a configured outbound transport resolves to a canonical host
- return the captured generation for the connection reader task

- [ ] **Step 5: Implement generation-aware inbound handling**

Replace `handle_peer_data(PeerDataMessage)` with either:
- `handle_inbound(InboundPeerEnvelope) -> HandleResult`, or
- a thin gating layer plus updated internal handlers

Requirements:
- stale generation drops immediately
- `PeerWireMessage::Data` and `PeerWireMessage::Routed` both go through gating
- accepted-state clocks/provenance update only on authority acceptance
- relay-dedup state is kept separate from accepted-state authority tracking

- [ ] **Step 6: Implement route and reverse-path state**

Manager behavior should now cover:
- direct route preference
- route creation on first authority-accepted discovery
- candidate learning from clock-accepted observations or gap-detecting deltas
- host-scoped route ordering by `learned_epoch` only; do not use repo-level `seq` to rank
  host routes
- reverse-path recording for forwarded `RequestResync`
- reverse-path forwarding for `ResyncSnapshot`
- request timeout bookkeeping in `pending_resync_requests`
- requester-side late-reply rejection when the pending request no longer exists
- routed-control loop prevention via `remaining_hops`

- [ ] **Step 7: Implement `disconnect_peer()` returning `DisconnectPlan`**

Behavior:
- no-op on stale generation
- remove sender ownership for the exact `(host, generation)`
- remove or stale-mark active repo state depending on replacement reachability
- promote direct route, then fallback, then validated candidate
- allocate new `request_id` values for failover-triggered routed resyncs
- return both `affected_repos` and concrete `resync_requests`

- [ ] **Step 8: Run manager tests to verify pass**

Run: `cargo test -p flotilla-daemon --locked peer::manager -- --nocapture`

Expected: manager unit tests pass with route/generation coverage.

- [ ] **Step 9: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/peer/mod.rs crates/flotilla-daemon/tests/multi_host.rs
git commit -m "feat: add generation-guarded peer routing state"
```

### Task 6: Update merge-facing peer state and remote-only cleanup paths

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/peer/merge.rs`
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`

- [ ] **Step 1: Write failing tests for stale retention and provenance rebinding**

Add tests for:
- failover keeps a repo snapshot with `stale = true` while replacement path exists
- failover resync can clear `stale` and rebind provenance even with identical payload/clock
- unrelated candidate paths are not promoted solely because a routed resync succeeded through them

- [ ] **Step 2: Run the focused tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked failover -- --nocapture`

Expected: missing `stale`/provenance fields or incorrect cleanup behavior.

- [ ] **Step 3: Extend `PerRepoPeerState` and any merge consumers**

Add:

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

Make sure merge/display code ignores provenance unless it needs it, and that `stale` does not accidentally hide still-reachable remote-only repos before resync completes.

- [ ] **Step 4: Run the focused tests to verify pass**

Run: `cargo test -p flotilla-daemon --locked multi_host -- --nocapture`

Expected: multi-host integration tests pass with the enriched stored state.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/peer/merge.rs crates/flotilla-daemon/tests/multi_host.rs
git commit -m "feat: track peer provenance and stale failover state"
```

## Chunk 4: Server Wiring, Session Roles, And Concurrent Relay

### Task 7: Rework `server.rs` around session classification and peer envelopes

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/src/lib.rs`
- Test: `crates/flotilla-daemon/src/server.rs`
- Test: `crates/flotilla-daemon/tests/socket_roundtrip.rs`

- [ ] **Step 1: Write failing server tests**

Add targeted tests for:
- invalid first message closes socket
- no daemon `Event` is written before initial `Hello`/`Request` classification
- inbound peer handshake registers via `activate_connection(...)`
- duplicate inbound peer connection supersedes the old sender
- outbound reader tasks use the generation captured before task spawn

- [ ] **Step 2: Run the focused server tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked server -- --nocapture`

Expected: old `PeerClientMap` behavior or eager event task startup still present.

- [ ] **Step 3: Remove `PeerClientMap` and eager peer registration**

Replace the current `peer_clients` / `next_peer_conn_id` shared map path with:
- `SocketPeerSender`
- `Message::Peer` decode path
- `activate_connection(...)` for inbound peers
- generation-captured `InboundPeerEnvelope` forwarding

- [ ] **Step 4: Implement shared-socket role switch**

In `handle_client(...)`:
- read first message before spawning event forwarding
- branch:
  - `Message::Request` => start normal client event writer and request loop
  - `Message::Hello` => validate version, respond with local `Hello`, activate peer, enter peer loop
  - anything else => log and close

Do not start any event writer until the branch is resolved.

- [ ] **Step 5: Wire routed control handling**

Update inbound processing so:
- `RequestResync` and `ResyncSnapshot` flow through the manager’s routed-message path
- `ResyncRequested` / `NeedsResync` callers emit `RoutedPeerMessage::RequestResync` rather than `PeerDataKind::RequestResync`
- local resync replies use `ResyncSnapshot` and reverse-path forwarding rather than plain `send_to(from, PeerDataMessage)`
- reverse-path replies use direct sender lookup to the recorded next hop rather than `send_to()`

- [ ] **Step 6: Make disconnect cleanup consume `DisconnectPlan`**

When a connection drops:
- call `disconnect_peer(host, generation)`
- rebuild overlays for `affected_repos`
- dispatch every `DisconnectPlan.resync_requests` entry via `send_to(...)`

- [ ] **Step 7: Run server-focused tests**

Run: `cargo test -p flotilla-daemon --locked server -- --nocapture`

Expected: server unit tests pass with the new session and cleanup flow.

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/lib.rs crates/flotilla-daemon/tests/socket_roundtrip.rs
git commit -m "feat: add peer hello session routing to daemon server"
```

### Task 8: Make flooded relay concurrent and sender-based

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/Cargo.toml`
- Test: `crates/flotilla-daemon/src/peer/manager.rs`
- Test: `crates/flotilla-daemon/tests/multi_host.rs`

- [ ] **Step 1: Write failing relay tests**

Add or update tests for:
- `prepare_relay()` includes/excludes peers by vector clock
- relay can target registered inbound-only senders
- a slow sender does not block later peers
- rejected-but-forwarded flooded state still relays

- [ ] **Step 2: Run the focused relay tests to verify failure**

Run: `cargo test -p flotilla-daemon --locked relay -- --nocapture`

Expected: old sequential `relay()` behavior or wrong message type.

- [ ] **Step 3: Implement `prepare_relay()`**

Move `relay()` to a pure collection helper:

```rust
pub fn prepare_relay(
    &self,
    origin: &HostName,
    msg: &PeerDataMessage,
) -> Vec<(HostName, Arc<dyn PeerSender>, PeerWireMessage)>
```

The helper should:
- clone senders under lock
- stamp the local host into the relayed vector clock
- filter only by flood-forwarding rules, not route authority

- [ ] **Step 4: Dispatch relay sends concurrently**

In `server.rs`, gather relay work under the manager lock, drop the lock, then `join_all` the send futures.

Add `futures` / `futures-util` to `crates/flotilla-daemon/Cargo.toml` if needed.

- [ ] **Step 5: Run relay-focused tests**

Run: `cargo test -p flotilla-daemon --locked relay -- --nocapture`

Expected: relay tests pass and no sequential-send assumptions remain.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/Cargo.toml crates/flotilla-daemon/tests/multi_host.rs
git commit -m "perf: fan out flooded peer relay concurrently"
```

## Chunk 5: End-To-End Verification And Cleanup

### Task 9: Stabilize end-to-end multi-host coverage

**Files:**
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
- Modify: `crates/flotilla-daemon/tests/socket_roundtrip.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/src/peer/ssh_transport.rs`

- [ ] **Step 1: Add focused integration coverage for Batch 1 behaviors**

Cover:
- handshake mismatch rejection
- routed resync request/response on a relayed path
- stale-generation disconnect no-op
- direct-route preference over relay fallback
- candidate route promotion only after validation
- late `ResyncSnapshot` is dropped after requester timeout/cancel
- routed-control hop budget prevents request/response loops

- [ ] **Step 2: Run daemon integration tests**

Run: `cargo test -p flotilla-daemon --locked multi_host socket_roundtrip -- --nocapture`

Expected: all new end-to-end scenarios pass.

- [ ] **Step 3: Run full daemon crate tests**

Run: `cargo test -p flotilla-daemon --locked`

Expected: daemon crate passes cleanly.

- [ ] **Step 4: Run workspace verification**

Normal environment:

```bash
cargo test --workspace --locked
```

Sandbox-safe fallback:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: workspace passes with either the normal or sandbox-safe command appropriate to the environment.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/tests/multi_host.rs crates/flotilla-daemon/tests/socket_roundtrip.rs crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/peer/ssh_transport.rs
git commit -m "test: cover batch1 multi-host resilience flows"
```

### Task 10: Update docs and implementation notes

**Files:**
- Modify: `docs/superpowers/specs/2026-03-12-multi-host-batch1-resilience-design.md`
- Modify: `CLAUDE.md` or relevant contributor docs only if workflow/test commands need updates

- [ ] **Step 1: Reconcile the spec with any implementation-driven deltas**

Update only if the shipped implementation makes a constrained deviation from the current spec wording.

- [ ] **Step 2: Record any intentional follow-ups**

If implementation leaves obvious follow-up work out of scope, add a short “Follow-ups” note to the spec or file issues rather than expanding Batch 1.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-03-12-multi-host-batch1-resilience-design.md CLAUDE.md
git commit -m "docs: align batch1 resilience docs with implementation"
```
