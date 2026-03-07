# Socket Server Design (Step 2)

Issue: #31 | Builds on: Step 1 (DaemonHandle trait + InProcessDaemon)

## Goal

Add two-process mode: a standalone daemon serves state over a unix socket, and the TUI connects as a client. The existing `--embedded` (in-process) mode remains.

## Architecture

```
flotilla daemon                         flotilla (TUI)
┌────────────────────┐                 ┌────────────────────┐
│ flotilla-daemon    │                 │ flotilla-tui       │
│                    │                 │                    │
│ InProcessDaemon    │◄── unix ───────►│ SocketDaemon       │
│ (owns repos,       │    socket       │ (DaemonHandle      │
│  refresh, execute) │    ndjson       │  over the wire)    │
│                    │                 │                    │
│ ConnectionManager  │                 └────────────────────┘
│ (accept, route,    │
│  fan-out events,   │
│  idle timeout)     │
└────────────────────┘
```

The daemon process embeds `InProcessDaemon`. The socket server adapts it to the wire protocol. The TUI's `SocketDaemon` adapts the wire protocol back to `DaemonHandle`. Both sides are thin translation layers.

## Wire Protocol

**Transport:** Unix domain socket at `${config_dir}/flotilla.sock`. Newline-delimited JSON — each message is one JSON object followed by `\n`.

### Client → Server

```json
{"type": "request", "id": 1, "method": "list_repos", "params": {}}
{"type": "request", "id": 2, "method": "get_state", "params": {"repo": "/path"}}
{"type": "request", "id": 3, "method": "execute", "params": {"repo": "/path", "command": {...}}}
{"type": "request", "id": 4, "method": "refresh", "params": {"repo": "/path"}}
{"type": "request", "id": 5, "method": "add_repo", "params": {"path": "/path"}}
{"type": "request", "id": 6, "method": "remove_repo", "params": {"path": "/path"}}
```

### Server → Client

Responses carry the matching `id`:

```json
{"type": "response", "id": 1, "ok": true, "data": [...]}
{"type": "response", "id": 4, "ok": true}
{"type": "response", "id": 9, "ok": false, "error": "repo not tracked"}
```

Events push to all connected clients (no `id`):

```json
{"type": "event", "event": {"kind": "snapshot", ...}}
{"type": "event", "event": {"kind": "repo_added", ...}}
{"type": "event", "event": {"kind": "repo_removed", "path": "/path"}}
```

### Rust Wire Types

The existing `Message` enum in `flotilla-protocol` changes:

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum Message {
    #[serde(rename = "request")]
    Request { id: u64, method: String, params: serde_json::Value },
    #[serde(rename = "response")]
    Response { id: u64, ok: bool, data: Option<serde_json::Value>, error: Option<String> },
    #[serde(rename = "event")]
    Event { event: DaemonEvent },
}
```

The `Response` variant uses `serde_json::Value` for `data`. The client knows what type to deserialize based on the method it called.

### Typed Client Parsing

```rust
struct RawResponse { ok: bool, data: Option<serde_json::Value>, error: Option<String> }

impl RawResponse {
    fn parse<T: DeserializeOwned>(self) -> Result<T, String> { ... }
    fn parse_empty(self) -> Result<(), String> { ... }
}
```

This keeps `CommandResult` untouched in core. The daemon server maps `Result<T, String>` to the wire format; the client maps it back.

## Daemon Server (`flotilla-daemon`)

### Connection Management

The server accepts connections on the unix socket. Each connection spawns a task that reads requests and writes responses plus events. The server tracks connected client count for idle timeout.

### Request Dispatch

Parse incoming `Request` messages and route to the `InProcessDaemon`:

| Method | Handler | Response data |
|--------|---------|---------------|
| `list_repos` | `daemon.list_repos()` | `Vec<RepoInfo>` |
| `get_state` | `daemon.get_state(repo)` | `Snapshot` |
| `execute` | `daemon.execute(repo, cmd)` | `CommandResult` |
| `refresh` | `daemon.refresh(repo)` | none |
| `add_repo` | `daemon.add_repo(path)` | none |
| `remove_repo` | `daemon.remove_repo(path)` | none |

Unknown methods return an error response.

### Event Broadcasting

Subscribe to the `InProcessDaemon`'s broadcast channel. Fan out each `DaemonEvent` to all connected clients as `Event` messages. Drop clients whose connections break.

### Idle Timeout

When the client count drops to zero, start a 5-minute timer. Any new connection resets it. When the timer fires, remove the socket file and exit.

### Startup and Shutdown

Startup: load config from `config_dir`, initialize `InProcessDaemon` with persisted repos, bind socket, enter accept loop.

Shutdown: SIGTERM/SIGINT handler stops accepting, closes connections, removes socket file, exits.

## Socket Client (`SocketDaemon`)

Lives in `flotilla-tui`. Implements `DaemonHandle` by serializing requests over the socket.

### Internal Structure

```rust
pub struct SocketDaemon {
    writer: Mutex<BufWriter<OwnedWriteHalf>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    next_id: AtomicU64,
}
```

A background reader task receives all messages from the socket:
- `Response` → route to the matching oneshot sender in `pending`
- `Event` → forward into `event_tx`

### Request Flow

1. Allocate `id`, create oneshot channel, register in `pending`
2. Serialize `Request`, write + flush
3. Await the oneshot receiver
4. Parse `RawResponse` into expected type via `serde_json::from_value`

### Error Handling

If the reader detects a disconnected socket, it drops all pending senders. Callers receive errors. The TUI exits.

## TUI Startup

### Subcommands

The binary gains subcommand dispatch:

```
flotilla                  # TUI (default)
flotilla daemon           # Run daemon server
flotilla status           # One-shot: print repos and state
flotilla watch            # Stream events to stdout
```

### TUI Connection Sequence

1. `--embedded` → use `InProcessDaemon` (current behavior)
2. Try connect to `${config_dir}/flotilla.sock`
3. Success → use `SocketDaemon`
4. Failure → spawn `flotilla daemon` as detached process, retry connect with backoff (50ms, 100ms, 200ms, 400ms, 800ms), give up after ~2s
5. Use `SocketDaemon`

### CLI Flags

All subcommands accept:
- `--config-dir <path>` (default `~/.config/flotilla/`)
- `--socket <path>` (default `${config_dir}/flotilla.sock`)

TUI-specific:
- `--embedded` — skip socket, use `InProcessDaemon`
- `--repo-root <path>` — (existing) add repo paths

Daemon-specific:
- `--timeout <seconds>` — idle timeout (default 300)

### `flotilla status`

Connect to socket, call `list_repos`, print each repo with name and provider health, disconnect.

### `flotilla watch`

Connect to socket, print each event as formatted JSON to stdout, run until Ctrl-C.

## Daemon Lifecycle

The daemon loads repos from `${config_dir}/repos/*.toml` on startup. Repos persist across daemon restarts. The TUI auto-spawns a daemon if one is not running.

**Liveness detection:** Try to connect to the socket. If the connection fails, remove the stale socket file and spawn a fresh daemon.

**No reconnection:** If the daemon dies while the TUI is connected, the TUI exits with an error. The user restarts.

## Testing

**Unit tests:**
- `Message` roundtrips for the new `Response` shape
- `SocketDaemon` request/response parsing

**Integration test:**
- Spawn daemon bound to a temp socket
- Connect `SocketDaemon`
- Exercise full cycle: `list_repos`, `get_state`, `execute`, `add_repo`, `remove_repo`
- Verify events arrive after refresh

**Manual smoke tests:**
- `flotilla daemon &` then `flotilla status`
- `flotilla watch` in one terminal, TUI in another
- Kill daemon, verify TUI exits with error
- Start TUI without daemon, verify auto-spawn
- `--embedded` preserves current behavior

## Not In Scope

- Reconnection or fallback on disconnect
- Concurrent in-flight requests (sequential only)
- Delta snapshots (Step 3)
- TCP listener for multi-host (Step 4)
