# All-Symbolic Step Execution — Batch 1: Eliminate Closures

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert all closure-based step actions to symbolic variants resolved by `ExecutorStepResolver`, eliminating `StepAction::Closure` and the Arc+Mutex slot pattern.

**Architecture:** Every `StepAction` becomes a data-only enum variant. The resolver's `resolve()` method dispatches each variant to a standalone function. `run_step_plan` no longer branches on `Closure` vs symbolic — it always delegates to the resolver. The `Produced(CommandValue)` outcome variant carries inter-step data without polluting the final result.

**Tech Stack:** Rust, async-trait, tokio, flotilla-protocol (serde), flotilla-core (executor, step)

**Spec:** `docs/superpowers/specs/2026-03-21-all-symbolic-step-execution-design.md`

**CI commands:**
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

---

### Task 1: Rename CommandResult → CommandValue in flotilla-protocol

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs:167-208` (enum definition)
- Modify: `crates/flotilla-protocol/src/lib.rs` (re-export)
- Modify: `crates/flotilla-protocol/src/peer.rs:5` (import)

- [ ] **Step 1: Rename the enum in commands.rs**

In `crates/flotilla-protocol/src/commands.rs`, rename `CommandResult` to `CommandValue` at the definition (line 170) and all references within the file. Add the two new inter-step variants:

```rust
/// Value produced by command execution or inter-step communication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandValue {
    // ... all existing variants unchanged ...

    // New inter-step variants
    AttachCommandResolved { command: String },
    CheckoutPathResolved { path: PathBuf },
}
```

Add the new variants to the roundtrip test `command_result_roundtrip_covers_all_variants` (rename to `command_value_roundtrip_covers_all_variants`).

- [ ] **Step 2: Update re-export in lib.rs**

Change `CommandResult` to `CommandValue` in the `pub use commands::` line. Add a type alias for backwards compat during the transition:

```rust
/// Deprecated alias — use CommandValue.
pub type CommandResult = CommandValue;
```

This lets us rename incrementally rather than changing every consumer in one commit.

- [ ] **Step 3: Update import in peer.rs**

Change `CommandResult` to `CommandValue` in the import on line 5.

- [ ] **Step 4: Run tests to verify protocol crate passes**

Run: `cargo test -p flotilla-protocol --locked`
Expected: PASS (alias keeps downstream code compiling)

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/
git commit -m "refactor: rename CommandResult to CommandValue, add inter-step variants"
```

---

### Task 2: Migrate CommandResult → CommandValue across all crates

**Files:**
- Modify: `crates/flotilla-core/src/step.rs:3` (import)
- Modify: `crates/flotilla-core/src/executor.rs:16` (import)
- Modify: `crates/flotilla-core/src/executor/session_actions.rs:3` (import)
- Modify: `crates/flotilla-core/src/in_process.rs` (import + usage)
- Modify: `crates/flotilla-tui/src/app/executor.rs:1` (import)
- Modify: `crates/flotilla-tui/src/cli.rs` (imports at lines 200, 740, 907)
- Modify: `crates/flotilla-daemon/tests/socket_roundtrip.rs:5` (import)
- Modify: `crates/flotilla-core/src/executor/tests.rs` (all refs)
- Modify: Any other files found by grep

- [ ] **Step 1: Find all remaining `CommandResult` references**

Run: `rg 'CommandResult' crates/ --type rust -l` to find all files. Expect ~20 files across all crates — the alias in lib.rs keeps them compiling during the transition. Also check for path-qualified refs like `commands::CommandResult` in event types (`DaemonEvent::CommandFinished` in `flotilla-protocol/src/lib.rs`).

- [ ] **Step 2: Replace `CommandResult` → `CommandValue` across all files**

Use search-and-replace across the codebase. The type alias in lib.rs means this can be done incrementally, but do it all at once for cleanliness.

- [ ] **Step 3: Remove the type alias from lib.rs**

Once all references are updated, remove `pub type CommandResult = CommandValue;` from `crates/flotilla-protocol/src/lib.rs`.

- [ ] **Step 4: Run full workspace tests**

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: complete CommandResult → CommandValue rename across all crates"
```

---

### Task 3: Add Produced variant to StepOutcome

**Files:**
- Modify: `crates/flotilla-core/src/step.rs:7-16` (StepOutcome enum)
- Modify: `crates/flotilla-core/src/step.rs:138-155` (final-result extraction)

- [ ] **Step 1: Add Produced variant**

In `crates/flotilla-core/src/step.rs`, add the `Produced` variant to `StepOutcome`:

```rust
pub enum StepOutcome {
    Completed,
    CompletedWith(CommandValue),
    /// Inter-step data visible to later steps but excluded from the final result.
    Produced(CommandValue),
    Skipped,
}
```

- [ ] **Step 2: Verify final-result logic excludes Produced**

The existing `find_map` at lines 138-141 and 148-154 already filters for `CompletedWith` only. Verify `Produced` is naturally excluded. The match exhaustiveness checker will flag any missing arms — fix them (they should be `_ => None` or `StepOutcome::Produced(_) => None`).

- [ ] **Step 3: Add a test for Produced-does-not-affect-final-result**

Add to `step.rs` tests:

```rust
#[tokio::test]
async fn produced_does_not_override_final_result() {
    let (cancel, tx) = setup();
    let plan = StepPlan::new(vec![
        make_step("step-a", Ok(StepOutcome::Produced(CommandValue::AttachCommandResolved {
            command: "attach cmd".into(),
        }))),
        make_step("step-b", Ok(StepOutcome::Completed)),
    ]);

    let result = run_step_plan(
        plan, 1, HostName::local(),
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        PathBuf::from("/repo"), cancel, tx, None,
    ).await;
    assert_eq!(result, CommandValue::Ok);
}
```

- [ ] **Step 4: Run step.rs tests**

Run: `cargo test -p flotilla-core --locked step::tests`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/step.rs
git commit -m "feat: add Produced variant to StepOutcome for inter-step data"
```

---

### Task 4: Add symbolic StepAction variants

**Files:**
- Modify: `crates/flotilla-core/src/step.rs:30-36` (StepAction enum)

- [ ] **Step 1: Add all batch 1 symbolic variants**

Replace the `StepAction` enum. Keep `Closure` for now (removed in task 6). Note: `CheckoutIntent` is `pub(super)` in `checkout.rs` — widen to `pub(crate)` since `StepAction` is in a different module.

```rust
pub enum StepAction {
    /// Opaque closure — removed after all plans are converted.
    Closure(Box<dyn FnOnce(Vec<StepOutcome>) -> StepFuture + Send>),

    // Checkout lifecycle
    CreateCheckout {
        branch: String,
        create_branch: bool,
        intent: CheckoutIntent,
        issue_ids: Vec<(String, String)>,
    },
    LinkIssuesToBranch {
        branch: String,
        issue_ids: Vec<(String, String)>,
    },
    RemoveCheckout {
        branch: String,
        terminal_keys: Vec<ManagedTerminalId>,
        deleted_checkout_paths: Vec<HostPath>,
    },

    // Workspace
    CreateWorkspaceForCheckout { label: String },

    // Teleport
    ResolveAttachCommand { session_id: String },
    EnsureCheckoutForTeleport {
        branch: Option<String>,
        checkout_key: Option<PathBuf>,
        initial_path: Option<PathBuf>,
    },
    CreateTeleportWorkspace {
        session_id: String,
        branch: Option<String>,
    },

    // Session
    ArchiveSession { session_id: String },
    GenerateBranchName { issue_keys: Vec<String> },
}
```

Add necessary imports to step.rs: `ManagedTerminalId`, `HostPath` from `flotilla_protocol`, and `CheckoutIntent` from the executor checkout module.

- [ ] **Step 2: Widen CheckoutIntent visibility**

In `crates/flotilla-core/src/executor/checkout.rs`, change:
```rust
pub(super) enum CheckoutIntent {
```
to:
```rust
pub(crate) enum CheckoutIntent {
```

- [ ] **Step 3: Fix compiler errors from new variants in resolve()**

The `ExecutorStepResolver::resolve()` match in `executor.rs:538-562` needs placeholder arms for the new variants. Add them as `todo!()` for now:

```rust
StepAction::CreateCheckout { .. } => todo!("task 7"),
StepAction::LinkIssuesToBranch { .. } => todo!("task 7"),
// ... etc for each new variant
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check --workspace --locked`
Expected: PASS (with dead_code warnings for unused variants, that's fine)

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/step.rs crates/flotilla-core/src/executor/checkout.rs crates/flotilla-core/src/executor.rs
git commit -m "feat: add symbolic StepAction variants for all plan steps"
```

---

### Task 5: Expand ExecutorStepResolver with required fields

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:527-534` (struct definition)
- Modify: `crates/flotilla-core/src/in_process.rs` (where resolver is constructed, ~line 2280)
- Modify: `crates/flotilla-core/src/executor/tests.rs` (6 construction sites at lines 1794, 2106, 2162, 2217, 2858, 2884)

- [ ] **Step 1: Add providers_data and runner fields**

In `crates/flotilla-core/src/executor.rs`, update the struct:

```rust
pub(crate) struct ExecutorStepResolver {
    pub repo: RepoExecutionContext,
    pub registry: Arc<ProviderRegistry>,
    pub providers_data: Arc<ProviderData>,
    pub runner: Arc<dyn CommandRunner>,
    pub config_base: PathBuf,
    pub attachable_store: SharedAttachableStore,
    pub daemon_socket_path: Option<PathBuf>,
    pub local_host: HostName,
}
```

- [ ] **Step 2: Update resolver construction in in_process.rs**

Find where `ExecutorStepResolver` is built (around line 2280 of `in_process.rs`). Add the new fields — `providers_data` and `runner` should already be available in that scope.

- [ ] **Step 3: Update all 6 resolver construction sites in executor tests**

In `crates/flotilla-core/src/executor/tests.rs`, update every `ExecutorStepResolver { ... }` literal (lines 1794, 2106, 2162, 2217, 2858, 2884) to include the new `providers_data` and `runner` fields. The `run_build_plan_to_completion` helper at line 1794 is the most important — it provides a reusable pattern for the others.

- [ ] **Step 4: Verify it compiles and tests pass**

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/
git commit -m "refactor: add providers_data and runner to ExecutorStepResolver"
```

---

### Task 6: Make resolver mandatory in run_step_plan

**Files:**
- Modify: `crates/flotilla-core/src/step.rs:64-99` (function signature + body)
- Modify: `crates/flotilla-core/src/step.rs:158-408` (tests)
- Modify: `crates/flotilla-core/src/in_process.rs` (call site)

- [ ] **Step 1: Create a TestResolver for step.rs tests**

The step.rs tests currently pass `None` for the resolver because they only use `Closure` steps. Add a minimal test resolver that panics on any symbolic action (tests won't hit it yet, but the signature requires it):

```rust
struct TestResolver;

#[async_trait::async_trait]
impl StepResolver for TestResolver {
    async fn resolve(&self, _desc: &str, action: StepAction, _prior: &[StepOutcome]) -> Result<StepOutcome, String> {
        match action {
            StepAction::Closure(_) => unreachable!("closures handled by stepper"),
            _ => panic!("TestResolver: unexpected symbolic action in step.rs unit test"),
        }
    }
}
```

- [ ] **Step 2: Change resolver parameter from Option to required**

In `run_step_plan` signature, change:
```rust
resolver: Option<&dyn StepResolver>,
```
to:
```rust
resolver: &dyn StepResolver,
```

Update the body — the `Closure` branch stays for now (removed in task 9), the symbolic branch drops the `match resolver { Some(r) => ..., None => ... }`:

```rust
let outcome = match step.action {
    StepAction::Closure(f) => f(outcomes.clone()).await,
    symbolic => resolver.resolve(&step.description, symbolic, &outcomes).await,
};
```

- [ ] **Step 3: Update all call sites**

- `in_process.rs`: change `Some(&resolver)` to `&resolver`
- `step.rs` tests: change `None` to `&TestResolver`
- `executor/tests.rs`: change `Some(&resolver)` to `&resolver`

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/
git commit -m "refactor: make StepResolver mandatory in run_step_plan"
```

---

### Task 7: Implement resolver functions for checkout + link issues

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs` (resolver match arms)

- [ ] **Step 1: Implement CreateCheckout resolver arm**

Replace the `todo!()` in the resolver's `CreateCheckout` arm. Extract the logic from `build_create_checkout_plan`'s step 1 closure (lines 278-300) into the resolver:

```rust
StepAction::CreateCheckout { branch, create_branch, intent, issue_ids } => {
    let checkout_flow = CheckoutFlow {
        branch: &branch,
        create_branch,
        intent,
        issue_ids: &issue_ids,
        repo_root: &self.repo.root,
        registry: self.registry.as_ref(),
        providers_data: self.providers_data.as_ref(),
        runner: self.runner.as_ref(),
        local_host: &self.local_host,
    };
    let result = checkout_flow
        .checkout_created_result(CheckoutExistingPolicy::ReuseKnownCheckout, CheckoutIssueLinkPolicy::Deferred)
        .await?;
    if let CommandValue::CheckoutCreated { path, .. } = &result {
        info!(checkout_path = %path.display(), "created checkout");
    }
    Ok(StepOutcome::CompletedWith(result))
}
```

- [ ] **Step 2: Implement LinkIssuesToBranch resolver arm**

```rust
StepAction::LinkIssuesToBranch { branch, issue_ids } => {
    write_branch_issue_links(&self.repo.root, &branch, &issue_ids, &*self.runner).await;
    Ok(StepOutcome::Completed)
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check --workspace --locked`
Expected: PASS

- [ ] **Step 4: Convert build_create_checkout_plan to use symbolic steps**

Replace the closure-based steps 1 and 2 in `build_create_checkout_plan` (lines 253-328). The function no longer needs `registry`, `providers_data`, `runner`, or `local_host` parameters — step data is self-contained and the resolver provides infrastructure. Simplify the signature:

```rust
fn build_create_checkout_plan(
    branch: String,
    create_branch: bool,
    intent: CheckoutIntent,
    issue_ids: Vec<(String, String)>,
) -> ExecutionPlan {
    let mut steps = Vec::new();

    steps.push(Step {
        description: format!("Create checkout for branch {branch}"),
        host: StepHost::Local,
        action: StepAction::CreateCheckout {
            branch: branch.clone(),
            create_branch,
            intent,
            issue_ids: issue_ids.clone(),
        },
    });

    if !issue_ids.is_empty() {
        steps.push(Step {
            description: "Link issues to branch".to_string(),
            host: StepHost::Local,
            action: StepAction::LinkIssuesToBranch {
                branch: branch.clone(),
                issue_ids,
            },
        });
    }

    steps.push(Step {
        description: "Create workspace".to_string(),
        host: StepHost::Local,
        action: StepAction::CreateWorkspaceForCheckout { label: branch },
    });

    ExecutionPlan::Steps(StepPlan::new(steps))
}
```

Update the call site in `build_plan` to pass fewer arguments.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace --locked`
Expected: PASS — existing checkout tests should produce identical results

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/
git commit -m "refactor: convert CreateCheckout plan to symbolic step actions"
```

---

### Task 8: Implement resolver functions for teleport steps

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs` (resolver match arms)
- Read: `crates/flotilla-core/src/executor/session_actions.rs` (TeleportFlow methods)

- [ ] **Step 1: Implement ResolveAttachCommand**

```rust
StepAction::ResolveAttachCommand { session_id } => {
    let teleport_flow = TeleportFlow::new(
        &self.repo.root,
        self.registry.as_ref(),
        self.providers_data.as_ref(),
        &self.config_base,
        &self.attachable_store,
        self.daemon_socket_path.as_deref(),
        &self.local_host,
        &session_id,
        None,  // branch not needed for attach resolution
        None,  // checkout_key not needed for attach resolution
    );
    let cmd = teleport_flow.resolve_attach_step().await?;
    Ok(StepOutcome::Produced(CommandValue::AttachCommandResolved { command: cmd }))
}
```

Check `TeleportFlow::new` and `resolve_attach_step` signatures in `session_actions.rs` to verify which parameters are actually used. The `branch` and `checkout_key` params may be needed — read the source and pass accordingly.

- [ ] **Step 2: Implement EnsureCheckoutForTeleport**

```rust
StepAction::EnsureCheckoutForTeleport { branch, checkout_key, initial_path } => {
    if let Some(path) = initial_path {
        return Ok(StepOutcome::Produced(CommandValue::CheckoutPathResolved { path }));
    }
    let teleport_flow = TeleportFlow::new(
        &self.repo.root,
        self.registry.as_ref(),
        self.providers_data.as_ref(),
        &self.config_base,
        &self.attachable_store,
        self.daemon_socket_path.as_deref(),
        &self.local_host,
        "",  // session_id not needed for checkout resolution
        branch.as_deref(),
        checkout_key.as_ref(),
    );
    match teleport_flow.ensure_checkout_step().await? {
        Some(path) => Ok(StepOutcome::Produced(CommandValue::CheckoutPathResolved { path })),
        None => Ok(StepOutcome::Skipped),
    }
}
```

Again, verify `TeleportFlow::new` signature — `session_id` may be required even if not used. Pass a reference to the real session_id if needed (add it to the variant fields).

- [ ] **Step 3: Implement CreateTeleportWorkspace**

```rust
StepAction::CreateTeleportWorkspace { session_id, branch } => {
    let cmd = prior.iter().find_map(|o| match o {
        StepOutcome::Produced(CommandValue::AttachCommandResolved { command }) => Some(command.clone()),
        _ => None,
    }).ok_or_else(|| "attach command not resolved by prior step".to_string())?;

    let path = prior.iter().find_map(|o| match o {
        StepOutcome::Produced(CommandValue::CheckoutPathResolved { path }) => Some(path.clone()),
        _ => None,
    }).ok_or_else(|| "checkout path not resolved by prior step".to_string())?;

    let teleport_flow = TeleportFlow::new(
        &self.repo.root,
        self.registry.as_ref(),
        self.providers_data.as_ref(),
        &self.config_base,
        &self.attachable_store,
        self.daemon_socket_path.as_deref(),
        &self.local_host,
        &session_id,
        branch.as_deref(),
        None,
    );
    teleport_flow.create_workspace_step(&path, &cmd).await?;
    Ok(StepOutcome::Completed)
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check --workspace --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat: implement resolver functions for teleport step actions"
```

---

### Task 9: Convert teleport plan to symbolic + remove Closure

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:337-497` (build_teleport_session_plan)
- Modify: `crates/flotilla-core/src/step.rs:30-36` (remove Closure variant)
- Modify: `crates/flotilla-core/src/step.rs:93-94` (remove Closure branch in run_step_plan)
- Modify: `crates/flotilla-core/src/step.rs:18-19` (remove StepFuture type alias)
- Modify: `crates/flotilla-core/src/step.rs` tests (remove Closure usage)

- [ ] **Step 1: Convert build_teleport_session_plan to symbolic**

Replace the entire function body. No more Arc slots, no more cloning 8+ values per closure. The function simplifies dramatically:

```rust
async fn build_teleport_session_plan(
    session_id: String,
    branch: Option<String>,
    checkout_key: Option<PathBuf>,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    config_base: PathBuf,
    attachable_store: SharedAttachableStore,
    daemon_socket_path: Option<PathBuf>,
    local_host: flotilla_protocol::HostName,
) -> ExecutionPlan {
    // Pre-resolve initial checkout path (needs providers_data at build time)
    let teleport_flow = TeleportFlow::new(
        &repo_root,
        registry.as_ref(),
        providers_data.as_ref(),
        &config_base,
        &attachable_store,
        daemon_socket_path.as_deref(),
        &local_host,
        &session_id,
        branch.as_deref(),
        checkout_key.as_ref(),
    );
    let initial_path = match teleport_flow.initial_checkout_path().await {
        Ok(path) => path,
        Err(message) => return ExecutionPlan::Immediate(CommandValue::Error { message }),
    };

    let steps = vec![
        Step {
            description: format!("Resolve attach command for session {session_id}"),
            host: StepHost::Local,
            action: StepAction::ResolveAttachCommand { session_id: session_id.clone() },
        },
        Step {
            description: "Ensure checkout for teleport".to_string(),
            host: StepHost::Local,
            action: StepAction::EnsureCheckoutForTeleport {
                branch: branch.clone(),
                checkout_key,
                initial_path,
            },
        },
        Step {
            description: "Create workspace with teleport command".to_string(),
            host: StepHost::Local,
            action: StepAction::CreateTeleportWorkspace {
                session_id,
                branch,
            },
        },
    ];

    ExecutionPlan::Steps(StepPlan::new(steps))
}
```

Note: the function still needs `registry`, `providers_data`, etc. for the `initial_checkout_path` pre-resolution. This is the batch 1 concession noted in the spec. The function signature can be simplified further when that resolution moves into the resolver.

- [ ] **Step 2: Run tests to verify teleport still works**

Run: `cargo test --workspace --locked`
Expected: PASS — teleport-related tests in executor/tests.rs should produce identical results

- [ ] **Step 3: Convert remaining plan builders**

Convert `build_remove_checkout_plan`, `build_archive_session_plan`, `build_generate_branch_name_plan` to use symbolic steps. Implement resolver arms for:

```rust
StepAction::RemoveCheckout { branch, terminal_keys, deleted_checkout_paths } => {
    let checkout_service = CheckoutService::new(self.registry.as_ref(), self.runner.as_ref());
    checkout_service.remove_checkout(&self.repo.root, &branch, &terminal_keys, &deleted_checkout_paths, &self.attachable_store).await?;
    Ok(StepOutcome::Completed)
}

StepAction::ArchiveSession { session_id } => {
    let session_actions = ReadOnlySessionActionService::new(self.registry.as_ref(), self.providers_data.as_ref());
    match session_actions.archive_session_result(&session_id).await {
        CommandValue::Error { message } => Err(message),
        result => Ok(StepOutcome::CompletedWith(result)),
    }
}

StepAction::GenerateBranchName { issue_keys } => {
    let session_actions = ReadOnlySessionActionService::new(self.registry.as_ref(), self.providers_data.as_ref());
    Ok(StepOutcome::CompletedWith(session_actions.generate_branch_name_result(&issue_keys).await))
}
```

Note: `build_archive_session_plan` and `build_generate_branch_name_plan` currently return `ExecutionPlan::Immediate` when `should_run_*_as_step()` is false. Keep this behavior — the guard logic stays in `build_plan`, returning `Immediate` for the non-step path.

- [ ] **Step 4: Verify no closures remain in plan builders**

Run: `rg 'StepAction::Closure' crates/flotilla-core/src/executor.rs`
Expected: no matches

- [ ] **Step 5: Remove StepAction::Closure variant**

In `crates/flotilla-core/src/step.rs`:
1. Remove the `Closure(Box<dyn FnOnce(Vec<StepOutcome>) -> StepFuture + Send>)` variant
2. Remove the `StepFuture` type alias (line 19)
3. Remove the `Closure` branch from `run_step_plan` (line 93-94) — all steps now go through the resolver
4. Remove the `Closure(_) => unreachable!()` arm from the resolver
5. Remove `Future` and `Pin` from the imports (line 1)

The step runner becomes:
```rust
let outcome = resolver.resolve(&step.description, step.action, &outcomes).await;
```

- [ ] **Step 6: Update step.rs tests**

The `make_step` helper and several tests use `StepAction::Closure`. Convert them to use a test resolver that returns predetermined outcomes:

```rust
struct TestResolver {
    outcomes: Vec<Option<Result<StepOutcome, String>>>,
}

#[async_trait::async_trait]
impl StepResolver for TestResolver {
    async fn resolve(&self, _desc: &str, action: StepAction, _prior: &[StepOutcome]) -> Result<StepOutcome, String> {
        match action {
            StepAction::ArchiveSession { session_id } => {
                let idx: usize = session_id.parse().expect("test session_id should be index");
                self.outcomes[idx].clone().expect("step called twice")
            }
            _ => panic!("unexpected action in test"),
        }
    }
}
```

Or use a simpler pattern with a `Vec<Result<StepOutcome, String>>` + `AtomicUsize` counter. The key point: tests need a way to return predetermined outcomes without closures.

The recommended approach: add a `StepAction::Test { index: usize }` variant behind `#[cfg(test)]` to keep test construction simple. The resolver matches `Test { index }` and returns `outcomes[index]`. This replaces the `make_step` helper cleanly and avoids the throwaway `TestResolver` from Task 6.

- [ ] **Step 7: Run full CI suite**

Run:
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
Expected: all PASS

- [ ] **Step 8: Commit**

```bash
git add crates/
git commit -m "refactor: eliminate StepAction::Closure, all steps now symbolic"
```

---

### Task 10: Clean up and final verification

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs` (remove unused imports, dead code)

- [ ] **Step 1: Remove dead imports**

The executor no longer needs `std::sync::Arc` for the teleport plan slots, nor `tokio::sync::Mutex`. Remove imports that are now unused. Run `cargo clippy --workspace --all-targets --locked -- -D warnings` to find them.

- [ ] **Step 2: Simplify build_teleport_session_plan signature if possible**

Now that step closures don't capture `Arc<ProviderRegistry>` etc., check if the function signature can drop parameters that are only used for the `initial_checkout_path` pre-resolution. If the pre-resolution is the only thing keeping those parameters, leave as-is with a comment for batch 2.

- [ ] **Step 3: Run full CI suite**

Run:
```bash
cargo +nightly-2026-03-12 fmt
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
Expected: all PASS

- [ ] **Step 4: Commit**

```bash
git add crates/
git commit -m "chore: clean up dead imports after closure elimination"
```

- [ ] **Step 5: Verify diff summary**

Run `git log --oneline main..HEAD` to see the commit chain. Verify it tells a clear story.
