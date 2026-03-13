# Step-Based Command Execution and Cancellation

Addresses [#58](https://github.com/rjwittams/flotilla/issues/58) (DAG-based intent execution with partial-failure visibility) and [#146](https://github.com/rjwittams/flotilla/issues/146) (cancellable long-running commands).

Multi-step commands (CreateCheckout, TeleportSession) currently execute as monolithic functions. If a late step fails, early steps have already succeeded but the user sees a single error or misleading success. There is no way to cancel a command once started.

This design introduces a step-based execution model with per-step progress events and cancellation support.

## Key Decisions

- **Shallow steps:** Steps are intent-level operations (create checkout, link issues, create workspace), not individual provider method calls. The step list per command is short (2-4 items).
- **Sequential execution:** The underlying data model is a DAG (some steps could parallelize), but execution is sequential for now. Parallel cases (multi-terminal startup, cross-machine sync) are deferred.
- **Idempotent "ensure" semantics:** `build_plan` inspects daemon state and omits steps whose work is already done. Re-running an intent after partial failure skips completed steps.
- **Single command slot:** One "real" command in flight at a time. Inline issue commands (viewport, search) are excluded and continue to execute without lifecycle events.
- **Cancel = stop everything:** Cancellation aborts the remaining pipeline. Idempotency handles resume — re-triggering the intent will skip completed steps.

## Three-Layer Model

```
TUI process                          Daemon process

Intent ──resolve(UI context)──→ Command ──wire──→ build_plan(state, providers) → StepPlan
                                                                                    ↓
                                                                              run_step_plan()
                                                                              + CancellationToken
                                                                                    ↓
                              CommandStepUpdate ←──────────────────── emit per-step progress
                              CommandFinished   ←──────────────────── emit final result
```

- **Intent** (TUI): UI abstraction. Resolves to a Command using selected WorkItem and app state. Lives in `flotilla-tui`.
- **Command** (wire): Serializable enum with resolved parameters. Crosses the daemon boundary. Lives in `flotilla-protocol`.
- **Step** (executor-internal): Closures over provider references. Cannot be serialized. Lives in `flotilla-core`.

## Protocol Changes

### New types in `flotilla-protocol`

```rust
pub enum StepStatus {
    Skipped,    // idempotent check: work already done
    Started,
    Succeeded,
    Failed(String),
}
```

### New DaemonEvent variant

```rust
CommandStepUpdate {
    command_id: u64,
    repo: PathBuf,
    step_index: usize,
    step_count: usize,
    description: String,
    status: StepStatus,
}
```

Existing `CommandStarted` and `CommandFinished` continue to bracket the whole command. `CommandStepUpdate` provides inner detail.

### CommandResult gains `Cancelled`

```rust
pub enum CommandResult {
    Ok,
    CheckoutCreated { branch: String },
    BranchNameGenerated { name: String, issue_ids: Vec<(String, String)> },
    CheckoutStatus(CheckoutStatus),
    Error { message: String },
    Cancelled, // new
}
```

Cancellation produces `CommandFinished { result: CommandResult::Cancelled }`. No separate `CommandCancelled` event.

### DaemonHandle gains `cancel`

```rust
async fn cancel(&self, command_id: u64) -> Result<(), String>;
```

## Core Types — `flotilla-core/src/step.rs`

New module.

### StepPlan and Step

```rust
pub struct StepPlan {
    pub steps: Vec<Step>,
}

pub struct Step {
    pub description: String,
    pub action: Box<dyn FnOnce() -> BoxFuture<'static, Result<StepOutcome, String>> + Send>,
}

// Note: step closures capture Arc<ProviderRegistry>, Arc<ProviderData>, etc.
// build_plan takes Arc references (not borrows) so closures can move clones
// into 'static futures.

pub enum StepOutcome {
    Completed,
    CompletedWith(CommandResult), // override the default Ok result
    Skipped,
}
```

Steps that need to produce a specific `CommandResult` (e.g., `CheckoutCreated { branch }`) return `CompletedWith`. The runner uses the last such value, or `CommandResult::Ok` if none.

### StepRunner

```rust
pub async fn run_step_plan(
    plan: StepPlan,
    command_id: u64,
    repo: PathBuf,
    cancel: CancellationToken,
    event_tx: broadcast::Sender<DaemonEvent>,
) -> CommandResult {
    let step_count = plan.steps.len();
    let mut final_result = CommandResult::Ok;

    for (i, step) in plan.steps.into_iter().enumerate() {
        if cancel.is_cancelled() {
            return CommandResult::Cancelled;
        }

        let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
            command_id,
            repo: repo.clone(),
            step_index: i,
            step_count,
            description: step.description.clone(),
            status: StepStatus::Started,
        });

        match (step.action)().await {
            Ok(StepOutcome::Completed) => {
                let _ = event_tx.send(/* StepStatus::Succeeded */);
            }
            Ok(StepOutcome::CompletedWith(result)) => {
                final_result = result;
                let _ = event_tx.send(/* StepStatus::Succeeded */);
            }
            Ok(StepOutcome::Skipped) => {
                let _ = event_tx.send(/* StepStatus::Skipped */);
            }
            Err(e) => {
                let _ = event_tx.send(/* StepStatus::Failed(e) */);
                return CommandResult::Error { message: e };
            }
        }
    }

    final_result
}
```

## Executor Changes

`executor::execute()` is replaced by `executor::build_plan()`:

```rust
pub enum ExecutionPlan {
    Immediate(CommandResult),
    Steps(StepPlan),
}

pub async fn build_plan(
    cmd: Command,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    runner: Arc<dyn CommandRunner>,
    config_base: PathBuf,
) -> ExecutionPlan
```

- **Simple commands** (`OpenChangeRequest`, `ArchiveSession`, `GenerateBranchName`, etc.) return `Immediate(result)` — executed inline during plan building, same as today.
- **Multi-step commands** (`CreateCheckout`, `TeleportSession`, `CreateWorkspaceForCheckout`) return `Steps(plan)` with closures capturing provider references.

### Example: CreateCheckout

```rust
Command::CreateCheckout { branch, create_branch, issue_ids } => {
    let mut steps = vec![];

    // Step 1: Ensure checkout exists
    // build_plan checks providers_data — if checkout already exists, omit this step
    if !checkout_exists(&branch, providers_data) {
        steps.push(Step {
            description: format!("Creating checkout for {}", branch),
            action: /* closure: cm.create_checkout(repo_root, &branch, create_branch) */,
        });
    }

    // Step 2: Link issues
    if !issue_ids.is_empty() {
        steps.push(Step {
            description: "Linking issues".into(),
            action: /* closure: write_branch_issue_links() */,
        });
    }

    // Step 3: Create workspace (includes terminal pool resolution)
    steps.push(Step {
        description: "Creating workspace".into(),
        action: /* closure: resolve_terminal_pool() then ws_mgr.create_workspace(&config) */,
    });

    // The checkout step returns CompletedWith(CheckoutCreated { branch })
    // to propagate the command-specific result

    ExecutionPlan::Steps(StepPlan { steps })
}
```

## InProcessDaemon Changes

`execute()` dispatches based on `ExecutionPlan`:

```rust
async fn execute(&self, repo: &Path, command: Command) -> Result<u64, String> {
    // Issue commands: inline, unchanged
    // ...

    let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);
    let plan = executor::build_plan(cmd, repo_root, registry, providers_data, runner, config_base).await;

    // Emit CommandStarted
    let _ = self.event_tx.send(DaemonEvent::CommandStarted { command_id: id, repo, description });

    match plan {
        ExecutionPlan::Immediate(result) => {
            // Execute synchronously in the current task — no spawn, no cancellation.
            // This is a behavioral change from today (which spawns all non-inline commands),
            // but Immediate commands are fast single calls so spawning adds no value.
            refresh_trigger.notify_one();
            let _ = self.event_tx.send(DaemonEvent::CommandFinished { command_id: id, repo, result });
        }
        ExecutionPlan::Steps(step_plan) => {
            let token = CancellationToken::new();
            *self.active_command.lock().await = Some(ActiveCommand { command_id: id, token: token.clone() });

            let active_ref = Arc::clone(&self.active_command);
            tokio::spawn(async move {
                let result = run_step_plan(step_plan, id, repo.clone(), token, event_tx.clone()).await;
                refresh_trigger.notify_one();
                // Clear active command before emitting CommandFinished to avoid
                // race where cancel() sees a stale entry after completion.
                *active_ref.lock().await = None;
                let _ = event_tx.send(DaemonEvent::CommandFinished { command_id: id, repo, result });
            });
        }
    }

    Ok(id)
}
```

### Active command tracking

```rust
struct ActiveCommand {
    command_id: u64,
    token: CancellationToken,
}

// Single slot — one real command at a time
active_command: Arc<Mutex<Option<ActiveCommand>>>,
```

`cancel()` implementation:

```rust
async fn cancel(&self, command_id: u64) -> Result<(), String> {
    let guard = self.active_command.lock().await;
    match &*guard {
        Some(active) if active.command_id == command_id => {
            active.token.cancel();
            Ok(())
        }
        _ => Err("No matching active command".into()),
    }
}
```

## SocketDaemon Changes

Both sides need updates:

- **Client (`SocketDaemon`):** `cancel()` sends a `"cancel"` JSON-RPC request with the command_id.
- **Server (`DaemonServer`):** New `"cancel"` method handler. The server needs access to the `ActiveCommand` state (same `Arc<Mutex<Option<ActiveCommand>>>`) to look up and cancel the token.

## TUI Changes

### Event handling

In `handle_daemon_event`:

```rust
DaemonEvent::CommandStepUpdate { command_id, description, step_index, step_count, status, .. } => {
    if let Some(cmd) = self.in_flight.get_mut(&command_id) {
        match status {
            StepStatus::Started => {
                cmd.description = format!("{}... ({}/{})", description, step_index + 1, step_count);
            }
            StepStatus::Skipped => {
                tracing::info!(%command_id, %description, "step skipped");
            }
            StepStatus::Succeeded => {
                tracing::info!(%command_id, %description, "step succeeded");
            }
            StepStatus::Failed(ref e) => {
                tracing::warn!(%command_id, %description, error = %e, "step failed");
            }
        }
    }
}
```

`CommandResult::Cancelled` in `handle_result`:
```rust
CommandResult::Cancelled => {
    app.model.status_message = Some("Command cancelled".into());
}
```

### Cancel key binding

In `handle_key`, Normal mode, `Esc` with an in-flight command cancels the command. This check takes **highest priority** in the Esc cascade — before clearing search, hiding providers panel, or clearing multi-select. Rationale: if the user triggered a command and immediately hits Esc, they want to stop the command, not toggle a panel.

```rust
KeyCode::Esc if !self.in_flight.is_empty() => {
    // Highest priority: cancel in-flight command before other Esc behaviors
    if let Some(&command_id) = self.in_flight.keys().next() {
        let daemon = self.daemon.clone();
        tokio::spawn(async move { let _ = daemon.cancel(command_id).await; });
    }
}
```

## What Changes for Each Command

Commands handled inline at the daemon level (issue viewport, search, repo management, refresh) are unchanged and omitted from this table.

| Command | Today | After |
|---------|-------|-------|
| `CreateCheckout` | Monolithic: checkout → links → workspace | 2-3 steps, skips checkout if exists |
| `TeleportSession` | Monolithic: resolve attach → checkout → workspace | 2-3 steps, skips checkout if exists |
| `CreateWorkspaceForCheckout` | Single provider call | 1 step (could stay Immediate) |
| `RemoveCheckout` | Checkout removal + terminal cleanup | 2 steps |
| `GenerateBranchName` | Single AI call with fallback | Immediate |
| `OpenChangeRequest` | Single runner.open() call | Immediate |
| `ArchiveSession` | Single provider call | Immediate |
| `FetchCheckoutStatus` | Git subprocess call | Immediate |
| `SelectWorkspace` | Single provider call | Immediate |
| `CloseChangeRequest` | Single API call | Immediate |
| `OpenIssue` | Single runner.open() call | Immediate |
| `LinkIssuesToChangeRequest` | Single API call | Immediate |

## Testing

- **StepRunner unit tests:** Plan with mixed outcomes (completed, skipped, failed). Cancellation between steps. Result override via `CompletedWith`.
- **build_plan tests:** Verify step lists for each multi-step command. Verify idempotent skip when state shows work already done.
- **Protocol tests:** Update `command_roundtrip_covers_all_variants` and `command_description_covers_all_variants` for `Cancelled` variant. Add `StepStatus` serde roundtrip tests.
- **Integration:** Existing executor tests adapted to call `build_plan` then `run_step_plan` instead of `execute` directly.

## Out of Scope

- **Parallel step execution:** DAG structure allows it, but all steps run sequentially for now.
- **Step-level retry:** Failed command → user re-triggers intent → `build_plan` skips completed steps. No automatic retry.
- **Progress widget:** No new UI widget for step list. Status bar + event log only.
- **Cancellation token threading into provider calls:** Steps check cancellation between steps. Threading tokens into HTTP clients or subprocess spawns is a future enhancement.
- **Multi-command concurrency:** Single "real" command slot. Expanding to concurrent commands would need a command list UI.

## New Dependencies

- `tokio-util` with `sync` feature (for `CancellationToken`) — added to `flotilla-core/Cargo.toml`.
