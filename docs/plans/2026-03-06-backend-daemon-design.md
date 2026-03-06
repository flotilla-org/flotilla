# Backend Daemon Architecture Design

Issue: #31 | Unlocks: #36 (web), #33 (multi-host), #35 (agent CLI)

## Motivation

Architectural cleanliness first. The TUI is growing and the boundary between domain state and view state is blurring. Extracting a daemon enforces the right split at compile time. Multi-host coordination (#33) is the next priority, followed by web dashboard (#36) and agent CLI (#35).

## Daemon Lifecycle

Hybrid model:
- TUI auto-spawns a daemon if one isn't already running
- Daemon persists after TUI disconnects (idle timeout or explicit `flotilla daemon stop`)
- `--embedded` flag runs everything in-process (no socket, uses `InProcessDaemon` directly)
- Standalone `flotilla daemon` invocation comes later for multi-host

## Transport

Unix domain socket at `~/.config/flotilla/flotilla.sock`. Newline-delimited JSON. Single connection carries both request/response and pushed events, distinguished by envelope type.

## Protocol

### Envelope

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum Message {
    #[serde(rename = "request")]
    Request { id: u64, method: String, params: serde_json::Value },

    #[serde(rename = "response")]
    Response { id: u64, result: ResponseResult },

    #[serde(rename = "event")]
    Event { event: EventKind },
}
```

Requests carry an `id`, responses echo it. Events have no `id` and are pushed to all subscribers. A `flotilla watch` command is just "connect, subscribe, print events."

### Methods (request/response)

| Method | Params | Response | Notes |
|--------|--------|----------|-------|
| `subscribe` | `{}` | `ok` | Start receiving events on this connection |
| `list_repos` | `{}` | `[{path, name, provider_health}]` | Current repo list |
| `get_state` | `{repo: path}` | Full snapshot for one repo | Initial hydration |
| `refresh` | `{repo: path}` | `ok` | Trigger immediate refresh |
| `execute` | `{repo: path, command: Command}` | Command result | Maps to executor |
| `add_repo` | `{path}` | `ok` | |
| `remove_repo` | `{path}` | `ok` | |

### Events (pushed to subscribers)

| Event | Data | Notes |
|-------|------|-------|
| `snapshot` | `{repo, seq, snapshot}` | New refresh data |
| `repo_added` | `{path, name}` | |
| `repo_removed` | `{path}` | |
| `command_result` | `{repo, command, result}` | Async command completion |
| `error` | `{repo, message}` | Provider errors |

### Snapshot Versioning

Each snapshot carries a monotonic `seq` per repo. Initial hydration sends a full snapshot. Delta support (diff against `prev_seq`) is a fast-follow — the `seq` field is present from day one so clients are sequencing-aware. On reconnect or missed seq, client requests full re-sync via `get_state`.

```rust
enum SnapshotEvent {
    Full { seq: u64, snapshot: Snapshot },
    Delta { seq: u64, prev_seq: u64, changes: Vec<Change> },
}

enum Change {
    WorkItemAdded(WorkItem),
    WorkItemUpdated(WorkItem),
    WorkItemRemoved(WorkItemIdentity),
    ProviderHealthChanged { provider: String, healthy: bool },
}
```

## DaemonHandle Trait

The core abstraction enabling in-process and remote operation:

```rust
#[async_trait]
pub trait DaemonHandle: Send + Sync {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent>;
    async fn get_state(&self, repo: &Path) -> Result<Snapshot>;
    async fn list_repos(&self) -> Result<Vec<RepoInfo>>;
    async fn execute(&self, repo: &Path, command: Command) -> Result<CommandResult>;
    async fn refresh(&self, repo: &Path) -> Result<()>;
    async fn add_repo(&self, path: &Path) -> Result<()>;
    async fn remove_repo(&self, path: &Path) -> Result<()>;
}
```

Two implementations:
- **`InProcessDaemon`** (in `flotilla-core`) — wraps current AppModel + refresh + executor directly
- **`SocketDaemon`** (in `flotilla-tui`) — connects to unix socket, JSON serialization

TUI startup: check for live socket -> `SocketDaemon`; no socket -> spawn daemon, then `SocketDaemon`; `--embedded` -> `InProcessDaemon`.

## Crate Structure

```
flotilla/                          # Cargo workspace
  crates/
    flotilla-core/                 # Providers, correlation, refresh, executor, config
                                   #   DaemonHandle trait + InProcessDaemon
    flotilla-protocol/             # Serde-only types: envelopes, snapshots, commands
    flotilla-daemon/               # Unix socket server, bridges protocol <-> core
    flotilla-tui/                  # UI rendering, input, SocketDaemon impl
  src/main.rs                     # Aggregator binary: subcommand dispatch
```

Dependencies: `flotilla-protocol` is the leaf (no internal deps). `flotilla-core` depends on protocol. Daemon and TUI depend on core + protocol. Aggregator binary depends on all four.

Subcommands: `flotilla` (TUI), `flotilla daemon` (server), `flotilla watch` (subscribe + print), `flotilla status` (one-shot dump).

## File Migration

### To flotilla-core

| Current | Destination | Notes |
|---------|-------------|-------|
| `src/providers/**` | `providers/` | Unchanged |
| `src/provider_data.rs` | `provider_data.rs` | Unchanged |
| `src/data.rs` | `data.rs` | Correlation + WorkItem |
| `src/refresh.rs` | `refresh.rs` | Background refresh |
| `src/config.rs` | `config.rs` | Repo persistence |
| `src/app/executor.rs` | `executor.rs` | Command execution |
| `src/app/command.rs` | `command.rs` | Command enum |
| `src/app/model.rs` | `model.rs` | `RepoModel`, `AppModel` (daemon state), `RepoLabels` |
| — | `daemon.rs` | New: `DaemonHandle` trait + `InProcessDaemon` |

### To flotilla-tui

| Current | Destination | Notes |
|---------|-------------|-------|
| `src/ui.rs` | `ui.rs` | Rendering |
| `src/app/mod.rs` | `app.rs` | Input handling |
| `src/app/ui_state.rs` | `ui_state.rs` | View state |
| `src/app/intent.rs` | `intent.rs` | UI concept |
| `src/event.rs` | `event.rs` | Terminal events |
| `src/event_log.rs` | `event_log.rs` | Tracing display |
| `src/template.rs` | `template.rs` | Workspace templates |
| — | `socket.rs` | New: `SocketDaemon` (Step 2) |

### New in flotilla-protocol

| File | Contents |
|------|----------|
| `lib.rs` | `Message` envelope, request/response/event types |
| `snapshot.rs` | Serializable `Snapshot`, `WorkItem`, `RepoInfo` |
| `commands.rs` | Serializable `Command`, `CommandResult` |

## Migration Strategy (Strangler Fig)

### Step 1: Define the boundary (in-process only)

- Create workspace structure (4 crates + aggregator)
- Move files per mapping above
- Define `DaemonHandle` trait
- Implement `InProcessDaemon`
- Define protocol types (even though nothing serializes yet)
- TUI talks to `InProcessDaemon` through the trait
- **Constraint:** nothing crosses the trait boundary that couldn't be serialized
- **Result:** same single-process behavior, seam enforced at compile time

### Step 2: Socket server

- Implement unix socket listener in `flotilla-daemon`
- Implement `SocketDaemon` client in `flotilla-tui`
- Add `flotilla daemon` subcommand
- Add daemon auto-spawn logic + `--embedded` flag
- Implement `flotilla watch` and `flotilla status`
- **Result:** two-process mode works

### Step 3: Delta snapshots

- Add `seq` tracking to daemon snapshot publishing
- Implement diffing between consecutive snapshots
- Clients track seq, request full re-sync on gap
- **Result:** efficient updates

### Step 4: Multi-host prep (future, #33)

- TCP listener alongside unix socket
- Daemon discovery / registration
- Out of scope for this design

## Key Design Decisions

- **Clients receive WorkItems, not raw provider data.** Daemon does correlation. Clients stay thin.
- **Intents stay client-side.** They're a UI concept (what actions are available). The client sends Commands, the daemon executes them.
- **In-process mode never goes away.** The `InProcessDaemon` impl is zero-cost and gives an `--embedded` option.
- **Full snapshots first, deltas later.** Ship with full snapshots; `seq` field present from day one for forward compatibility.
