# Peer Connection Ownership V2

## Context

Batch 1 resilience introduced a deterministic pairwise initiator rule:

- for peer pair `(A, B)`, only the lexicographically smaller canonical `HostName`
  initiates the transport connection
- the larger host accepts inbound for that pair

That rule solves duplicate-connection churn, but it makes connection ownership
depend on host naming rather than operator intent or real network topology.
This is awkward for deployments where:

- only one side has the right SSH reachability or credentials
- discovery systems may cause either side to initiate
- the "right" initiator is defined by config, not by hostname ordering

This follow-up design replaces hostname-ordered initiation with config-driven
ownership and runtime duplicate arbitration.

## Goals

- Let connection initiation follow config or discovery intent.
- Converge cleanly if both sides initiate the same peer connection.
- Keep a single active connection owner per canonical peer `HostName`.
- Avoid endless reconnect churn after intentional duplicate retirement.
- Preserve the existing `Hello`-established canonical identity model.

## Non-Goals

- No cryptographic peer identity beyond the current `Hello.host_name` model.
- No general routing redesign.
- No attempt to support multiple simultaneously active equivalent connections to
  the same canonical peer.

## Design

### Initiation Model

Outbound initiation is config-driven, not hostname-driven.

- If this node has outbound config or discovery intent for peer `P`, it may try
  to connect.
- Inbound `Hello` sessions are still always accepted long enough to identify the
  peer and run arbitration.
- Remove the pairwise "lexicographically smaller host initiates" rule.

This means two peers may both attempt outbound to each other. That is allowed.
It is resolved after identity is known rather than prevented up front.

### Connection Metadata

`activate_connection(...)` remains the single authority for connection
ownership. It must have enough metadata to compare a newly established
connection against any current active connection for the same canonical peer.

The decision inputs are:

- canonical peer `HostName`
- connection direction: inbound or outbound
- whether this specific connection candidate is backed by explicit local
  outbound config
- optional config label for status/logging

The config-backed flag is candidate-specific, not just peer-specific:

- an outbound connection created from local config is config-backed
- an unsolicited inbound connection from that same peer is not automatically
  config-backed on receipt
- discovery-only or opportunistic candidates are not config-backed unless a
  local outbound transport actually owns them

This preserves the distinction between "I intended to own this connection" and
"a connection from that peer exists."

Config backing affects long-term ownership and reconnect policy. It does not
choose the winning live socket.

### Arbitration Rule

When a new connection for peer `P` reaches `activate_connection(...)`, compare
it with the existing active connection for `P` if one exists.

Winner rule:

1. If two simultaneous physical connections exist for peer pair `(local, P)`,
   pick a single winning physical connection for the pair using a deterministic
   stable host-identity rule.
2. Connection direction is only a local heuristic inside that winning
   physical-connection rule. It must not be applied independently on both sides
   in a way that can cause both legs to be dropped.

The critical requirement is that both peers derive complementary keep/drop
behavior for the same pair of simultaneous sockets. The arbitration rule must
choose one physical connection, not merely prefer "outbound" as an abstract
class.

In other words:

- the pairwise physical-connection tie-break decides which currently-live
  socket survives
- `config_backed` decides who should own reconnect responsibility over time
- the pairwise physical-connection tie-break decides which currently-live
  socket survives when both sides connected at once

### Duplicate Retirement

If arbitration rejects the newly established connection or supersedes the old
one, the loser is intentionally retired.

Add a direct connection-level control frame:

```rust
pub enum PeerWireMessage {
    Data(PeerDataMessage),
    Routed(RoutedPeerMessage),
    Goodbye {
        reason: GoodbyeReason,
    },
}

pub enum GoodbyeReason {
    Superseded,
}
```

`Goodbye` is used only for intentional retirement between peers that have
already completed a compatible `Hello` exchange. It is not required for
ordinary network disconnects.

Behavior:

- before closing the losing connection, send `Goodbye { reason: Superseded }`
- then close the socket / sender normally

Handshake failures such as protocol mismatch or unexpected peer identity remain
close-only or best-effort logging cases. They do not rely on `Goodbye`.

### Reconnect Suppression

Without `Goodbye`, a losing outbound owner only sees "connection closed" and
will reconnect immediately. That is correct but noisy.

On receiving `Goodbye { reason: Superseded }`:

- suppress outbound reconnect for that canonical peer for a bounded cooldown
- keep the transport configured, but do not redial during the cooldown
- if the winning connection disappears later, normal reconnect can resume

This is a runtime hygiene mechanism, not a separate long-lived ownership state
machine.

Initial design:

- one cooldown per canonical peer
- fixed bounded duration
- ordinary disconnects without `Goodbye` still use the normal backoff loop

### Status Semantics

Peer status should reflect whether there is an active winning connection for a
canonical peer, not whether some loser connection is currently churning.

Implications:

- a peer is `Connected` if any active winning connection exists
- retiring a duplicate loser must not emit a visible `Disconnected` transition
  if another winning connection for that same peer remains active
- configured peers should still appear in the overview even when disconnected

This keeps the UI stable under duplicate arbitration.

### Hello Flow

The handshake remains identity-first:

1. establish socket/transport
2. exchange `Hello`
3. validate protocol version and expected peer identity
4. call `activate_connection(...)`
5. if accepted, install sender/reader ownership
6. if rejected or superseded, send `Goodbye` if appropriate and close

Arbitration therefore happens after canonical identity is known, which is the
minimum information needed to resolve duplicates correctly.

For simultaneous dual-outbound connects, `activate_connection(...)` must compare
the two concrete physical connection candidates and select one winner for the
pair. Both sides must derive the same surviving connection from the same stable
inputs.

## Implementation Notes

The main implementation changes relative to Batch 1 are:

- remove hostname-ordered gating from:
  - `should_initiate_peer(...)`
  - `outbound_peer_names()`
  - `reconnect_peer(...)`
- replace it with config/discovery-driven outbound eligibility
- extend active connection state to remember enough metadata for arbitration
- add `Goodbye` to the peer wire model
- add reconnect suppression keyed by canonical peer identity
- make peer status updates depend on winning-connection ownership, not raw
  socket closes

## Testing

Add focused tests for:

- configured outbound beats unsolicited inbound for the same peer
- two discovery/unconfigured connections converge via direction + tie-break
- simultaneous dual-outbound connects converge to one winner
- loser receives `Goodbye { Superseded }` and suppresses reconnect temporarily
- winner disconnect later re-enables reconnect
- duplicate loser teardown does not flip overview status to `Disconnected` while
  the winner remains active

## Open Questions

- Should reconnect suppression be a fixed cooldown or end early when the winner
  is observed to disappear?
- Should `Goodbye` be a dedicated top-level peer wire variant rather than a
  routed peer control variant?
- If discovery and explicit config both exist for the same peer, should the
  transport layer coalesce them before arbitration?
