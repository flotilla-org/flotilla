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

Star with leader as hub. The local daemon is the leader; remote daemons are followers. The leader initiates SSH connections outward to all followers, forwarding their sockets locally. Each connection carries bidirectional peer data exchange via `Message::PeerData` (see Protocol section). The leader relays each follower's data to all other followers so every daemon holds the full dataset.

```
  ┌──────────┐     ┌──────────┐
  │ Follower │     │ Follower │
  │ (desktop)│     │ (cloud)  │
  └────┬─────┘     └────┬─────┘
       │                 │
       │  SSH fwd + peer protocol
       │                 │
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

Connection direction: the leader connects outward to followers (SSH tunnel + client connection to follower's daemon server). Data flows bidirectionally over each connection.

### Data Flow

1. Each daemon gathers local provider data (checkouts, branches, workspaces, and — on the leader — PRs, issues, cloud agents).
2. Daemons exchange raw `ProviderData` (pre-correlation) via snapshot on connect, then deltas with gap recovery.
3. The leader relays: when it receives data from follower A, it forwards to follower B (never reflects a peer's own data back).
4. Each daemon merges local + all peers' provider data, then runs correlation on the full set.
5. The TUI connects to its local daemon and receives the correlated, merged snapshot — it does not know about multi-host.

Cross-host correlation works naturally: a checkout on the desktop and a PR fetched by the laptop share a branch name, so the correlation engine links them.

### Repo Matching

Two repos on different hosts are the same logical repo if they share the same **repo slug** (e.g. `rjwittams/flotilla`), extracted from the root remote URL. Slug-based matching avoids false negatives when hosts use different URL formats for the same repo (SSH `git@github.com:...` vs HTTPS `https://github.com/...`). The existing `extract_repo_slug` in `discovery.rs` already does this extraction.

The daemon maintains:

```rust
repo_slug → LogicalRepo {
    host_repos: HashMap<HostName, RepoInfo>,
}
```

Each logical repo gets one tab. Repos that exist only on remote hosts still get a tab.

Matching fallbacks:
- **No usable remote**: The repo is local-only and cannot match across hosts.
- **Multiple remotes**: Use the first remote (existing `first_remote_url()` behavior).
- **Unrecognized URL format**: Fall back to exact URL comparison instead of slug extraction.

For TUI snapshot keying (which uses `PathBuf` as repo identity): if the local host has the repo, use the local path. If the repo exists only on remote hosts, use a synthetic path like `<remote>/<host>/<remote-path>`.

The synthetic path is not just cosmetic — the TUI uses `PathBuf` repo keys for tab labels, tab ordering persistence (`tab-order.json`), and `RepoUiState` identity. The format must be stable across restarts so saved tab order is preserved. The daemon also needs to handle these virtual repos in `InProcessDaemon`/`RepoState` without assuming they correspond to local filesystem paths.

### Host-Namespaced Identity

The collision problem goes deeper than correlation keys. `ProviderData` uses `PathBuf` as the key for `checkouts` and `managed_terminals`. `WorkItemIdentity::Checkout(PathBuf)` is how the TUI identifies rows for selection and scrolling. If two hosts report the same path, these structures break.

**Solution**: When the daemon merges remote `ProviderData` into its local state, all path-based keys from remote hosts are prefixed with the host name: `desktop:/Users/robert/dev/flotilla`. This namespacing applies to:

- `ProviderData.checkouts` keys (PathBuf)
- `ProviderData.managed_terminals` keys (PathBuf)
- `CorrelationKey::CheckoutPath` values
- `WorkItemIdentity::Checkout` values

This ensures no collisions in any data structure. The prefixed paths are opaque identifiers — the TUI does not parse them.

Branch-based correlation (`CorrelationKey::Branch`) is intentionally *not* namespaced — a branch name on host A should correlate with the same branch name and its associated PR from host B. This is the primary mechanism for cross-host correlation.

Note: the `is_singleton_kind` rule in the correlation engine prevents two `Checkout` items from merging into the same group. This means the same branch checked out on two hosts produces two separate rows — which is correct, as they represent separate work environments.

### HostName

`HostName` is the user-chosen alias from `hosts.toml` (e.g. `desktop`, `cloud`). The local host's name defaults to the machine hostname but can be overridden in `daemon.toml`. This alias appears in the Source column and config view.

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

When `follower = true`, the daemon skips registration of external-polling providers (GitHub PRs/issues, cloud agent services) during provider discovery. Only local providers are initialized: VCS (git), CheckoutManager (worktrees), WorkspaceManager (shpool). The `follower` flag is threaded through provider discovery and registry initialization.

The follower still receives the full dataset from the leader via relay, so it can serve a local TUI with the complete picture. Service-level data (PRs, issues) arrives via `PeerData` messages, not local polling.

## SSH Transport

### Connection Lifecycle

The `PeerManager` in `flotilla-daemon` manages connections to all configured remote hosts:

1. Spawns an SSH child process: `ssh -N -L <local-sock>:<remote-sock> <user>@<hostname>`
2. Local socket path: `~/.config/flotilla/peers/<host-name>.sock`
3. Connects to the forwarded socket using `flotilla-client::SocketDaemon`
4. Receives snapshot, then subscribes to deltas

On startup, stale forwarding sockets in `~/.config/flotilla/peers/` are removed (same pattern as the daemon's own socket cleanup). The SSH child process is spawned with `kill_on_drop` so it is cleaned up if the daemon exits.

On failure: reconnects with exponential backoff (capped at 60 seconds). Connection status (connected / disconnected / reconnecting) is tracked per host.

The remote daemon must already be running. If the socket is not available, the connection enters the reconnect loop. Spawning remote daemons via SSH is out of scope for Phase 1.

### PeerTransport Trait

```rust
#[async_trait]
trait PeerTransport {
    async fn connect(&mut self) -> Result<(), String>;
    async fn disconnect(&mut self) -> Result<(), String>;
    fn is_connected(&self) -> bool;

    /// Subscribe to peer data updates (provider snapshots + deltas)
    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerDataMessage>, String>;
    /// Send peer data to the remote daemon (for relay)
    async fn send(&mut self, msg: PeerDataMessage) -> Result<(), String>;
}
```

This is narrower than `DaemonHandle` — scoped to peer data exchange only. The `PeerManager` uses these methods; it does not send commands or receive correlated snapshots over peer connections.

The SSH implementation is the first implementor. The trait exists so future transports (direct TCP, WireGuard, etc.) can slot in without changing the `PeerManager`.

## Daemon-to-Daemon Protocol

### Wire Format

The existing protocol is asymmetric: clients send `Message::Request`, servers push `Message::Event`. For peer communication, we add a new variant:

```rust
// Schematic — actual field names follow the existing Message enum shape.
// The key change is adding one new variant to the existing enum.
enum Message {
    Request { .. },
    Response { .. },
    Event(DaemonEvent),
    PeerData(PeerDataMessage),  // NEW
}
```

`PeerData` messages flow in both directions over the same connection. The leader connects to each follower as a client (via the forwarded socket), and both sides can send `PeerData` messages. The follower's daemon server recognizes `PeerData` as a peer exchange rather than a TUI request.

### Payload

The daemon-to-daemon payload is raw `ProviderData` (pre-correlation), not correlated `WorkItem` snapshots:

- **TUI-to-daemon**: correlated `WorkItem` snapshots (post-correlation)
- **Daemon-to-daemon**: raw `ProviderData` snapshots (pre-correlation)

This distinction matters because correlation must run on the merged dataset from all hosts. If daemons exchanged post-correlation data, cross-host links (checkout on host A ↔ PR on host B) would be lost.

### PeerDataMessage

```rust
struct PeerDataMessage {
    origin_host: HostName,         // who generated this data
    repo_slug: String,             // logical repo identity
    repo_path: PathBuf,            // filesystem path on origin host
    kind: PeerDataKind,
}
```

```rust
enum PeerDataKind {
    Snapshot { data: ProviderData, seq: u64 },
    Delta { changes: ProviderDataDelta, seq: u64, prev_seq: u64 },
    RequestResync { since_seq: u64 },
}
```

When a peer detects a sequence gap, it sends `RequestResync` with the last known sequence. The responder replies with a full `Snapshot` for that (origin_host, repo_slug) pair — the same strategy the TUI-daemon gap recovery uses. Delta streams resume from the new snapshot's sequence.
```

Each message carries an `origin_host` tag so the receiver knows the data source and the relay logic can avoid reflecting data back to its origin. Sequence numbers are per-(origin_host, repo_slug).

### Authentication

The daemon server distinguishes peer clients from TUI clients. In Phase 1, any client that sends a `PeerData` message is treated as a peer — no explicit handshake. A peer authentication protocol is deferred to future work.

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

Each daemon maintains peer state keyed by both host and repo:

```rust
peer_data: HashMap<HostName, HashMap<RepoSlug, PerRepoPeerState>>

struct PerRepoPeerState {
    provider_data: ProviderData,
    repo_path: PathBuf,  // filesystem path on the origin host
    seq: u64,
}
```

A single host may report multiple repos — keying only by `HostName` would collapse them. When any entry changes, the daemon re-merges and re-correlates.

### Merging Strategy

For a given logical repo, the daemon merges `ProviderData` from all hosts:

- **Host-scoped data** (checkouts, managed terminals): combined from all hosts with path namespacing. No conflicts possible since keys are host-prefixed.
- **Service-level data** (PRs, issues, cloud agents): the leader is the single source of truth. Followers do not poll these, so there are no duplicates to reconcile. The leader's local `ProviderData` contains the canonical service-level data; peer data from followers contains only host-scoped items.

## TUI Changes

Minimal — the TUI does not know about multi-host. It receives a unified snapshot from the daemon.

### Source Column

Already renders provider attribution. For host-scoped items (checkouts), the Source includes the host name — e.g. `desktop:git`. Service-level items (PRs, issues, cloud agents) are not host-scoped and display as before.

Workspaces are not standalone rows — they appear as `workspace_refs` attached to correlated items. A correlated row's Source reflects its anchor item (typically a checkout). The host distinction for workspaces shows up in the workspace ref details (preview pane), not the Source column.

### Config View

The Flotilla tab's config screen gains a "Hosts" section showing:
- Each configured remote host
- Connection status (connected / disconnected / reconnecting)
- Last successful sync time

This sits alongside the existing provider health display.

### Host Provenance on Work Items

Each `WorkItem` (and `ProtoWorkItem`) carries an explicit `host: Option<HostName>` field. `None` means local; `Some(name)` means the item originates from a remote host. This field is set during snapshot merging and propagates through correlation and grouping.

The action menu and executor use `host` to filter actions:
- **Local items** (`host: None`): all actions available as today.
- **Remote items** (`host: Some(_)`): actions requiring local filesystem access (open terminal, delete worktree, create checkout) are hidden. Actions that work without a local clone (open PR in browser, copy branch name) remain available.
- **Remote-only repos**: For repos that exist only on remote hosts, `gh`-based browser actions are hidden since there is no local clone to provide context. Future work will either proxy commands to a remote checkout or call the GitHub API directly.

### No Other Changes

No new tab types. No new modes. No new key bindings. The tab system, navigation, selection, and correlation all work as-is because the daemon presents a unified model.

## Crate Impact

| Crate | Changes |
|-------|---------|
| `flotilla-daemon` | `PeerManager`, `PeerTransport` trait, SSH implementation, relay logic, follower mode flag, snapshot merging |
| `flotilla-protocol` | `Message::PeerData` variant, `PeerDataMessage`, `PeerDataKind`, `HostName` type |
| `flotilla-core` | Config parsing for `hosts.toml`, host-namespaced correlation keys, `host` field on work items |
| `flotilla-client` | None (reused as-is for peer connections) |
| `flotilla-tui` | Host in Source column, Hosts section in config view, action filtering by host provenance |
| `flotilla` (root) | None |

## Future Work

- **Phase 2**: Remote terminal opening, remote checkout creation (file follow-up issue)
- **Per-provider leader election**: Split-brain resilience, capability-restricted election
- **Auto-discovery**: mDNS or similar for LAN hosts
- **Alternate transports**: Direct TCP, WireGuard tunnels
- **Config compatibility**: Version negotiation between daemons
