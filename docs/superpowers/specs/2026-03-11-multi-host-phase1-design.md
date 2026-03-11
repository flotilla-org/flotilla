# Multi-Host Phase 1: Read-Only Visibility

**Issue:** [#33 — Multi-host coordination](https://github.com/rjwittams/flotilla/issues/33)
**Date:** 2026-03-11
**Status:** Draft

## Goal

See work items across multiple development hosts from a single flotilla instance. A developer working across laptop, desktop, and cloud VMs sees all checkouts, branches, and workspaces in one unified view — each repo appears as a single tab regardless of how many hosts have it checked out.

## Scope

**In scope:**
- Configure remote hosts in flotilla config
- SSH-forward remote daemon unix sockets
- Daemon-to-daemon replication of raw provider data
- Follower mode: remote daemons report only local state
- Repo matching by root remote URL into unified tabs
- Host attribution in the Source column for checkouts and workspaces
- Connection status in the config view

**Out of scope (future phases):**
- Opening terminals on remote hosts
- Creating checkouts on remote hosts
- Session handoff between hosts
- Per-provider leader election
- Auto-discovery of hosts
- Auth beyond SSH keys
- Config compatibility checks between daemons

## Architecture

### Topology

Star with leader as hub. The local daemon is the leader; remote daemons are followers. Followers connect only to the leader. The leader relays each follower's data to all other followers so every daemon holds the full dataset.

```
  ┌──────────┐     ┌──────────┐
  │ Follower │     │ Follower │
  │ (desktop)│     │ (cloud)  │
  └────┬─────┘     └────┬─────┘
       │    SSH fwd      │
       └──────┬──────────┘
              │
        ┌─────┴──────┐
        │   Leader   │
        │  (laptop)  │
        └────────────┘
              │
         ┌────┴────┐
         │   TUI   │
         └─────────┘
```

### Data Flow

1. Each daemon gathers local provider data (checkouts, branches, workspaces, and — on the leader — PRs, issues, cloud agents).
2. Daemons exchange raw `ProviderData` (pre-correlation) via snapshot on connect, then deltas with gap recovery.
3. The leader relays: when it receives data from follower A, it forwards to follower B (never reflects a peer's own data back).
4. Each daemon merges local + all peers' provider data, then runs correlation on the full set.
5. The TUI connects to its local daemon and receives the correlated, merged snapshot — it does not know about multi-host.

Cross-host correlation works naturally: a checkout on the desktop and a PR fetched by the laptop share a branch name, so the correlation engine links them.

### Repo Matching

Two repos on different hosts are the same logical repo if they share the same root remote URL (e.g. `git@github.com:rjwittams/flotilla.git`). The daemon maintains:

```rust
root_remote_url → LogicalRepo {
    host_repos: HashMap<HostName, RepoInfo>,
}
```

Each logical repo gets one tab. Repos that exist only on remote hosts still get a tab.

## Configuration

### Remote Hosts

File: `~/.config/flotilla/hosts.toml`

```toml
[hosts.desktop]
hostname = "desktop.local"
user = "robert"
daemon_socket = "/run/user/1000/flotilla/daemon.sock"

[hosts.cloud]
hostname = "10.0.1.50"
daemon_socket = "/home/robert/.config/flotilla/daemon.sock"
```

Fields:
- `hostname` — SSH destination (hostname or IP)
- `user` — SSH user (optional, defaults to current user)
- `daemon_socket` — path to the daemon's unix socket on the remote host

### Follower Mode

File: `~/.config/flotilla/daemon.toml` on the remote host

```toml
follower = true
```

When `follower = true`, the daemon disables all external polling (GitHub PRs/issues, cloud agent services). It reports only local state: git worktrees, branches, and terminal sessions.

The follower still receives the full dataset from the leader via relay, so it can serve a local TUI with the complete picture.

## SSH Transport

### Connection Lifecycle

The `PeerManager` in `flotilla-daemon` manages connections to all configured remote hosts:

1. Spawns an SSH child process: `ssh -N -L <local-sock>:<remote-sock> <user>@<hostname>`
2. Local socket path: `~/.config/flotilla/peers/<host-name>.sock`
3. Connects to the forwarded socket using `flotilla-client::SocketDaemon`
4. Receives snapshot, then subscribes to deltas

On failure: reconnects with exponential backoff. Connection status (connected / disconnected / reconnecting) is tracked per host.

### PeerTransport Trait

```rust
#[async_trait]
trait PeerTransport {
    async fn connect(&mut self) -> Result<(), String>;
    async fn disconnect(&mut self) -> Result<(), String>;
    fn is_connected(&self) -> bool;
    fn daemon_handle(&self) -> &dyn DaemonHandle;
}
```

The SSH implementation is the first implementor. The trait exists so future transports (direct TCP, WireGuard, etc.) can slot in without changing the `PeerManager`.

## Daemon-to-Daemon Protocol

The daemon-to-daemon protocol uses the same `Message` envelope and transport as TUI-to-daemon, but carries different payload:

- **TUI-to-daemon**: correlated `WorkItem` snapshots (post-correlation)
- **Daemon-to-daemon**: raw `ProviderData` snapshots (pre-correlation)

This distinction matters because correlation must run on the merged dataset from all hosts. If daemons exchanged post-correlation data, cross-host links (checkout on host A ↔ PR on host B) would be lost.

The protocol reuses:
- Snapshot on connect for initial sync
- Delta messages for ongoing updates
- Sequence numbers and gap recovery

Each message is tagged with its origin host so the receiver can maintain `HashMap<HostName, ProviderData>` and re-correlate when any host's data changes.

## Relay Logic

The leader forwards peer data to other peers:

```
Leader receives ProviderData from "desktop"
  → forwards to "cloud" (tagged origin: "desktop")
  → does NOT reflect back to "desktop"

Leader receives ProviderData from "cloud"
  → forwards to "desktop" (tagged origin: "cloud")
  → does NOT reflect back to "cloud"
```

The leader also sends its own local data to all followers.

Each daemon maintains:
```rust
peer_data: HashMap<HostName, ProviderData>
```

When any entry changes, the daemon re-merges and re-correlates.

## TUI Changes

Minimal — the TUI does not know about multi-host. It receives a unified snapshot from the daemon.

### Source Column

Already renders provider attribution. For host-scoped items (checkouts, workspaces), the Source now includes the host name — e.g. `desktop:git` or `cloud:shpool`. Service-level items (PRs, issues, cloud agents) are not host-scoped and display as before.

### Config View

The Flotilla tab's config screen gains a "Hosts" section showing:
- Each configured remote host
- Connection status (connected / disconnected / reconnecting)
- Last successful sync time

This sits alongside the existing provider health display.

### No Other Changes

No new tab types. No new modes. No new key bindings. The tab system, navigation, selection, and correlation all work as-is because the daemon presents a unified model.

## Crate Impact

| Crate | Changes |
|-------|---------|
| `flotilla-daemon` | `PeerManager`, `PeerTransport` trait, SSH implementation, relay logic, follower mode flag, snapshot merging |
| `flotilla-protocol` | Host-tagged provider data messages, peer data envelope |
| `flotilla-core` | Config parsing for `hosts.toml`, possible minor correlation adjustments for host tagging |
| `flotilla-client` | None (reused as-is for peer connections) |
| `flotilla-tui` | Host in Source column, Hosts section in config view |
| `flotilla` (root) | None |

## Future Work

- **Phase 2**: Remote terminal opening, remote checkout creation (file follow-up issue)
- **Per-provider leader election**: Split-brain resilience, capability-restricted election
- **Auto-discovery**: mDNS or similar for LAN hosts
- **Alternate transports**: Direct TCP, WireGuard tunnels
- **Config compatibility**: Version negotiation between daemons
