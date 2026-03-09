# Async Command Progress Design

Addresses [#23](https://github.com/rjwittams/flotilla/issues/23) (move command execution off the UI event loop) and lays groundwork for [#58](https://github.com/rjwittams/flotilla/issues/58) (intent execution as DAG with partial-failure UI).

## Problem

Commands execute inline on the TUI event loop via `await`. Slow commands (CreateCheckout, GenerateBranchName, FetchCheckoutStatus, TeleportSession) freeze the UI for 1-5 seconds. Existing spinner states (`generating: true`, `loading: true`) never animate because the UI can't repaint during execution.

The actual work already happens in the daemon (in-process or via socket). The TUI just blocks waiting for the result.

## Design

### Command IDs

Every command gets a `u64` ID assigned by the daemon when it accepts the command. This is the correlation key for lifecycle events.

### Protocol: DaemonEvent changes

Replace the existing `CommandResult` variant with two lifecycle events:

```rust
pub enum DaemonEvent {
    Snapshot(Box<Snapshot>),
    RepoAdded(Box<RepoInfo>),
    RepoRemoved { path: PathBuf },
    CommandStarted {
        command_id: u64,
        repo: PathBuf,
        description: String,
    },
    CommandFinished {
        command_id: u64,
        repo: PathBuf,
        result: CommandResult,
    },
}
```

### Protocol: DaemonHandle signature change

```rust
// Before:
async fn execute(&self, repo: &Path, command: Command) -> Result<CommandResult, String>;

// After:
async fn execute(&self, repo: &Path, command: Command) -> Result<u64, String>;
```

Returns command ID immediately. Result arrives via `CommandFinished` event.

### Command descriptions

Each `Command` variant provides a human-readable description via a method on `Command`:

```rust
impl Command {
    pub fn description(&self) -> &'static str { ... }
}
```

### InProcessDaemon

`execute` assigns an ID from an `AtomicU64` counter, broadcasts `CommandStarted`, spawns the work as a `tokio::spawn` task, and returns the ID immediately. The spawned task calls `executor::execute`, triggers a refresh, then broadcasts `CommandFinished`.

Issue commands (`SetIssueViewport`, `FetchMoreIssues`, `SearchIssues`, `ClearIssueSearch`) stay fire-and-forget as today -- they don't need command IDs or lifecycle events since they produce no `CommandResult` the TUI needs.

### SocketDaemon

Server-side: the handler responds immediately with the command ID, then spawns execution. `CommandStarted`/`CommandFinished` are broadcast as events to all subscribers.

Client-side: `execute` parses the command ID from the immediate response. Results arrive through the existing reader loop that forwards `DaemonEvent` via `event_tx.send(*event)`.

### TUI: in-flight tracking

```rust
pub struct InFlightCommand {
    pub repo: PathBuf,
    pub description: String,
}

// On App:
pub in_flight: HashMap<u64, InFlightCommand>,
```

`CommandStarted` inserts into the map. `CommandFinished` removes from the map and calls the existing `handle_result`.

### TUI: UI feedback

For this pass, the status bar shows the description text of any in-flight command for the active repo. Existing modal states (`BranchInput { generating: true }`, `DeleteConfirm { loading: true }`) are set before dispatching the command and cleared by `handle_result` when `CommandFinished` arrives -- same as today, but now the UI actually repaints between dispatch and completion.

### TUI: event loop change

The inline `await` in `main.rs` becomes non-blocking:

```rust
// Before: blocks until command completes
while let Some(cmd) = app.proto_commands.take_next() {
    app::executor::execute(cmd, &mut app).await;
}

// After: dispatch returns immediately (daemon.execute returns command ID)
while let Some(cmd) = app.proto_commands.take_next() {
    app::executor::dispatch(cmd, &mut app).await;
}
```

The `dispatch` call only awaits the daemon accepting the command (returning the ID), not the execution itself.

### Error handling

If `daemon.execute()` fails (e.g. repo not tracked), that's an immediate error before any command ID is assigned -- show as `status_message`. If the command runs and fails, it arrives as `CommandFinished` with `CommandResult::Error`.

### Broadcast model

All clients subscribed to the daemon see `CommandStarted`/`CommandFinished` events. The initiating client can show commands more prominently (spinner/modal). Other clients see activity in the status bar or event log. This supports multi-client scenarios and sets up the foundation for #58's step-level progress.

## Command audit

| Command | Latency | Needs result? | Notes |
|---|---|---|---|
| `CreateCheckout` | seconds | Yes (`CheckoutCreated`) | Longest running |
| `GenerateBranchName` | 1-5s | Yes (`BranchNameGenerated`) | Claude API call |
| `FetchCheckoutStatus` | ~1s | Yes (`CheckoutStatus`) | Multiple git/gh calls |
| `TeleportSession` | seconds | No (just `Ok`) | Checkout + workspace |
| `CreateWorkspaceForCheckout` | seconds | No | Workspace creation |
| `RemoveCheckout` | fast | No | Single git call |
| `SelectWorkspace` | instant | No | Instant |
| `OpenChangeRequest` | instant | No | Opens URL |
| `OpenIssue` | instant | No | Opens URL |
| `LinkIssuesToChangeRequest` | ~1s | No | Two gh calls |
| `ArchiveSession` | fast | No | API call |

All commands go through the async path for consistency. Fast commands will start and finish almost instantly.

## Future work

- Step-level progress for compound commands (#58)
- Resumable/retryable failed steps
- Richer progress UI (progress bars, per-step status)
