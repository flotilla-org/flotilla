# Phase 2 Host Info Design

**Date:** 2026-03-14
**Status:** Approved

## Goal

Implement the transport and daemon-state portion of:

- `#270` Host system info exchange
- the narrow replication/storage subset of `#271` Remote tool inventory/health

This batch will make each daemon publish a static host summary to peers and retain the summaries it receives from remote hosts. It will not add the broader dedicated host-query APIs, CLI commands, or richer TUI host views; those move to a follow-up issue.

## Scope

### In Scope

- Keep the existing `Hello` handshake unchanged.
- Add a dedicated peer message for a host-level summary sent after connect.
- Replicate and retain one static `HostSummary` per host.
- Populate `HostSummary` from existing local discovery/runtime data.
- Include static system information:
  - `host_name`
  - home directory
  - operating system
  - architecture
  - CPU count
  - total memory, if cheaply available
  - coarse environment classification (`bare_metal`, `vm`, `container`, `unknown`)
- Include remote inventory/health data derived from existing discovery/provider state:
  - discovered binaries, sockets, auth files, env markers
  - host-level provider availability / health summary
- Retain local and remote host summaries in daemon state for later consumers.
- Clear stale remote host summaries on peer disconnect and remote daemon restart.
- Add protocol, daemon, and conversion tests for the new types and flows.

### Out of Scope

- Dynamic metrics like free memory, CPU load, or periodic refresh.
- New dedicated daemon APIs such as `list_hosts()` or `get_host_providers()`.
- New CLI commands or richer TUI pages built specifically around host summaries.
- Remote command forwarding or topology work.

## Why A Dedicated Host Summary Message

Three approaches were considered:

1. Put host info and inventory into `Message::Hello`.
2. Keep `Hello` for connection identity and add a dedicated host-summary peer message.
3. Split static facts into `Hello` and inventory/health into a dedicated message.

We are choosing option 2.

Reasons:

- `Hello` already serves a narrow identity/session role and should stay easy to reason about.
- Host system info and host inventory form a coherent replicated state object separate from repo snapshots.
- A dedicated message lets later work add refresh or selective updates without redesigning the handshake.
- The daemon already distinguishes peer connection state from replicated repo data; a host-summary object fits that model.

## Data Model

### Protocol Types

Add a host-summary module to `flotilla-protocol` with serde types for:

- `HostSummary`
- `SystemInfo`
- `HostEnvironment`
- `ToolInventory`
- `HostProviderStatus`

Proposed shape:

```rust
pub struct HostSummary {
    pub host_name: HostName,
    pub system: SystemInfo,
    pub inventory: ToolInventory,
    pub providers: Vec<HostProviderStatus>,
}

pub struct SystemInfo {
    pub home_dir: Option<PathBuf>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub cpu_count: Option<u16>,
    pub memory_total_mb: Option<u64>,
    pub environment: HostEnvironment,
}

pub enum HostEnvironment {
    BareMetal,
    Vm,
    Container,
    Unknown,
}

pub struct ToolInventory {
    pub binaries: Vec<DiscoveryFact>,
    pub sockets: Vec<DiscoveryFact>,
    pub auth: Vec<DiscoveryFact>,
    pub env_vars: Vec<DiscoveryFact>,
}

pub struct DiscoveryFact {
    pub name: String,
    pub detail: Vec<(String, String)>,
}

pub struct HostProviderStatus {
    pub category: String,
    pub name: String,
    pub healthy: bool,
}
```

Notes:

- The exact helper types can vary, but the payload should remain explicit and serde-friendly.
- Optional system fields ensure partial collection never blocks connection setup.
- `HostSummary` includes `host_name` even though the connection already knows the peer; this keeps the summary self-describing in storage and tests.

### Peer Wire Message

Extend `PeerWireMessage` with a new variant:

```rust
HostSummary(HostSummary)
```

This message is not routed hop-by-hop and is not vector-clocked. It is point-to-point state from one connected peer to another, analogous to the initial repo snapshot push but scoped to host metadata rather than a specific repo.

## Data Sources

### System Information

System information should come from a small host-info collector in `flotilla-core`, using cheap, best-effort sources only:

- home directory from `HOME`
- OS and architecture from `std::env::consts`
- CPU count from `std::thread::available_parallelism()`
- total memory from a lightweight best-effort probe if already available or straightforward to implement; otherwise leave `None`
- environment classification from simple heuristics:
  - container if common container markers are present
  - VM if a reliable low-cost signal exists
  - otherwise `Unknown` unless we can confidently classify `BareMetal`

This batch prefers correctness and fault-tolerance over exhaustive detection. Unknown is acceptable.

### Inventory and Provider Health

Reuse existing daemon discovery state:

- host discovery assertions already produced by the discovery runtime
- host-level provider registry / health information already retained by the daemon

Convert these existing internal representations into protocol-friendly summary types rather than serializing core discovery internals directly. That keeps the protocol stable even if discovery internals evolve.

For this batch, the local provider list is derived from the daemon's discovered/registered provider set at startup, with `healthy = true` for those available providers. Dynamic host-level health refresh remains out of scope with the rest of the static-only decision.

## Daemon Storage Model

### Local Summary

`InProcessDaemon` should build and retain a local `HostSummary` at startup, alongside the existing local discovery/runtime state it already owns.

This summary is static for the daemon lifetime in this batch. If local discovery changes later, a future refreshable design can replace or update it.

### Remote Summaries

`PeerManager` should store remote host summaries separately from repo peer data:

- repo snapshots remain keyed by origin host and repo identity
- host summaries are keyed only by remote host

This keeps host-level state from being shoehorned into repo-specific structures and makes disconnect/restart cleanup straightforward.

Suggested shape:

```rust
peer_host_summaries: HashMap<HostName, HostSummary>
```

Required operations:

- store/update summary for a host
- read all remote summaries
- remove summary for one host on disconnect
- clear summary for one host on remote restart

## Connection Flow

### Outbound Connect

When a peer becomes active:

1. existing `Hello` exchange completes
2. current local `HostSummary` is sent to that peer
3. existing repo snapshot push continues as today

Ordering between host summary and repo snapshots does not need strong guarantees for this batch, but the host summary should be sent during the same initial synchronization window.

### Inbound Summary

When a `PeerWireMessage::HostSummary` is received:

- store it under the connection peer's `HostName`
- replace any previous summary for that host
- do not trigger repo overlay rebuilds, because this state is host-level rather than repo-level

### Disconnect / Restart

On peer disconnect:

- remove the remote host summary for that peer
- keep the existing repo peer-data cleanup behavior

On remote daemon restart detection (`session_id` change):

- clear the stale remote host summary for that host
- keep the existing repo peer-data restart cleanup behavior

## Consumer Surface For This Batch

This batch focuses on making the data available, not redesigning presentation layers.

Minimal exposure is acceptable if there is already an existing host-status surface that can cheaply incorporate one or two summary fields. But the batch should not grow into a full host query/UI project.

The important contract is:

- local and remote host summaries are available from daemon state
- later work can build dedicated APIs and views on top of that retained state

## Error Handling

- Host summary collection must be best-effort.
- Missing fields become `None` or empty collections.
- Failure to collect one fact must not fail daemon startup or peer connect.
- Receiving malformed host-summary data should fail that message decode but should not require new connection semantics beyond the existing framing/serde failure behavior.

## Testing Strategy

### Protocol Tests

- roundtrip tests for `HostSummary` and child types
- roundtrip test for `PeerWireMessage::HostSummary`

### Conversion Tests

- convert representative discovery assertions into inventory summary entries
- classify environment markers into `HostEnvironment`
- verify unknown/missing data stays optional rather than failing

### Peer Manager Tests

- storing a host summary updates the correct peer entry
- disconnect removes stored host summaries
- restart cleanup clears stored host summaries for the restarted peer

### Daemon / Integration Tests

- on peer connect, the local host summary is sent alongside initial state sync
- inbound host summaries from a follower are retained on the leader
- clearing peer state removes stale remote host summaries

## Issue Split

After this design is written, issue scope should be aligned with the chosen batch:

- `#270` remains the host system info exchange issue
- `#271` should be narrowed to replication and daemon retention of remote tool inventory/provider health
- a new follow-up issue should cover the broader host-facing APIs and presentation work:
  - dedicated daemon host queries
  - CLI commands for host inventory/status
  - richer TUI host views

This prevents the current batch from silently growing from protocol/state work into a larger product-surface project.
