# Concurrent Command Execution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single-slot command gate in `InProcessDaemon` with a concurrent command map, so lightweight commands are not blocked by long-running ones.

**Architecture:** Replace `active_command: Option<ActiveCommand>` with `HashMap<u64, CancellationToken>`. Drop the rejection logic. Fix the TUI's Esc targeting to use the highest command ID (most recent). Fix the status bar to show the most recent command.

**Tech Stack:** Rust, tokio, tokio-util (CancellationToken)

**Spec:** `docs/superpowers/specs/2026-03-22-concurrent-command-execution-design.md`

**CI commands:**
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

---

### Task 1: Replace single-slot gate with concurrent command map

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`

- [ ] **Step 1: Replace ActiveCommand struct and field**

Delete the `ActiveCommand` struct (lines 467-471):

```rust
/// Tracks a currently executing step-based command for cancellation.
struct ActiveCommand {
    command_id: u64,
    token: CancellationToken,
}
```

Change the field on `InProcessDaemon` (line 614) from:

```rust
/// The currently active step-based command, if any — for cancellation.
active_command: Arc<Mutex<Option<ActiveCommand>>>,
```

to:

```rust
/// Running commands, keyed by command ID, for cancellation.
active_commands: Arc<Mutex<HashMap<u64, CancellationToken>>>,
```

Update the initialization (line 710) from:

```rust
active_command: Arc::new(Mutex::new(None)),
```

to:

```rust
active_commands: Arc::new(Mutex::new(HashMap::new())),
```

Add `HashMap` to the `std::collections` import at the top of the file if not already present.

- [ ] **Step 2: Update the execute method — registration**

In the `execute` method, find `let active_ref = Arc::clone(&self.active_command);` (line 2225) and change to:

```rust
let active_ref = Arc::clone(&self.active_commands);
```

Then replace the entire rejection block (lines 2265-2284):

```rust
                    // Reject if another step command is already running.
                    // Single-slot design: one step command at a time (global).
                    // Hold the lock across check-and-set to avoid TOCTOU races.
                    let token = CancellationToken::new();
                    {
                        let mut guard = active_ref.lock().await;
                        if let Some(active) = &*guard {
                            let _ = event_tx.send(DaemonEvent::CommandFinished {
                                command_id: id,
                                host: command_host.clone(),
                                repo_identity: repo_identity.clone(),
                                repo: repo_path,
                                result: flotilla_protocol::CommandValue::Error {
                                    message: format!("another command is already running (id {})", active.command_id),
                                },
                            });
                            return;
                        }
                        *guard = Some(ActiveCommand { command_id: id, token: token.clone() });
                    }
```

with:

```rust
                    let token = CancellationToken::new();
                    {
                        let mut guard = active_ref.lock().await;
                        guard.insert(id, token.clone());
                    }
```

- [ ] **Step 3: Update the execute method — cleanup**

Replace the cleanup block after `run_step_plan` (lines 2308-2311):

```rust
                    let mut guard = active_ref.lock().await;
                    if guard.as_ref().map(|a| a.command_id) == Some(id) {
                        *guard = None;
                    }
```

with:

```rust
                    let mut guard = active_ref.lock().await;
                    guard.remove(&id);
```

- [ ] **Step 4: Update the cancel method**

Replace the `cancel` method (lines 2326-2335):

```rust
    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        let guard = self.active_command.lock().await;
        match &*guard {
            Some(active) if active.command_id == command_id => {
                active.token.cancel();
                Ok(())
            }
            _ => Err("no matching active command".into()),
        }
    }
```

with:

```rust
    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        let guard = self.active_commands.lock().await;
        match guard.get(&command_id) {
            Some(token) => {
                token.cancel();
                Ok(())
            }
            None => Err("no matching active command".into()),
        }
    }
```

- [ ] **Step 5: Verify existing tests pass**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`
Expected: PASS — the existing cancellation tests (`archive_session_can_be_cancelled`, `generate_branch_name_can_be_cancelled`, `cancel_nonexistent_command_returns_error`) should pass unchanged.

- [ ] **Step 6: Add test for concurrent command execution**

In `crates/flotilla-core/tests/in_process_daemon.rs`, add a test that verifies two commands can run concurrently — the core behavior being introduced. Use the existing `BlockingCloudAgent` pattern from the archive cancellation test: start a slow command, start a second command while the first is still running, and assert the second is NOT rejected (both produce `CommandFinished` events).

The test should:
1. Register a `BlockingCloudAgent` and a mock `AiUtility` (or similar slow + fast pair)
2. Start an `ArchiveSession` command (which blocks on the cloud agent)
3. While it's blocking, start a `GenerateBranchName` command (or `OpenChangeRequest`, etc.)
4. Assert the second command completes successfully (receives `CommandFinished`, not an error)
5. Release the first command and assert it completes too

Use the existing test helpers and `wait_for_event` pattern already in the file.

- [ ] **Step 7: Run all tests**

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs
git commit -m "refactor: replace single-slot command gate with concurrent command map"
```

---

### Task 2: Fix TUI Esc targeting and status bar

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/repo_page.rs:240-244`
- Modify: `crates/flotilla-tui/src/widgets/status_bar_widget.rs:238-251`

- [ ] **Step 1: Change dismiss to target highest command ID**

In `crates/flotilla-tui/src/widgets/repo_page.rs`, change the `dismiss` method (line 242) from:

```rust
        if let Some(&command_id) = ctx.in_flight.keys().next() {
```

to:

```rust
        if let Some(&command_id) = ctx.in_flight.keys().max() {
```

- [ ] **Step 2: Update the dismiss test**

The test `dismiss_cascade_cancels_in_flight_first` (line 569) inserts a single command with ID 42. With only one entry, `max()` and `next()` return the same thing, so the test passes unchanged. But add a second test to verify LIFO behavior:

```rust
    #[test]
    fn dismiss_cancels_most_recent_command() {
        let mut page = page_with_items(vec![issue_item("1")]);
        let mut harness = TestWidgetHarness::new();
        let repo_identity = harness.model.repo_order[0].clone();
        harness.in_flight.insert(10, crate::app::InFlightCommand {
            repo_identity: repo_identity.clone(),
            repo: PathBuf::from("/tmp/test-repo"),
            description: "older".into(),
        });
        harness.in_flight.insert(20, crate::app::InFlightCommand {
            repo_identity,
            repo: PathBuf::from("/tmp/test-repo"),
            description: "newer".into(),
        });
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(
            ctx.app_actions.iter().any(|a| matches!(a, AppAction::CancelCommand(20))),
            "should cancel the most recent command (highest ID)"
        );
    }
```

- [ ] **Step 3: Update active_task to show most recent command**

In `crates/flotilla-tui/src/widgets/status_bar_widget.rs`, change the `active_task` function (lines 238-251). Currently it collects descriptions from arbitrary iteration order. Change it to find the most recent command (highest ID) for the active repo:

```rust
pub(crate) fn active_task(model: &TuiModel, in_flight: &HashMap<u64, InFlightCommand>) -> Option<TaskSection> {
    let active_repo = &model.repo_order[model.active_repo];
    let repo_cmds: Vec<(&u64, &InFlightCommand)> =
        in_flight.iter().filter(|(_, cmd)| &cmd.repo_identity == active_repo).collect();

    if repo_cmds.is_empty() {
        return None;
    }

    let most_recent = repo_cmds.iter().max_by_key(|(id, _)| *id).expect("non-empty").1;
    let description = if repo_cmds.len() == 1 {
        most_recent.description.clone()
    } else {
        format!("{} (+{})", most_recent.description, repo_cmds.len() - 1)
    };

    Some(TaskSection::new(&description, 0))
}
```

- [ ] **Step 4: Add test for active_task with multiple commands**

In `crates/flotilla-tui/src/widgets/status_bar_widget.rs`, add a test that verifies the `(+N)` suffix when multiple commands are in flight for the same repo, and that the most recent command's description is shown:

```rust
#[test]
fn active_task_shows_most_recent_with_count() {
    let model = /* build a TuiModel with one repo */;
    let repo_identity = model.repo_order[0].clone();
    let mut in_flight = HashMap::new();
    in_flight.insert(10, InFlightCommand {
        repo_identity: repo_identity.clone(),
        repo: PathBuf::from("/tmp/repo"),
        description: "Older command...".into(),
    });
    in_flight.insert(20, InFlightCommand {
        repo_identity,
        repo: PathBuf::from("/tmp/repo"),
        description: "Newer command...".into(),
    });

    let task = active_task(&model, &in_flight).expect("should have active task");
    assert!(task.description.contains("Newer command..."), "should show most recent");
    assert!(task.description.contains("(+1)"), "should show count of other commands");
}
```

Use the existing test model helpers in the status bar test module (or neighboring widget tests) to construct the `TuiModel`.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 5: Run full CI**

```bash
cargo +nightly-2026-03-12 fmt
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
Expected: all PASS

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-tui/
git commit -m "fix: Esc cancels most recent command, status bar shows most recent"
```
