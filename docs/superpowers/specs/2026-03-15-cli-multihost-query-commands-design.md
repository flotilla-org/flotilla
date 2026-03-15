# CLI Multi-Host Query Commands Design

**Date**: 2026-03-15
**Issue**: #284
**Status**: Approved

This document narrows `#284` to the first shippable slice of multi-host CLI queries. It supersedes the `#284` portion of `docs/superpowers/specs/2026-03-13-cli-multihost-testing-design.md`.

## Scope

Add four one-shot CLI query commands, all supporting human output and `--json`:

| Command | Description |
|---------|-------------|
| `flotilla host list` | Show the local host plus known peer hosts with connection and summary data |
| `flotilla host <host> status` | Detailed single-host status from local/replicated state |
| `flotilla host <host> providers` | Host-level discovery/inventory/provider summary for a local or remote host |
| `flotilla topology` | Show the daemon's current multi-host routing view in human and JSON output |

## Out Of Scope

- `flotilla topology --dot` — follow-up issue `#355`
- `watch` scoping and filtering — follow-up issue `#356`
- Remote command forwarding changes
- New peer wire messages beyond the already-landed host summary replication

## Command Semantics

### `flotilla host list`

Returns a row per known host:

- the local host
- configured peers from `hosts.toml`
- any additional remote host learned at runtime from current peer state

Each row includes:

- host name
- whether it is the local host
- whether it is config-backed
- current connection status
- whether a host summary is available
- repo count contributed by that host
- work item count contributed by that host

If a configured host is disconnected and has no retained summary, it still appears with `summary = null` / `has_summary = false`.

### `flotilla host <host> status`

Returns:

- host identity and flags (`is_local`, `configured`)
- current connection status
- optional `HostSummary`
- `repo_count`
- `work_item_count`

This command intentionally does not duplicate the full inventory/provider tables from `host <host> providers`.

### `flotilla host <host> providers`

Returns:

- host identity and flags
- current connection status
- optional `HostSummary`

For the local host, the summary comes from `InProcessDaemon::local_host_summary()`. For remote hosts, it comes from replicated peer summaries. If no summary is available for the requested host, return a user-facing error instead of an empty success payload.

### `flotilla topology`

The initial topology command exposes the daemon's current routing view rather than attempting a globally authoritative graph.

Each entry represents one known remote host and includes:

- target host
- current next hop
- whether the route is direct
- fallback next hops, if any
- whether the target is currently connected

This is enough for a useful human/JSON command now and gives `#355` a stable data source for later DOT rendering.

## Protocol Additions

Add host/topology query types to `flotilla-protocol/src/query.rs`:

```rust
pub struct HostListResponse {
    pub hosts: Vec<HostListEntry>,
}

pub struct HostListEntry {
    pub host: HostName,
    pub is_local: bool,
    pub configured: bool,
    pub connection_status: PeerConnectionState,
    pub has_summary: bool,
    pub repo_count: usize,
    pub work_item_count: usize,
}

pub struct HostStatusResponse {
    pub host: HostName,
    pub is_local: bool,
    pub configured: bool,
    pub connection_status: PeerConnectionState,
    pub summary: Option<HostSummary>,
    pub repo_count: usize,
    pub work_item_count: usize,
}

pub struct HostProvidersResponse {
    pub host: HostName,
    pub is_local: bool,
    pub configured: bool,
    pub connection_status: PeerConnectionState,
    pub summary: HostSummary,
}

pub struct TopologyResponse {
    pub local_host: HostName,
    pub routes: Vec<TopologyRoute>,
}

pub struct TopologyRoute {
    pub target: HostName,
    pub next_hop: HostName,
    pub direct: bool,
    pub connected: bool,
    pub fallbacks: Vec<HostName>,
}
```

`HostSummary` and `PeerConnectionState` are reused directly rather than re-expanded into another protocol shape.

## Daemon API

Extend `DaemonHandle` with:

```rust
async fn list_hosts(&self) -> Result<HostListResponse, String>;
async fn get_host_status(&self, host: &str) -> Result<HostStatusResponse, String>;
async fn get_host_providers(&self, host: &str) -> Result<HostProvidersResponse, String>;
async fn get_topology(&self) -> Result<TopologyResponse, String>;
```

`host` is the raw CLI query string. Resolution remains daemon-side so the CLI stays thin.

## Core Data Model

`InProcessDaemon` already retains:

- local host summary
- peer connection status
- peer provider overlays

To answer the new queries, it also needs read-only snapshots of:

- remote `HostSummary` values
- topology / route view
- config-backed peer names

The daemon server and embedded peer wiring already own the authoritative `PeerManager`. They should mirror the query-relevant host/topology state into `InProcessDaemon` whenever peer connections or summaries change. This keeps `DaemonHandle` implementations symmetric and avoids coupling CLI query code directly to daemon-server internals.

## Topology Source Of Truth

The source of truth is `PeerManager`:

- direct connection state
- route table primary next hop
- fallback route hops

Expose a read-only topology snapshot from `PeerManager`, then convert and mirror it into `InProcessDaemon`.

## CLI Grammar

Keep `host` control commands working while adding host queries.

The simplest approach is:

- change `SubCommand::Host` to accept raw `args: Vec<String>`
- parse `host list`
- parse `host <host> status`
- parse `host <host> providers`
- fall back to existing host-scoped control command parsing for `refresh`, `repo ...`, and `checkout ... remove`

Add a new top-level `Topology` subcommand for `flotilla topology`.

## Output Formatting

Follow the existing `status` / repo query command pattern:

- `crates/flotilla-tui/src/cli.rs` owns human-readable formatting helpers
- JSON output uses `flotilla_protocol::output::json_pretty`

Human output expectations:

- `host list`: condensed table
- `host status`: short header plus summary counts and system info
- `host providers`: inventory and provider tables derived from `HostSummary`
- `topology`: routing table with `Target`, `Via`, `Direct`, `Connected`, `Fallbacks`

## Testing Strategy

### Protocol

- serde round-trip coverage for the new response types

### Core

- `InProcessDaemon` tests for local host resolution, configured disconnected peers, remote summary availability, and host/work counts derived from peer overlays

### Server / Socket

- request dispatch tests for the new RPC methods
- socket round-trip coverage for list/status/providers/topology

### CLI

- parser tests for `host list`, `host <host> status`, `host <host> providers`, and legacy host control commands
- formatter tests for human output

## Non-Goals And Follow-Ups

- `#355` adds DOT rendering once the base topology data shape is implemented and validated.
- `#356` investigates current `watch` behavior before any scoped watch syntax is added.
