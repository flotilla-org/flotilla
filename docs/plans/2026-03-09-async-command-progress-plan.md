# Async Command Progress Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Move command execution off the UI event loop so the TUI stays responsive while slow commands run in the daemon.

**Architecture:** Commands fire-and-forget to the daemon which assigns an ID, broadcasts `CommandStarted`/`CommandFinished` events, and spawns the work. The TUI tracks in-flight commands and handles results when they arrive as daemon events.

**Tech Stack:** Rust, tokio (spawn, AtomicU64), async-trait, serde, ratatui

---

### Task 1: Protocol — Replace `DaemonEvent::CommandResult` with lifecycle events

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs:125-141` (DaemonEvent enum)
- Modify: `crates/flotilla-protocol/src/lib.rs:143-342` (tests)

**Step 1: Write failing test for new DaemonEvent variants**

Add to the existing `mod tests` block in `crates/flotilla-protocol/src/lib.rs`:

```rust
#[test]
fn daemon_event_command_started_roundtrip() {
    let event = DaemonEvent::CommandStarted {
        command_id: 42,
        repo: PathBuf::from("/tmp/repo"),
        description: "Creating checkout...".to_string(),
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        DaemonEvent::CommandStarted {
            command_id,
            repo,
            description,
        } => {
            assert_eq!(command_id, 42);
            assert_eq!(repo, PathBuf::from("/tmp/repo"));
            assert_eq!(description, "Creating checkout...");
        }
        other => panic!("expected CommandStarted, got {:?}", other),
    }
}

#[test]
fn daemon_event_command_finished_roundtrip() {
    let event = DaemonEvent::CommandFinished {
        command_id: 42,
        repo: PathBuf::from("/tmp/repo"),
        result: CommandResult::CheckoutCreated {
            branch: "feat-x".into(),
        },
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        DaemonEvent::CommandFinished {
            command_id,
            repo,
            result,
        } => {
            assert_eq!(command_id, 42);
            assert_eq!(repo, PathBuf::from("/tmp/repo"));
            match result {
                CommandResult::CheckoutCreated { branch } => assert_eq!(branch, "feat-x"),
                other => panic!("expected CheckoutCreated, got {:?}", other),
            }
        }
        other => panic!("expected CommandFinished, got {:?}", other),
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-protocol -- daemon_event_command`
Expected: FAIL — `CommandStarted` and `CommandFinished` variants don't exist.

**Step 3: Replace the `CommandResult` variant in DaemonEvent**

In `crates/flotilla-protocol/src/lib.rs`, replace the existing `CommandResult` variant (lines 134-140) with:

```rust
#[serde(rename = "command_started")]
CommandStarted {
    command_id: u64,
    repo: std::path::PathBuf,
    description: String,
},
#[serde(rename = "command_finished")]
CommandFinished {
    command_id: u64,
    repo: std::path::PathBuf,
    result: commands::CommandResult,
},
```

**Step 4: Fix compilation errors from removed variant**

The old `DaemonEvent::CommandResult` is referenced in:
- `crates/flotilla-tui/src/app/mod.rs:159-162` — `handle_daemon_event` match arm. Replace with two new arms (placeholder — just log for now, we'll implement properly in Task 5):

```rust
DaemonEvent::CommandStarted { command_id, repo, description } => {
    tracing::debug!("command {command_id} started on {}: {description}", repo.display());
}
DaemonEvent::CommandFinished { command_id, repo, result } => {
    tracing::debug!("command {command_id} finished on {}", repo.display());
    executor::handle_result(result, self);
}
```

**Step 5: Run tests**

Run: `cargo test -p flotilla-protocol -- daemon_event_command`
Expected: PASS

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: All pass, no warnings.

**Step 6: Commit**

```
feat: replace DaemonEvent::CommandResult with lifecycle events (#23)

CommandStarted and CommandFinished replace the single CommandResult
variant, carrying a command_id for correlation.
```

---

### Task 2: Protocol — Add `Command::description()` method

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs:8-71` (Command enum)

**Step 1: Write failing test**

Add to `mod tests` in `crates/flotilla-protocol/src/commands.rs`:

```rust
#[test]
fn command_description_covers_all_variants() {
    // Ensure every variant has a non-empty description.
    let cases: Vec<Command> = vec![
        Command::CreateWorkspaceForCheckout {
            checkout_path: PathBuf::from("/tmp"),
        },
        Command::SelectWorkspace {
            ws_ref: "x".into(),
        },
        Command::CreateCheckout {
            branch: "b".into(),
            create_branch: true,
            issue_ids: vec![],
        },
        Command::RemoveCheckout {
            branch: "b".into(),
        },
        Command::FetchCheckoutStatus {
            branch: "b".into(),
            checkout_path: None,
            change_request_id: None,
        },
        Command::OpenChangeRequest { id: "1".into() },
        Command::OpenIssue { id: "1".into() },
        Command::LinkIssuesToChangeRequest {
            change_request_id: "1".into(),
            issue_ids: vec![],
        },
        Command::ArchiveSession {
            session_id: "s".into(),
        },
        Command::GenerateBranchName {
            issue_keys: vec![],
        },
        Command::TeleportSession {
            session_id: "s".into(),
            branch: None,
            checkout_key: None,
        },
        Command::AddRepo {
            path: PathBuf::from("/tmp"),
        },
        Command::RemoveRepo {
            path: PathBuf::from("/tmp"),
        },
        Command::Refresh,
        Command::SetIssueViewport {
            repo: PathBuf::from("/tmp"),
            visible_count: 10,
        },
        Command::FetchMoreIssues {
            repo: PathBuf::from("/tmp"),
            desired_count: 10,
        },
        Command::SearchIssues {
            repo: PathBuf::from("/tmp"),
            query: "q".into(),
        },
        Command::ClearIssueSearch {
            repo: PathBuf::from("/tmp"),
        },
    ];
    for cmd in cases {
        let desc = cmd.description();
        assert!(!desc.is_empty(), "empty description for {:?}", cmd);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-protocol -- command_description`
Expected: FAIL — method doesn't exist.

**Step 3: Implement `Command::description()`**

Add to `crates/flotilla-protocol/src/commands.rs`, after the enum definition:

```rust
impl Command {
    /// Human-readable description for progress display.
    pub fn description(&self) -> &'static str {
        match self {
            Command::CreateWorkspaceForCheckout { .. } => "Creating workspace...",
            Command::SelectWorkspace { .. } => "Switching workspace...",
            Command::CreateCheckout { .. } => "Creating checkout...",
            Command::RemoveCheckout { .. } => "Removing checkout...",
            Command::FetchCheckoutStatus { .. } => "Fetching checkout status...",
            Command::OpenChangeRequest { .. } => "Opening in browser...",
            Command::OpenIssue { .. } => "Opening in browser...",
            Command::LinkIssuesToChangeRequest { .. } => "Linking issues...",
            Command::ArchiveSession { .. } => "Archiving session...",
            Command::GenerateBranchName { .. } => "Generating branch name...",
            Command::TeleportSession { .. } => "Teleporting session...",
            Command::AddRepo { .. } => "Adding repository...",
            Command::RemoveRepo { .. } => "Removing repository...",
            Command::Refresh => "Refreshing...",
            Command::SetIssueViewport { .. } => "Loading issues...",
            Command::FetchMoreIssues { .. } => "Fetching issues...",
            Command::SearchIssues { .. } => "Searching issues...",
            Command::ClearIssueSearch { .. } => "Clearing search...",
        }
    }
}
```

**Step 4: Run tests**

Run: `cargo test -p flotilla-protocol -- command_description`
Expected: PASS

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: All pass.

**Step 5: Commit**

```
feat: add Command::description() for progress display (#23)
```

---

### Task 3: Change `DaemonHandle::execute` signature to return `u64`

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs:22` (trait method)
- Modify: `crates/flotilla-core/src/in_process.rs:506` (InProcessDaemon impl)
- Modify: `crates/flotilla-tui/src/socket.rs:185-193` (SocketDaemon impl)
- Modify: `crates/flotilla-tui/src/app/executor.rs:58-61` (caller)

**Step 1: Change the trait signature**

In `crates/flotilla-core/src/daemon.rs:22`, change:

```rust
/// Execute a command. Returns a command ID; the result arrives via
/// CommandStarted/CommandFinished events.
async fn execute(&self, repo: &Path, command: Command) -> Result<u64, String>;
```

Update the import on line 6 — remove `CommandResult` (it's no longer returned directly):

```rust
use flotilla_protocol::{Command, DaemonEvent, RepoInfo, Snapshot};
```

**Step 2: Update InProcessDaemon to spawn and return ID**

In `crates/flotilla-core/src/in_process.rs`:

Add `AtomicU64` and `Ordering` to imports (line 8 area):

```rust
use std::sync::atomic::{AtomicU64, Ordering};
```

Add field to `InProcessDaemon` struct (after line 104):

```rust
next_command_id: AtomicU64,
```

Initialize in `new()` (in the `Arc::new(Self { ... })` block around line 142):

```rust
next_command_id: AtomicU64::new(1),
```

Replace the `execute` method body (lines 506-563). The issue command handling at the top stays the same (spawn and return a command ID). The main path becomes:

```rust
async fn execute(&self, repo: &Path, command: Command) -> Result<u64, String> {
    let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);

    // Issue commands stay fire-and-forget — no lifecycle events needed
    match &command {
        Command::SetIssueViewport { visible_count, .. } => {
            let visible_count = *visible_count;
            let repo = repo.to_path_buf();
            let self_ref = self.clone_weak();
            tokio::spawn(async move {
                if let Some(d) = self_ref.upgrade() {
                    d.ensure_issues_cached(&repo, visible_count * 2).await;
                    d.broadcast_snapshot(&repo).await;
                }
            });
            return Ok(id);
        }
        Command::FetchMoreIssues { desired_count, .. } => {
            let desired_count = *desired_count;
            let repo = repo.to_path_buf();
            let self_ref = self.clone_weak();
            tokio::spawn(async move {
                if let Some(d) = self_ref.upgrade() {
                    d.ensure_issues_cached(&repo, desired_count).await;
                    d.broadcast_snapshot(&repo).await;
                }
            });
            return Ok(id);
        }
        Command::SearchIssues { query, .. } => {
            let query = query.clone();
            let repo = repo.to_path_buf();
            let self_ref = self.clone_weak();
            tokio::spawn(async move {
                if let Some(d) = self_ref.upgrade() {
                    d.search_issues(&repo, &query).await;
                    d.broadcast_snapshot(&repo).await;
                }
            });
            return Ok(id);
        }
        Command::ClearIssueSearch { .. } => {
            let repo = repo.to_path_buf();
            let self_ref = self.clone_weak();
            tokio::spawn(async move {
                if let Some(d) = self_ref.upgrade() {
                    {
                        let mut repos = d.repos.write().await;
                        if let Some(state) = repos.get_mut(&repo) {
                            state.search_results = None;
                        }
                    }
                    d.broadcast_snapshot(&repo).await;
                }
            });
            return Ok(id);
        }
        _ => {} // fall through to spawned execution
    }

    // Broadcast started, spawn execution, broadcast finished
    let description = command.description().to_string();
    let repo_path = repo.to_path_buf();
    let _ = self.event_tx.send(DaemonEvent::CommandStarted {
        command_id: id,
        repo: repo_path.clone(),
        description,
    });

    let runner = Arc::clone(&self.runner);
    let event_tx = self.event_tx.clone();
    let (registry, providers_data) = {
        let repos = self.repos.read().await;
        let state = repos
            .get(repo)
            .ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
        (
            Arc::clone(&state.model.registry),
            Arc::clone(&state.model.data.providers),
        )
    };

    // Need a way to trigger refresh after completion
    let repos_ref = self.repos.clone();

    tokio::spawn(async move {
        let result =
            executor::execute(command, &repo_path, &registry, &providers_data, &*runner).await;

        // Trigger a refresh
        {
            let repos = repos_ref.read().await;
            if let Some(state) = repos.get(&repo_path) {
                state.model.refresh_handle.trigger_refresh();
            }
        }

        let _ = event_tx.send(DaemonEvent::CommandFinished {
            command_id: id,
            repo: repo_path,
            result,
        });
    });

    Ok(id)
}
```

Note: `InProcessDaemon` currently doesn't have a `clone_weak` or similar helper for issue commands. The issue commands currently call `self` methods directly. Since we're inside `#[async_trait] impl DaemonHandle for InProcessDaemon` where `self: &Self` and `Self: Arc`-wrapped, we need to handle this. The simplest approach: for issue commands, keep the existing inline `await` approach (they're already spawned from the TUI side in `executor.rs:44-53`). Actually, looking again, the issue commands are called here directly too. We need to restructure.

**Alternative for issue commands:** Since issue commands are already spawned from the TUI executor (`crates/flotilla-tui/src/app/executor.rs:44-53`), and `InProcessDaemon::execute` is called from that spawned task, they can stay as inline awaits here — they're already off the event loop. But we still want to return a command ID for consistency.

Simpler approach: keep issue commands as inline `await` (they're already backgrounded by the TUI), just return a command ID:

```rust
async fn execute(&self, repo: &Path, command: Command) -> Result<u64, String> {
    let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);

    // Issue commands: execute inline (already backgrounded by TUI), return ID
    match &command {
        Command::SetIssueViewport { visible_count, .. } => {
            self.ensure_issues_cached(repo, *visible_count * 2).await;
            self.broadcast_snapshot(repo).await;
            return Ok(id);
        }
        Command::FetchMoreIssues { desired_count, .. } => {
            self.ensure_issues_cached(repo, *desired_count).await;
            self.broadcast_snapshot(repo).await;
            return Ok(id);
        }
        Command::SearchIssues { query, .. } => {
            self.search_issues(repo, query).await;
            self.broadcast_snapshot(repo).await;
            return Ok(id);
        }
        Command::ClearIssueSearch { .. } => {
            let mut repos = self.repos.write().await;
            if let Some(state) = repos.get_mut(repo) {
                state.search_results = None;
            }
            drop(repos);
            self.broadcast_snapshot(repo).await;
            return Ok(id);
        }
        _ => {}
    }

    // All other commands: broadcast started, spawn, broadcast finished
    let description = command.description().to_string();
    let repo_path = repo.to_path_buf();
    let _ = self.event_tx.send(DaemonEvent::CommandStarted {
        command_id: id,
        repo: repo_path.clone(),
        description,
    });

    let runner = Arc::clone(&self.runner);
    let event_tx = self.event_tx.clone();
    let (registry, providers_data) = {
        let repos = self.repos.read().await;
        let state = repos
            .get(repo)
            .ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
        (
            Arc::clone(&state.model.registry),
            Arc::clone(&state.model.data.providers),
        )
    };

    let repos_ref = {
        // We need a handle to trigger refresh. Store a clone of the RwLock.
        // InProcessDaemon owns repos as RwLock<HashMap<...>>.
        // We can't clone self (it's behind Arc), but we can use a Weak ref.
        // Actually, the poll loop already uses Arc::downgrade. Let's store
        // the repos RwLock in an Arc so we can share it.
        //
        // For now, just read what we need before spawning.
        // The refresh trigger is the key thing — we need the refresh_handle.
        let repos = self.repos.read().await;
        let trigger = repos.get(repo).map(|s| s.model.refresh_handle.trigger_tx.clone());
        trigger
    };

    tokio::spawn(async move {
        let result =
            executor::execute(command, &repo_path, &registry, &providers_data, &*runner).await;

        // Trigger a refresh
        if let Some(trigger_tx) = repos_ref {
            let _ = trigger_tx.send(());
        }

        let _ = event_tx.send(DaemonEvent::CommandFinished {
            command_id: id,
            repo: repo_path,
            result,
        });
    });

    Ok(id)
}
```

Note: Check how `trigger_refresh()` works — if it uses a watch/notify channel, we need to clone the sender. Look at the `RefreshHandle` struct to find the right field to clone. The implementer should check `crates/flotilla-core/src/refresh.rs` for the exact field name and type.

**Step 3: Update SocketDaemon**

In `crates/flotilla-tui/src/socket.rs:185-193`, change the return type:

```rust
async fn execute(&self, repo: &Path, command: Command) -> Result<u64, String> {
    let resp = self
        .request(
            "execute",
            serde_json::json!({ "repo": repo, "command": command }),
        )
        .await?;
    resp.parse::<u64>()
}
```

**Step 4: Update TUI executor call site**

In `crates/flotilla-tui/src/app/executor.rs`, the `execute` function (line 58-61) currently does:

```rust
match app.daemon.execute(&repo, cmd).await {
    Ok(result) => handle_result(result, app),
    Err(e) => app.model.status_message = Some(e),
}
```

Change to:

```rust
match app.daemon.execute(&repo, cmd).await {
    Ok(_command_id) => {
        // Result will arrive via CommandFinished event
    }
    Err(e) => app.model.status_message = Some(e),
}
```

Also update the issue command block (lines 40-54) — they currently spawn `daemon.execute` and ignore the result. Now `execute` returns `u64`, so the code stays the same (the `Ok(u64)` is ignored via `let _ =`).

**Step 5: Update daemon server handler**

In `crates/flotilla-daemon/src/server.rs:278-297`, the execute handler currently awaits the command result and returns it as the response. Change to return the command ID immediately:

```rust
"execute" => {
    let repo = match extract_repo_path(&params) {
        Ok(p) => p,
        Err(e) => return Message::error_response(id, e),
    };
    let command: Command = match params
        .get("command")
        .cloned()
        .ok_or_else(|| "missing 'command' field".to_string())
        .and_then(|v| {
            serde_json::from_value(v).map_err(|e| format!("invalid command: {e}"))
        }) {
        Ok(cmd) => cmd,
        Err(e) => return Message::error_response(id, e),
    };
    match daemon.execute(&repo, command).await {
        Ok(command_id) => Message::ok_response(id, &command_id),
        Err(e) => Message::error_response(id, e),
    }
}
```

**Step 6: Run full test suite**

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: All pass. The daemon now returns IDs; results arrive via events.

**Step 7: Commit**

```
feat: change DaemonHandle::execute to return command ID (#23)

Execute returns immediately with a u64 command ID. The actual result
arrives via CommandStarted/CommandFinished broadcast events. The
InProcessDaemon spawns command execution as a background task.
```

---

### Task 4: In-flight command tracking on App

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs:125-132` (App struct)
- Modify: `crates/flotilla-tui/src/app/mod.rs:134-150` (App::new)

**Step 1: Add InFlightCommand type and field to App**

In `crates/flotilla-tui/src/app/mod.rs`, add near the top (after `CommandQueue`):

```rust
/// A command that has been dispatched to the daemon and is awaiting completion.
pub struct InFlightCommand {
    pub repo: PathBuf,
    pub description: String,
}
```

Add to `App` struct:

```rust
pub in_flight: HashMap<u64, InFlightCommand>,
```

Initialize in `App::new`:

```rust
in_flight: HashMap::new(),
```

**Step 2: Run to verify it compiles**

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: All pass (new field is unused but present).

**Step 3: Commit**

```
feat: add in-flight command tracking to App (#23)
```

---

### Task 5: Handle CommandStarted/CommandFinished in TUI

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs:154-164` (handle_daemon_event)
- Modify: `crates/flotilla-tui/src/app/executor.rs:12-62` (execute function)

**Step 1: Update handle_daemon_event**

Replace the placeholder arms from Task 1 with proper in-flight tracking:

```rust
DaemonEvent::CommandStarted {
    command_id,
    repo,
    description,
} => {
    tracing::info!("command {command_id} started: {description}");
    self.in_flight.insert(
        command_id,
        InFlightCommand {
            repo,
            description,
        },
    );
}
DaemonEvent::CommandFinished {
    command_id,
    result,
    ..
} => {
    if let Some(_cmd) = self.in_flight.remove(&command_id) {
        tracing::info!("command {command_id} finished");
    }
    executor::handle_result(result, self);
}
```

**Step 2: Update executor to track dispatched commands**

In `crates/flotilla-tui/src/app/executor.rs`, update the main execute path. After `daemon.execute` returns the command ID, we no longer need to track it here — the `CommandStarted` event will insert it. But we should clear `status_message` on dispatch:

```rust
match app.daemon.execute(&repo, cmd).await {
    Ok(_command_id) => {
        // CommandStarted event will add to in_flight
        // CommandFinished event will call handle_result
    }
    Err(e) => app.model.status_message = Some(e),
}
```

**Step 3: Run tests**

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: All pass.

**Step 4: Commit**

```
feat: handle CommandStarted/CommandFinished events in TUI (#23)

In-flight commands are tracked in a HashMap. CommandStarted inserts,
CommandFinished removes and calls handle_result.
```

---

### Task 6: Show in-flight command status in the status bar

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs:114-120` (render_status_bar)

**Step 1: Pass in-flight commands to render_status_bar**

The `render_status_bar` function currently takes `model: &TuiModel` and `ui: &UiState`. It needs access to in-flight commands. The simplest approach: pass the `App`'s in-flight map, or add a helper that extracts the active repo's in-flight description.

Check how `render` calls `render_status_bar` — it likely passes model and ui separately. The in-flight map lives on `App`. Two options:
1. Pass `in_flight: &HashMap<u64, InFlightCommand>` as an extra parameter
2. Move in-flight info into `TuiModel` or `UiState`

Option 1 is simplest. Update the `render` function signature to accept the in-flight map (or the full `App`). Check `crates/flotilla-tui/src/ui.rs` for the `render` function signature and update accordingly.

In `render_status_bar`, add after the error check and before the mode-specific text:

```rust
// Show in-flight command progress for the active repo
let active_repo = &model.repo_order[model.active_repo];
let in_flight_desc: Option<&str> = in_flight
    .values()
    .find(|cmd| &cmd.repo == active_repo)
    .map(|cmd| cmd.description.as_str());

if let Some(desc) = in_flight_desc {
    let msg = format!(" {desc}");
    let status = Paragraph::new(msg).style(Style::default().fg(Color::Yellow));
    frame.render_widget(status, area);
    return;
}
```

This shows the first in-flight command's description in yellow, taking priority over the normal status bar text but not over errors.

**Step 2: Update render call chain**

The `render` public function in `ui.rs` is called from `main.rs:156`:

```rust
terminal.draw(|f| ui::render(&app.model, &mut app.ui, f))?;
```

Update to also pass in-flight:

```rust
terminal.draw(|f| ui::render(&app.model, &mut app.ui, &app.in_flight, f))?;
```

Update `render` signature and forward to `render_status_bar`.

**Step 3: Run to verify**

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: All pass.

**Step 4: Commit**

```
feat: show in-flight command progress in status bar (#23)

Active repo's in-flight commands display in yellow in the status bar,
taking priority over normal hints but not over errors.
```

---

### Task 7: Make the event loop non-blocking

**Files:**
- Modify: `src/main.rs:296-299` (command draining loop)
- Modify: `crates/flotilla-tui/src/app/executor.rs:12-62` (execute function)

**Step 1: Verify current behavior**

The command loop at `src/main.rs:296-299` calls `app::executor::execute(cmd, &mut app).await`. Since `daemon.execute()` now returns immediately (just a command ID), this `await` is already fast. But the issue commands are still spawned from the TUI executor.

Check: are there any remaining slow `await`s in `executor::execute`? The daemon-level commands (`AddRepo`, `RemoveRepo`, `Refresh`) still await inline. These should also be fast (just sending a message to the daemon).

The main change is ensuring the TUI executor doesn't do any slow work. Review the full function and confirm all paths return quickly:

- `AddRepo` → `daemon.add_repo()` — sends message, fast
- `RemoveRepo` → `daemon.remove_repo()` — sends message, fast
- `Refresh` → `daemon.refresh()` — sends message, fast
- Issue commands → `tokio::spawn` — instant
- All other commands → `daemon.execute()` — now returns ID instantly

If all paths are fast, the event loop is already non-blocking after Task 3. This task is just verification and cleanup.

**Step 2: Rename `execute` to `dispatch` for clarity**

In `crates/flotilla-tui/src/app/executor.rs`, rename the function:

```rust
pub async fn dispatch(cmd: Command, app: &mut App) {
```

Update the call site in `src/main.rs:298`:

```rust
app::executor::dispatch(cmd, &mut app).await;
```

**Step 3: Run full suite**

Run: `cargo fmt && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: All pass.

**Step 4: Commit**

```
feat: rename executor::execute to dispatch (#23)

The function no longer awaits command completion — it dispatches to the
daemon and returns immediately. The rename makes this clear.
```

---

### Task 8: Integration test — end-to-end async command flow

**Files:**
- Check existing integration test patterns in the project

**Step 1: Find existing integration test patterns**

Look for tests that use `InProcessDaemon` or mock `DaemonHandle`. Check `crates/flotilla-core/src/` and `crates/flotilla-tui/src/` for test modules.

**Step 2: Write integration test**

If there's a pattern for daemon integration tests, write one that:
1. Creates an `InProcessDaemon`
2. Subscribes to events
3. Calls `execute` for a command
4. Verifies it gets `CommandStarted` then `CommandFinished` events
5. Verifies the command ID matches

If no integration test pattern exists, add a unit test in `crates/flotilla-core/src/in_process.rs` (may need a mock registry).

If mocking is too complex for this pass, skip and verify manually.

**Step 3: Run full suite**

Run: `cargo fmt && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: All pass.

**Step 4: Commit**

```
test: async command lifecycle integration test (#23)
```

---

### Task 9: Final verification and cleanup

**Step 1: Run full CI checks**

```bash
cargo fmt
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

**Step 2: Manual smoke test**

Run the app, trigger slow commands (branch name generation, delete confirmation, worktree creation) and verify:
- UI stays responsive (can navigate, switch tabs)
- Status bar shows progress text
- Result arrives and is handled correctly (branch input prefilled, delete modal populated)
- Existing spinner states animate (BranchInput generating, DeleteConfirm loading)

**Step 3: Commit any fixes**

```
fix: address issues found during manual testing (#23)
```
