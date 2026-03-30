# Node Identity

**Status:** Design — ready for implementation planning after environment model and UI/correlation work land.
**Related:** #500 (HostName FQDN collisions), #382 (protocol version mismatch diagnostics), environment model spec (2026-03-28), service host resolution spec (2026-03-26)

## Problem

`HostName` is used as the daemon's mesh identity — peer manager maps, vector clocks, message routing, command attribution are all keyed by it. But hostnames are unreliable: routers assign fake domains, Tailscale changes them, FQDN stripping causes collisions (#500), NFS-shared home directories mean two machines see the same config.

This spec replaces `HostName` with a stable cryptographic identity for the daemon mesh. It is the second of two specs:

1. **Environment model spec** (separate document) — Reclassify host as environment, unify discovery and execution context.
2. **This spec** — Cryptographic daemon identity, replacing `HostName` in mesh internals.

By the time this spec is implemented, the environment model spec has already separated "execution context" uses of `HostName` into `EnvironmentId`. What remains is the mesh identity uses — peer maps, vector clocks, routing, message origin — which this spec rekeys to `NodeId`.

## Node

A **node** is a running flotilla daemon with a stable cryptographic identity.

- Identity is an Ed25519 keypair generated on first start.
- **`NodeId`** is the public key's SHA-256 fingerprint, **truncated to 16 bytes (32 hex chars)**. Full SHA-256 is 32 bytes / 64 hex chars; truncation is an intentional trade-off for ergonomics (shorter display, shorter serialized keys) while retaining 128 bits of collision resistance — more than sufficient for a personal device fleet. This is the mesh identity key.
- Nodes have a human-friendly **display name** from config or `gethostname()`. This is for UI and logs only, never used as a key.
- A node participates in the peer mesh, routes messages, and manages zero or more environments.

## Keypair Storage

On first start, the daemon generates an Ed25519 keypair stored at:

```
~/.config/flotilla/identity/<machine-id>/node.key    # private
~/.config/flotilla/identity/<machine-id>/node.pub    # public
```

The `<machine-id>` discriminator prevents collisions when home directories are shared over NFS. Resolution order:

1. `/etc/machine-id` (Linux — present on all systemd machines)
2. `IOPlatformUUID` via `ioreg` (macOS)
3. `machine_id` field in `daemon.toml` (explicit override)
4. If none: the daemon refuses to start with a clear error asking the user to set `machine_id` in config.

No silent generation — a generated ID stored in the shared config dir would defeat the purpose on NFS.

## NodeId Type

```rust
/// Stable cryptographic identity for a flotilla daemon.
/// SHA-256 fingerprint of the node's Ed25519 public key.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);
```

Constructed once at daemon startup from the keypair. Injected everywhere — never derived from ambient state.

## Hello Handshake

`Message::Hello` gains a `node_id` field. The existing `host_name` field becomes `display_name`:

```rust
Hello {
    protocol_version: u32,
    node_id: NodeId,
    display_name: String,
    session_id: uuid::Uuid,
}
```

The previous `environment_id: Option<EnvironmentId>` field is removed. With the multi-environment model (environment model spec), a node's environments are advertised via `HostSummary`, not the hello handshake. Hello is purely for node-level identity and protocol negotiation.

Peers validate the `node_id` against their stored fingerprint (TOFU — see below).

## Trust Model: TOFU

Trust-on-first-use, matching the SSH model. Trust is pinned to **SSH destinations** (the canonical endpoint), not to config labels or keys alone.

Two storage directories:
- `~/.local/share/flotilla/known-endpoints/<ssh-destination-hash>` — maps an SSH destination (e.g. `robert@desktop.local`) to the `NodeId` it presented on first connection. The filename is a hash of the SSH destination string to keep it filesystem-safe. This is the primary trust anchor — it survives config label renames because the SSH destination is the stable identifier.
- `~/.local/share/flotilla/known-nodes/<node-id>` — maps a `NodeId` to metadata (display name, SSH destinations, first-seen timestamp). Secondary, for cross-referencing.

**Connection flow:**

1. Connect to peer via its SSH destination (from the `hostname` field in `[peers.<label>]`).
2. Peer presents `NodeId` in hello handshake.
3. Look up `known-endpoints/<hash-of-ssh-destination>`:
   - **No entry (first use):** Accept, record the `NodeId` for this endpoint, log a notice. Also record in `known-nodes/`.
   - **Entry matches:** Accept, connection is trusted.
   - **Entry differs (endpoint changed key):** **Reject** with a clear error: "peer at 'robert@desktop.local' previously presented NodeId X but now presents Y." This is the SSH-style scary warning — the endpoint's identity changed. The user must manually clear the old entry to accept the new key.
4. If `expected_node_id` is set in config, skip TOFU — validate directly against the configured value.

This ensures that "same SSH destination, different key" is detected and rejected, matching SSH host-key behavior. Renaming a config label (e.g. `[peers.desktop]` → `[peers.workstation]`) preserves trust because the SSH destination hasn't changed. Simply keying trust by `NodeId` alone would miss endpoint-changed-identity scenarios.

## Configuration

`hosts.toml` is retired. Peer definitions fold into `daemon.toml`:

```toml
# ~/.config/flotilla/daemon.toml
display_name = "homelab"
# machine_id = "my-homelab"   # only needed if /etc/machine-id and IOPlatformUUID are unavailable

[ssh]
multiplex = true

[peers.laptop]
hostname = "robert@laptop.local"          # SSH destination
expected_node_id = "a3f8..."              # optional, TOFU if omitted
display_name = "laptop"                   # optional, falls back to peer's self-reported name
flotilla_command = "flotilla"             # optional, default "flotilla"

[peers.desktop]
hostname = "robert@desktop.local"
```

**`flotilla_command`**: Path or command used to invoke flotilla on the remote side. Defaults to `"flotilla"` (on `$PATH`). Override for development builds or non-standard installations.

## Peer Connection Bootstrap

Instead of hardcoding the remote daemon socket path, the connecting node uses an ephemeral `CommandRunner` over SSH:

1. SSH to peer: run `<flotilla_command> ensure-daemon` via the ephemeral runner.
2. Remote flotilla finds an existing daemon or starts one. Prints connection metadata to stdout (socket path, protocol version, node ID).
3. Local side validates protocol compatibility and sets up the SSH tunnel to the reported socket.
4. Proceed with hello handshake, TOFU validation.

This replaces the current `daemon_socket` config field. The remote side is responsible for finding/starting its own daemon — the caller doesn't need to know internal paths.

## Internal Rekeying

All mesh-internal data structures rekey from `HostName` to `NodeId`. By this point, execution-context uses are already `EnvironmentId` (from the environment model spec). What remains is mesh identity:

**`flotilla-protocol`:**
- `VectorClock`: `BTreeMap<HostName, u64>` → `BTreeMap<NodeId, u64>`
- `PeerDataMessage`: `origin_host` → `origin_node: NodeId`
- `RoutedPeerMessage` variants: all `requester_host`, `target_host`, `responder_host` fields → `NodeId`
- `HostSummary`: `host_name` → `node_id: NodeId` + `display_name: String`
- `HostSnapshot`: same
- `RepoSnapshot`: `host_name` → `node_id: NodeId`
- `WorkItem`: `host` → `node_id: NodeId`
- `DaemonEvent` variants: `host` fields → `node_id: NodeId`
- `Command`: `host: Option<HostName>` → `node: Option<NodeId>`

**`flotilla-daemon`:**
- `PeerManager`: all `HashMap<HostName, ...>` → `HashMap<NodeId, ...>`
- `ActiveConnection`, route tables, displaced senders — all rekeyed
- SSH transport: validates `NodeId` from hello, not `HostName`

**`flotilla-core`:**
- `HostRegistry`: `HashMap<HostName, HostState>` → `HashMap<NodeId, HostState>`
- `InProcessDaemon`: `host_name: HostName` → `node_id: NodeId` + `display_name: String`
- `peer_providers`: keyed by `NodeId`

**`flotilla-tui`:**
- `TuiModel.hosts`: keyed by `NodeId`
- UI renders display names, not node IDs
- CLI commands that accept a host argument match against display names (with disambiguation if needed)

## HostName Elimination

After both specs are implemented, `HostName` as a type is no longer needed:

- Mesh identity → `NodeId`
- Execution context → `EnvironmentId`
- Display name → `String` field on nodes and environments

`HostName::local()` is removed entirely. The daemon's `NodeId` is derived from its keypair at startup and threaded explicitly. The display name comes from config or `gethostname()` at the one entry point in `main.rs`. No ambient identity derivation anywhere in the execution path.

## What This Does Not Cover

- **Message signing.** The keypair exists for identity. Signing relayed messages for authenticity verification is a natural extension but not in scope.
- **Key rotation.** Future work. Would need a protocol for peers to accept a new key from a node they already trust.
- **Multi-user.** This design assumes a single user's fleet. Shared-tenancy nodes would need authorization beyond identity.
