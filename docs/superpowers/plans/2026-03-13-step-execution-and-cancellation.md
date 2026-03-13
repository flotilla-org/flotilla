# Step-Based Command Execution and Cancellation Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace monolithic command execution with a step-based model that provides per-step progress visibility and cancellation support.

**Architecture:** Multi-step commands return a `StepPlan` (list of idempotent steps) instead of executing inline. A `StepRunner` drives execution with cancellation token checks between steps. Progress is reported via `CommandStepUpdate` events. Simple commands stay as `Immediate` results. Single "real" command slot with `Esc` to cancel.

**Tech Stack:** Rust, tokio, tokio-util (CancellationToken), async-trait, serde

**Spec:** `docs/superpowers/specs/2026-03-13-step-execution-and-cancellation-design.md`

---

## Chunk 1: Protocol Layer

### Task 1: Add `StepStatus` type to protocol

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs`

- [ ] **Step 1: Add StepStatus enum**

After the `CommandResult` enum (after line 114), add:

```rust
/// Status of an individual step within a multi-step command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status")]
pub enum StepStatus {
    Skipped,
    Started,
    Succeeded,
    Failed { message: String },
}
```

- [ ] **Step 2: Re-export StepStatus from lib.rs**

In `crates/flotilla-protocol/src/lib.rs`, add to the existing re-exports:

```rust
pub use commands::StepStatus;
```

- [ ] **Step 3: Add serde roundtrip test for StepStatus**

In the `tests` module, add:

```rust
#[test]
fn step_status_roundtrip() {
    let cases = vec![
        StepStatus::Skipped,
        StepStatus::Started,
        StepStatus::Succeeded,
        StepStatus::Failed { message: "workspace creation failed".into() },
    ];
    for case in cases {
        assert_json_roundtrip(&case);
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-protocol --locked`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/commands.rs crates/flotilla-protocol/src/lib.rs
git commit -m "feat(protocol): add StepStatus type for step-based execution (#58, #146)"
```

### Task 2: Add `CommandResult::Cancelled` variant

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs`

- [ ] **Step 1: Add Cancelled variant**

In the `CommandResult` enum (line ~112), add after `Error { message: String }`:

```rust
Cancelled,
```

- [ ] **Step 2: Add description arm**

In `CommandResult::description()` (if it exists — check the match), add:

```rust
CommandResult::Cancelled => "Cancelled",
```

- [ ] **Step 3: Update `command_roundtrip_covers_all_variants` test**

In the `result_cases` vec within the roundtrip test, add:

```rust
CommandResult::Cancelled,
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-protocol --locked`
Expected: All pass. If there's a `command_description_covers_all_variants` test for results, update it too.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/commands.rs
git commit -m "feat(protocol): add CommandResult::Cancelled variant (#146)"
```

### Task 3: Add `CommandStepUpdate` event variant

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Add CommandStepUpdate to DaemonEvent**

In the `DaemonEvent` enum (line ~119), add after `CommandFinished`:

```rust
CommandStepUpdate {
    command_id: u64,
    repo: std::path::PathBuf,
    step_index: usize,
    step_count: usize,
    description: String,
    status: commands::StepStatus,
},
```

- [ ] **Step 2: Update any exhaustive match tests**

Search for exhaustive match tests on `DaemonEvent`. If found, add the new variant. The `commands::StepStatus` import may be needed.

- [ ] **Step 3: Add placeholder match arms in downstream crates**

Adding `CommandStepUpdate` to `DaemonEvent` and `Cancelled` to `CommandResult` will break exhaustive matches in other crates. Add placeholder arms to keep the workspace compiling:

In `crates/flotilla-tui/src/app/mod.rs`, in `handle_daemon_event()`, add:

```rust
DaemonEvent::CommandStepUpdate { .. } => {
    // TODO: full handling in Task 16
}
```

In `crates/flotilla-tui/src/app/executor.rs`, in `handle_result()`, add:

```rust
CommandResult::Cancelled => {
    // TODO: full handling in Task 16
}
```

In `crates/flotilla-client/src/lib.rs`, check if there are exhaustive matches on `DaemonEvent` or `CommandResult` that need the new variants.

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace --locked`
Expected: All pass across all crates.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/lib.rs crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/executor.rs
git commit -m "feat(protocol): add CommandStepUpdate event for per-step progress (#58)"
```

### Task 4: Add `cancel` to `DaemonHandle` trait

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs`

- [ ] **Step 1: Add cancel method to DaemonHandle**

In the `DaemonHandle` trait (line ~12), add after `execute`:

```rust
async fn cancel(&self, command_id: u64) -> Result<(), String>;
```

- [ ] **Step 2: Check compilation**

Run: `cargo check -p flotilla-core --locked 2>&1 | head -20`
Expected: Compilation errors in `InProcessDaemon` and `SocketDaemon` — they don't implement `cancel` yet. This is expected; we'll fix them in later tasks.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/daemon.rs
git commit -m "feat(core): add cancel() to DaemonHandle trait (#146)"
```

## Chunk 2: Core Step Infrastructure

### Task 5: Add `tokio-util` dependency

**Files:**
- Modify: `crates/flotilla-core/Cargo.toml`

- [ ] **Step 1: Add tokio-util dependency**

Add to `[dependencies]`:

```toml
tokio-util = "0.7"
```

`CancellationToken` is in `tokio_util::sync` and requires no feature flags (available with default features).

- [ ] **Step 2: Verify it resolves**

Run `cargo check -p flotilla-core` (without `--locked`) first to update the lockfile with the new dependency. Then verify with `cargo check -p flotilla-core --locked`.
Expected: May fail on trait impl errors from Task 4, but the dependency should resolve.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/Cargo.toml Cargo.lock
git commit -m "chore(core): add tokio-util dependency for CancellationToken (#146)"
```

### Task 6: Create `step.rs` module with core types

**Files:**
- Create: `crates/flotilla-core/src/step.rs`
- Modify: `crates/flotilla-core/src/lib.rs`

- [ ] **Step 1: Create step.rs with StepPlan, Step, StepOutcome**

Create `crates/flotilla-core/src/step.rs`:

```rust
use std::future::Future;
use std::pin::Pin;

use flotilla_protocol::CommandResult;

/// Outcome of a single step execution.
pub enum StepOutcome {
    /// Step completed successfully, no specific result to report.
    Completed,
    /// Step completed and wants to override the final CommandResult.
    CompletedWith(CommandResult),
    /// Step determined its work was already done and skipped.
    Skipped,
}

/// A single step in a multi-step command.
pub struct Step {
    pub description: String,
    pub action: Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Result<StepOutcome, String>> + Send>> + Send>,
}

/// A plan of steps to execute for a command.
pub struct StepPlan {
    pub steps: Vec<Step>,
}

impl StepPlan {
    pub fn new(steps: Vec<Step>) -> Self {
        Self { steps }
    }
}
```

- [ ] **Step 2: Add module declaration to lib.rs**

In `crates/flotilla-core/src/lib.rs`, add after `pub mod refresh;` (line ~13):

```rust
pub mod step;
```

- [ ] **Step 3: Check compilation**

Run: `cargo check -p flotilla-core --locked 2>&1 | head -10`
Expected: May still have errors from DaemonHandle cancel(), but step.rs should compile.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/step.rs crates/flotilla-core/src/lib.rs
git commit -m "feat(core): add StepPlan, Step, StepOutcome types (#58)"
```

### Task 7: Add StepRunner with tests

**Files:**
- Modify: `crates/flotilla-core/src/step.rs`

- [ ] **Step 1: Add run_step_plan function**

Add to `step.rs`, after the `StepPlan` impl:

```rust
use std::path::PathBuf;

use flotilla_protocol::{DaemonEvent, StepStatus};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Execute a step plan, emitting progress events and checking cancellation between steps.
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
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    repo: repo.clone(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status: StepStatus::Succeeded,
                });
            }
            Ok(StepOutcome::CompletedWith(result)) => {
                final_result = result;
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    repo: repo.clone(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status: StepStatus::Succeeded,
                });
            }
            Ok(StepOutcome::Skipped) => {
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    repo: repo.clone(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status: StepStatus::Skipped,
                });
            }
            Err(e) => {
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    repo: repo.clone(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status: StepStatus::Failed { message: e.clone() },
                });
                return CommandResult::Error { message: e };
            }
        }
    }

    final_result
}
```

- [ ] **Step 2: Write tests for the runner**

Add to `step.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn make_step(desc: &str, outcome: Result<StepOutcome, String>) -> Step {
        let outcome = std::sync::Arc::new(tokio::sync::Mutex::new(Some(outcome)));
        Step {
            description: desc.to_string(),
            action: Box::new(move || {
                let outcome = Arc::clone(&outcome);
                Box::pin(async move { outcome.lock().await.take().expect("step called twice") })
            }),
        }
    }

    fn setup() -> (CancellationToken, broadcast::Sender<DaemonEvent>) {
        let (tx, _rx) = broadcast::channel(64);
        (CancellationToken::new(), tx)
    }

    #[tokio::test]
    async fn all_steps_succeed() {
        let (cancel, tx) = setup();
        let mut rx = tx.subscribe();
        let plan = StepPlan::new(vec![
            make_step("step-a", Ok(StepOutcome::Completed)),
            make_step("step-b", Ok(StepOutcome::Completed)),
        ]);

        let result = run_step_plan(plan, 1, PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Ok);

        // Should have 4 events: Started+Succeeded for each step
        let mut events = vec![];
        while let Ok(evt) = rx.try_recv() {
            events.push(evt);
        }
        assert_eq!(events.len(), 4);
    }

    #[tokio::test]
    async fn step_failure_stops_execution() {
        let (cancel, tx) = setup();
        let plan = StepPlan::new(vec![
            make_step("step-a", Ok(StepOutcome::Completed)),
            make_step("step-b", Err("boom".into())),
            make_step("step-c", Ok(StepOutcome::Completed)),
        ]);

        let result = run_step_plan(plan, 1, PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Error { message: "boom".into() });
    }

    #[tokio::test]
    async fn cancellation_before_step() {
        let (cancel, tx) = setup();
        cancel.cancel(); // cancel immediately
        let plan = StepPlan::new(vec![
            make_step("step-a", Ok(StepOutcome::Completed)),
        ]);

        let result = run_step_plan(plan, 1, PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Cancelled);
    }

    #[tokio::test]
    async fn skipped_step_continues() {
        let (cancel, tx) = setup();
        let plan = StepPlan::new(vec![
            make_step("step-a", Ok(StepOutcome::Skipped)),
            make_step("step-b", Ok(StepOutcome::Completed)),
        ]);

        let result = run_step_plan(plan, 1, PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Ok);
    }

    #[tokio::test]
    async fn completed_with_overrides_result() {
        let (cancel, tx) = setup();
        let plan = StepPlan::new(vec![
            make_step("step-a", Ok(StepOutcome::CompletedWith(
                CommandResult::CheckoutCreated { branch: "feat/x".into() }
            ))),
            make_step("step-b", Ok(StepOutcome::Completed)),
        ]);

        let result = run_step_plan(plan, 1, PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::CheckoutCreated { branch: "feat/x".into() });
    }

    #[tokio::test]
    async fn empty_plan_returns_ok() {
        let (cancel, tx) = setup();
        let plan = StepPlan::new(vec![]);

        let result = run_step_plan(plan, 1, PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Ok);
    }
}
```

Note: This requires `CommandResult` to derive `PartialEq`. If it doesn't already, add `PartialEq` to its derive list in `commands.rs`.

- [ ] **Step 3: Add PartialEq to CommandResult if needed**

Check if `CommandResult` already derives `PartialEq` in `crates/flotilla-protocol/src/commands.rs`. If not, add it to the derive list.

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core --locked -- step`
Expected: All 6 step runner tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/step.rs crates/flotilla-protocol/src/commands.rs
git commit -m "feat(core): add StepRunner with cancellation and progress events (#58, #146)"
```

## Chunk 3: Executor Transformation

### Task 8: Add `ExecutionPlan` and `build_plan` function

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`

This is the largest task — transforming `execute()` into `build_plan()`. The approach: add `build_plan` alongside the existing `execute` function, then switch callers over, then remove `execute`.

- [ ] **Step 1: Add ExecutionPlan enum and build_plan signature**

At the top of `executor.rs`, add after the imports:

```rust
use std::sync::Arc;

use crate::step::{Step, StepOutcome, StepPlan};

/// The result of planning a command's execution.
pub enum ExecutionPlan {
    /// Command completed immediately (single-step commands).
    Immediate(CommandResult),
    /// Command requires multiple steps.
    Steps(StepPlan),
}
```

Add the new function signature (leave body as a TODO that delegates to `execute` for now):

```rust
pub async fn build_plan(
    cmd: Command,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    runner: Arc<dyn CommandRunner>,
    config_base: PathBuf,
) -> ExecutionPlan {
    // Temporary: delegate to execute() during migration
    let result = execute(cmd, &repo_root, &*registry, &*providers_data, &*runner, &config_base).await;
    ExecutionPlan::Immediate(result)
}
```

- [ ] **Step 2: Check compilation**

Run: `cargo check -p flotilla-core --locked 2>&1 | head -10`
Expected: Compiles (though DaemonHandle cancel still missing impl).

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat(core): add ExecutionPlan enum and build_plan scaffold (#58)"
```

### Task 9: Convert `CreateCheckout` to step-based execution

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`

- [ ] **Step 1: Implement CreateCheckout as steps in build_plan**

In `build_plan`, replace the temporary delegation for `CreateCheckout` with step-based logic. The `CreateCheckout` arm in `build_plan` should:

Use an `Arc<Mutex<Option<PathBuf>>>` slot to pass the checkout path between the checkout step and workspace step. Pre-populate from existing state if the checkout already exists.

```rust
Command::CreateCheckout { branch, create_branch, issue_ids } => {
    let mut steps: Vec<Step> = vec![];
    let local_host = HostName::local();

    // Shared slot for passing checkout path between steps.
    // Pre-populate if checkout already exists (idempotent skip).
    let checkout_path_slot: Arc<tokio::sync::Mutex<Option<PathBuf>>> = Arc::new(
        tokio::sync::Mutex::new(
            providers_data.checkouts.iter()
                .find(|(hp, _)| hp.host == local_host)
                .filter(|(_, co)| co.branch == branch)
                .map(|(hp, _)| hp.path.clone())
        )
    );

    // Step 1: Ensure checkout exists
    let needs_checkout = checkout_path_slot.lock().await.is_none();
    if needs_checkout {
        if let Some((_, cm)) = registry.checkout_managers.values().next() {
            let cm = Arc::clone(cm);
            let repo = repo_root.clone();
            let br = branch.clone();
            let slot = Arc::clone(&checkout_path_slot);
            steps.push(Step {
                description: format!("Creating checkout for {}", branch),
                action: Box::new(move || {
                    Box::pin(async move {
                        match cm.create_checkout(&repo, &br, create_branch).await {
                            Ok((path, _checkout)) => {
                                *slot.lock().await = Some(path);
                                Ok(StepOutcome::CompletedWith(
                                    CommandResult::CheckoutCreated { branch: br }
                                ))
                            }
                            Err(e) => Err(e),
                        }
                    })
                }),
            });
        }
    }

    // Step 2: Link issues
    if !issue_ids.is_empty() {
        let repo = repo_root.clone();
        let br = branch.clone();
        let ids = issue_ids.clone();
        let r = Arc::clone(&runner);
        steps.push(Step {
            description: "Linking issues".into(),
            action: Box::new(move || {
                Box::pin(async move {
                    write_branch_issue_links(&repo, &br, &ids, &*r).await;
                    Ok(StepOutcome::Completed)
                })
            }),
        });
    }

    // Step 3: Create workspace (includes terminal pool resolution)
    if let Some((_, ws_mgr)) = &registry.workspace_manager {
        let ws = Arc::clone(ws_mgr);
        let tp = registry.terminal_pool.as_ref().map(|(_, tp)| Arc::clone(tp));
        let repo = repo_root.clone();
        let br = branch.clone();
        let cfg_base = config_base.clone();
        let slot = Arc::clone(&checkout_path_slot);

        steps.push(Step {
            description: "Creating workspace".into(),
            action: Box::new(move || {
                Box::pin(async move {
                    let checkout_path = slot.lock().await.clone()
                        .ok_or_else(|| "checkout path not available".to_string())?;
                    let mut config = workspace_config(&repo, &br, &checkout_path, "claude", &cfg_base);
                    if let Some(tp) = &tp {
                        resolve_terminal_pool(&mut config, tp.as_ref()).await;
                    }
                    if let Err(e) = ws.create_workspace(&config).await {
                        return Err(e);
                    }
                    Ok(StepOutcome::Completed)
                })
            }),
        });
    }

    if steps.is_empty() {
        ExecutionPlan::Immediate(CommandResult::Error {
            message: "no checkout manager available".into(),
        })
    } else {
        ExecutionPlan::Steps(StepPlan::new(steps))
    }
}
```

- [ ] **Step 2: Run existing executor tests**

Run: `cargo test -p flotilla-core --locked -- executor`
Expected: Existing tests still pass (they call `execute()` which is unchanged).

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat(core): convert CreateCheckout to step-based execution (#58)"
```

### Task 10: Convert `TeleportSession` to step-based execution

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`

- [ ] **Step 1: Implement TeleportSession as steps in build_plan**

Follow the same pattern as CreateCheckout. TeleportSession has 2-3 steps:

1. **Resolve attach command** — look up session, get attach command from cloud agent provider
2. **Ensure checkout** — if branch provided and no existing checkout, create one (skip if checkout_key points to existing checkout)
3. **Create workspace** — with the teleport command as the main command

Use the same `checkout_path_slot` pattern for passing the checkout path between steps.

```rust
Command::TeleportSession { session_id, branch, checkout_key } => {
    let mut steps: Vec<Step> = vec![];
    let local_host = HostName::local();

    // Shared state between steps
    let teleport_cmd_slot: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));
    // Pre-populate checkout path only if checkout_key references a known checkout
    let validated_checkout_path = checkout_key.as_ref().and_then(|key| {
        let host_key = HostPath::new(local_host.clone(), key.clone());
        providers_data.checkouts.get(&host_key).map(|_| key.clone())
    });
    let checkout_path_slot: Arc<tokio::sync::Mutex<Option<PathBuf>>> = Arc::new(tokio::sync::Mutex::new(
        validated_checkout_path
    ));

    // Step 1: Resolve attach command
    {
        let reg = Arc::clone(&registry);
        let pd = Arc::clone(&providers_data);
        let sid = session_id.clone();
        let slot = Arc::clone(&teleport_cmd_slot);
        steps.push(Step {
            description: "Resolving session attach command".into(),
            action: Box::new(move || {
                Box::pin(async move {
                    let cmd = resolve_attach_command(&sid, &*reg, &*pd).await?;
                    *slot.lock().await = Some(cmd);
                    Ok(StepOutcome::Completed)
                })
            }),
        });
    }

    // Step 2: Ensure checkout (if needed)
    if checkout_key.is_none() {
        if let Some(branch_name) = &branch {
            let checkout_exists = providers_data.checkouts.values().any(|co| co.branch == *branch_name);
            if !checkout_exists {
                if let Some((_, cm)) = registry.checkout_managers.values().next() {
                    let cm = Arc::clone(cm);
                    let repo = repo_root.clone();
                    let br = branch_name.clone();
                    let slot = Arc::clone(&checkout_path_slot);
                    steps.push(Step {
                        description: format!("Creating checkout for {}", branch_name),
                        action: Box::new(move || {
                            Box::pin(async move {
                                match cm.create_checkout(&repo, &br, false).await {
                                    Ok((path, _)) => {
                                        *slot.lock().await = Some(path);
                                        Ok(StepOutcome::Completed)
                                    }
                                    Err(e) => Err(e),
                                }
                            })
                        }),
                    });
                }
            }
        }
    }

    // Step 3: Create workspace with teleport command
    if let Some((_, ws_mgr)) = &registry.workspace_manager {
        let ws = Arc::clone(ws_mgr);
        let tp = registry.terminal_pool.as_ref().map(|(_, tp)| Arc::clone(tp));
        let repo = repo_root.clone();
        let name = branch.as_deref().unwrap_or("session").to_string();
        let cfg_base = config_base.clone();
        let cmd_slot = Arc::clone(&teleport_cmd_slot);
        let path_slot = Arc::clone(&checkout_path_slot);
        steps.push(Step {
            description: "Creating workspace".into(),
            action: Box::new(move || {
                Box::pin(async move {
                    let teleport_cmd = cmd_slot.lock().await.clone()
                        .ok_or_else(|| "attach command not resolved".to_string())?;
                    let checkout_path = path_slot.lock().await.clone()
                        .ok_or_else(|| "could not determine checkout path for teleport".to_string())?;
                    let mut config = workspace_config(&repo, &name, &checkout_path, &teleport_cmd, &cfg_base);
                    if let Some(tp) = &tp {
                        resolve_terminal_pool(&mut config, tp.as_ref()).await;
                    }
                    if let Err(e) = ws.create_workspace(&config).await {
                        return Err(e);
                    }
                    Ok(StepOutcome::Completed)
                })
            }),
        });
    }

    if steps.is_empty() {
        ExecutionPlan::Immediate(CommandResult::Error {
            message: "no providers available for teleport".into(),
        })
    } else {
        ExecutionPlan::Steps(StepPlan::new(steps))
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: All pass.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat(core): convert TeleportSession to step-based execution (#58)"
```

### Task 11: Convert `RemoveCheckout` to step-based execution

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`

- [ ] **Step 1: Implement RemoveCheckout as steps in build_plan**

Two steps: remove checkout, then clean up terminals.

```rust
Command::RemoveCheckout { branch, terminal_keys } => {
    let mut steps: Vec<Step> = vec![];

    // Step 1: Remove checkout
    if let Some((_, cm)) = registry.checkout_managers.values().next() {
        let cm = Arc::clone(cm);
        let repo = repo_root.clone();
        let br = branch.clone();
        steps.push(Step {
            description: format!("Removing checkout {}", branch),
            action: Box::new(move || {
                Box::pin(async move {
                    cm.remove_checkout(&repo, &br).await.map_err(|e| e)?;
                    Ok(StepOutcome::Completed)
                })
            }),
        });
    }

    // Step 2: Clean up terminals (best-effort)
    if !terminal_keys.is_empty() {
        if let Some((_, tp)) = &registry.terminal_pool {
            let tp = Arc::clone(tp);
            let keys = terminal_keys.clone();
            steps.push(Step {
                description: "Cleaning up terminal sessions".into(),
                action: Box::new(move || {
                    Box::pin(async move {
                        for terminal_id in &keys {
                            if let Err(e) = tp.kill_terminal(terminal_id).await {
                                tracing::warn!(terminal = %terminal_id, err = %e, "failed to kill terminal");
                            }
                        }
                        Ok(StepOutcome::Completed)
                    })
                }),
            });
        }
    }

    if steps.is_empty() {
        ExecutionPlan::Immediate(CommandResult::Error {
            message: "no checkout manager available".into(),
        })
    } else {
        ExecutionPlan::Steps(StepPlan::new(steps))
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: All pass.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat(core): convert RemoveCheckout to step-based execution (#58)"
```

### Task 12: Route remaining commands through build_plan as Immediate

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`

- [ ] **Step 1: Add remaining commands as Immediate in build_plan**

All commands not yet handled in `build_plan` should delegate to the existing `execute()` function and wrap in `Immediate`:

```rust
// Default: single-step commands delegate to existing execute()
cmd => {
    let result = execute(cmd, &repo_root, &*registry, &*providers_data, &*runner, &config_base).await;
    ExecutionPlan::Immediate(result)
}
```

This is the catch-all arm. The old `execute()` function remains for now — it handles the simple commands via this delegation. As more commands are converted to steps, the catch-all shrinks. Full removal of `execute()` can happen in a follow-up when all commands are migrated or when the catch-all is small enough to inline.

- [ ] **Step 2: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: All pass.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat(core): route all commands through build_plan (#58)"
```

## Chunk 4: Daemon Integration

### Task 13: Implement `cancel` and step-aware `execute` in InProcessDaemon

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`

- [ ] **Step 1: Add ActiveCommand struct and field**

Add near the top of the file:

```rust
use tokio_util::sync::CancellationToken;

struct ActiveCommand {
    command_id: u64,
    token: CancellationToken,
}
```

Add to `InProcessDaemon` struct (after `next_command_id` field):

```rust
active_command: Arc<tokio::sync::Mutex<Option<ActiveCommand>>>,
```

Initialize in the constructor (`new()` or `build()`) as:

```rust
active_command: Arc::new(tokio::sync::Mutex::new(None)),
```

- [ ] **Step 2: Implement cancel()**

Add to the `DaemonHandle` impl for `InProcessDaemon`:

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

- [ ] **Step 3: Update execute() to use build_plan + run_step_plan**

Replace the spawned task in `execute()` (lines ~1059-1066) with:

```rust
use crate::step::run_step_plan;
use crate::executor::{self, ExecutionPlan};

// ... after gathering registry, providers_data, etc.

let plan = executor::build_plan(command, repo_path.clone(), registry, providers_data, runner, config_base).await;

match plan {
    ExecutionPlan::Immediate(result) => {
        refresh_trigger.notify_one();
        let _ = self.event_tx.send(DaemonEvent::CommandFinished {
            command_id: id,
            repo: repo_path,
            result,
        });
    }
    ExecutionPlan::Steps(step_plan) => {
        let token = CancellationToken::new();
        *self.active_command.lock().await = Some(ActiveCommand {
            command_id: id,
            token: token.clone(),
        });

        let active_ref = Arc::clone(&self.active_command);
        tokio::spawn(async move {
            let result = run_step_plan(step_plan, id, repo_path.clone(), token, event_tx.clone()).await;
            refresh_trigger.notify_one();
            *active_ref.lock().await = None;
            let _ = event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                repo: repo_path,
                result,
            });
        });
    }
}
```

Note: `build_plan` now takes owned/Arc types. The existing code already has `Arc::clone(&state.model.registry)` etc., so adjust the call to pass them directly instead of borrowing.

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs
git commit -m "feat(core): integrate StepRunner into InProcessDaemon with cancellation (#58, #146)"
```

### Task 14: Implement `cancel` in SocketDaemon

**Files:**
- Modify: `crates/flotilla-client/src/lib.rs`

- [ ] **Step 1: Add cancel() to SocketDaemon's DaemonHandle impl**

Following the pattern of `execute()` (line ~578):

Follow the pattern of other void-returning methods (e.g., `refresh`, `add_repo`):

```rust
async fn cancel(&self, command_id: u64) -> Result<(), String> {
    self.request("cancel", serde_json::json!({ "command_id": command_id })).await?;
    Ok(())
}
```

- [ ] **Step 2: Check compilation**

Run: `cargo check -p flotilla-client --locked`
Expected: Compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-client/src/lib.rs
git commit -m "feat(client): add cancel() to SocketDaemon (#146)"
```

### Task 15: Add `cancel` handler to daemon server

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`

- [ ] **Step 1: Add cancel dispatch arm**

In `dispatch_request()` (line ~1050), add a new match arm after "execute":

```rust
"cancel" => {
    let command_id: u64 = match params
        .get("command_id")
        .and_then(|v| v.as_u64())
    {
        Some(id) => id,
        None => return Message::error_response(id, "missing or invalid 'command_id'".to_string()),
    };
    match daemon.cancel(command_id).await {
        Ok(()) => Message::empty_ok_response(id),
        Err(e) => Message::error_response(id, e),
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p flotilla-daemon --locked`
Expected: All pass.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs
git commit -m "feat(daemon): add cancel method handler to server dispatch (#146)"
```

## Chunk 5: TUI Integration

### Task 16: Handle `CommandStepUpdate` and `Cancelled` in TUI

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/executor.rs`

- [ ] **Step 1: Replace placeholder CommandStepUpdate handling in handle_daemon_event**

In `handle_daemon_event()` (line ~223 in `mod.rs`), replace the placeholder `CommandStepUpdate` arm (added in Task 3) with the full implementation:

```rust
DaemonEvent::CommandStepUpdate { command_id, description, step_index, step_count, status, .. } => {
    if let Some(cmd) = self.in_flight.get_mut(&command_id) {
        match status {
            StepStatus::Started => {
                cmd.description = format!("{} ({}/{})", description, step_index + 1, step_count);
            }
            StepStatus::Skipped => {
                tracing::info!(command_id, %description, "step skipped");
            }
            StepStatus::Succeeded => {
                tracing::info!(command_id, %description, "step succeeded");
            }
            StepStatus::Failed { ref message } => {
                tracing::warn!(command_id, %description, error = %message, "step failed");
            }
        }
    }
}
```

Add the necessary import at the top:

```rust
use flotilla_protocol::StepStatus;
```

- [ ] **Step 2: Replace placeholder CommandResult::Cancelled in executor.rs**

In `handle_result()` (line ~70 in `app/executor.rs`), replace the placeholder `Cancelled` arm (added in Task 3) with:

```rust
CommandResult::Cancelled => {
    app.model.status_message = Some("Command cancelled".into());
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/executor.rs
git commit -m "feat(tui): handle step progress events and cancellation result (#58, #146)"
```

### Task 17: Add Esc-to-cancel key binding

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/run.rs`

`handle_normal_key` is synchronous (`fn`, not `async fn`), so it cannot call `daemon.cancel()` directly. Use a `pending_cancel` field that the event loop drains, following the same pattern as `r` for refresh (which is handled in the main event loop rather than the key handler).

- [ ] **Step 1: Add `pending_cancel` field to App**

In `crates/flotilla-tui/src/app/mod.rs`, add to the `App` struct:

```rust
pub pending_cancel: Option<u64>,
```

Initialize as `None` in the constructor.

- [ ] **Step 2: Modify Esc handler in key_handlers.rs**

Note: also check if `run.rs` has `proto_commands` processing — the cancel drain should go **before** it.


In `handle_normal_key()` (line 82 of `key_handlers.rs`), replace the `Esc` arm:

```rust
KeyCode::Esc => {
    // Highest priority: cancel in-flight command
    if let Some(&command_id) = self.in_flight.keys().next() {
        self.pending_cancel = Some(command_id);
    } else if self.active_ui().active_search_query.is_some() {
        self.clear_active_issue_search(ClearDispatch::OnlyIfActive);
    } else if self.active_ui().show_providers {
        self.active_ui_mut().show_providers = false;
    } else if !self.active_ui().multi_selected.is_empty() {
        self.active_ui_mut().multi_selected.clear();
    } else {
        self.should_quit = true;
    }
}
```

- [ ] **Step 3: Drain pending_cancel in event loop**

In `crates/flotilla-tui/src/run.rs`, in `run_event_loop()`, after the `for evt in other_events` loop and **before** any `proto_commands` processing, add:

```rust
// Process pending cancel
if let Some(command_id) = app.pending_cancel.take() {
    let daemon = app.daemon.clone();
    tokio::spawn(async move {
        let _ = daemon.cancel(command_id).await;
    });
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/run.rs
git commit -m "feat(tui): add Esc-to-cancel for in-flight commands (#146)"
```

## Chunk 6: Cleanup and Final Testing

### Task 18: Update executor tests for build_plan

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`

- [ ] **Step 1: Add build_plan tests alongside existing execute tests**

Add new tests that exercise `build_plan` for the converted commands. Test that:

- `CreateCheckout` with no existing checkout returns `Steps` with 2-3 steps
- `CreateCheckout` with existing checkout returns `Steps` with fewer steps (checkout step skipped)
- `TeleportSession` returns `Steps`
- `RemoveCheckout` returns `Steps`
- Simple commands (e.g., `OpenChangeRequest`) return `Immediate`

Use the existing mock providers. Example:

```rust
#[tokio::test]
async fn build_plan_create_checkout_returns_steps() {
    let registry = /* set up with mock checkout manager + workspace manager */;
    let providers_data = /* empty checkouts */;
    let plan = build_plan(
        Command::CreateCheckout { branch: "feat/x".into(), create_branch: true, issue_ids: vec![] },
        PathBuf::from("/repo"),
        Arc::new(registry),
        Arc::new(providers_data),
        Arc::new(MockRunner::new()),
        PathBuf::from("/config"),
    ).await;

    match plan {
        ExecutionPlan::Steps(sp) => {
            assert!(sp.steps.len() >= 2, "expected checkout + workspace steps");
        }
        ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
    }
}
```

- [ ] **Step 2: Run all tests**

Run: `cargo test --locked`
Expected: All pass across all crates.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "test(core): add build_plan tests for step-based commands (#58)"
```

### Task 19: Run full CI checks

**Files:** None (verification only)

- [ ] **Step 1: Format**

Run: `cargo +nightly fmt`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Fix any warnings.

- [ ] **Step 3: Test**

Run: `cargo test --locked`
Expected: All pass.

- [ ] **Step 4: Commit any fixes**

Stage only the files that changed (check `git diff --name-only` first):

```bash
git add <changed files>
git commit -m "chore: fmt + clippy fixes for step execution (#58, #146)"
```
