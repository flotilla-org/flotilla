# All-Symbolic Step Execution — Batch 2: Eliminate Immediate

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert all remaining `execute()` handlers to symbolic step actions, remove `ExecutionPlan::Immediate` and the `execute()` function — leaving one uniform execution model where every command becomes a step plan.

**Architecture:** Each `CommandAction` variant handled by `execute()` becomes a new `StepAction` variant with a resolver match arm. `build_plan()` returns `ExecutionPlan::Steps(StepPlan)` for every action. Once no code produces `ExecutionPlan::Immediate`, the variant and `execute()` are deleted. The two existing `should_run_*_as_step()` guards in `build_archive_session_plan` and `build_generate_branch_name_plan` are also removed — all commands uniformly become step plans.

**Tech Stack:** Rust, async-trait, tokio, flotilla-protocol (serde), flotilla-core (executor, step)

**Spec:** `docs/superpowers/specs/2026-03-21-all-symbolic-step-execution-design.md` (Batch 2 section)

**CI commands:**
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

---

## File Structure

All changes happen within existing files. No new files are created.

| File | Changes |
|------|---------|
| `crates/flotilla-core/src/step.rs` | Add 9 new `StepAction` variants |
| `crates/flotilla-core/src/executor.rs` | Add 9 resolver match arms, convert `build_plan()` match arms, remove `execute()`, remove `ExecutionPlan::Immediate`, remove `RemoveCheckoutFlow`, remove `should_run_*_as_step()` guards |
| `crates/flotilla-core/src/executor/session_actions.rs` | Remove `should_run_archive_as_step()`, `should_run_generate_branch_name_as_step()`, `TeleportFlow::execute()` |
| `crates/flotilla-core/src/in_process.rs` | Simplify `ExecutionPlan` match to always run step plan |
| `crates/flotilla-core/src/executor/tests.rs` | Migrate tests from `run_execute()`/`execute()` to `run_build_plan_to_completion()`, remove `run_execute` helper, remove paired characterization tests (both paths collapse to one) |

---

### Task 1: Add symbolic StepAction variants for the 9 remaining actions

**Files:**
- Modify: `crates/flotilla-core/src/step.rs:32-83` (StepAction enum)

- [ ] **Step 1: Add the new variants to StepAction**

In `crates/flotilla-core/src/step.rs`, add these variants to the `StepAction` enum, grouped by domain below the existing variants:

```rust
    // Workspace lifecycle (new)
    CreateWorkspaceFromPreparedTerminal {
        target_host: HostName,
        branch: String,
        checkout_path: PathBuf,
        attachable_set_id: Option<flotilla_protocol::AttachableSetId>,
        commands: Vec<flotilla_protocol::PreparedTerminalCommand>,
    },
    SelectWorkspace {
        ws_ref: String,
    },
    PrepareTerminalForCheckout {
        checkout_path: PathBuf,
        commands: Vec<flotilla_protocol::PreparedTerminalCommand>,
    },

    // Checkout with AlwaysCreate/Inline policy (forwarded-command path)
    CheckoutImmediate {
        target: flotilla_protocol::CheckoutTarget,
        issue_ids: Vec<(String, String)>,
    },

    // Query
    FetchCheckoutStatus {
        branch: String,
        checkout_path: Option<PathBuf>,
        change_request_id: Option<String>,
    },

    // External interactions
    OpenChangeRequest { id: String },
    CloseChangeRequest { id: String },
    OpenIssue { id: String },
    LinkIssuesToChangeRequest {
        change_request_id: String,
        issue_ids: Vec<String>,
    },
```

Add necessary imports to `step.rs`: `HostName` is already imported. Add `AttachableSetId`, `CheckoutTarget`, and `PreparedTerminalCommand` from `flotilla_protocol`.

- [ ] **Step 2: Add `todo!()` arms to the resolver to make it compile**

In `crates/flotilla-core/src/executor.rs`, add placeholder `todo!("task N")` match arms in `ExecutorStepResolver::resolve()` for each new variant.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check --workspace --locked`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/step.rs crates/flotilla-core/src/executor.rs
git commit -m "feat: add StepAction variants for all remaining immediate commands"
```

---

### Task 2: Implement resolver arms for simple external-interaction commands

These are the simplest — they call one provider method and return `Ok`/`Error`.

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:368-487` (resolver impl)

- [ ] **Step 1: Implement OpenChangeRequest resolver arm**

Replace the `todo!()` placeholder:

```rust
StepAction::OpenChangeRequest { id } => {
    debug!(%id, "opening change request in browser");
    if let Some(cr) = self.registry.change_requests.preferred() {
        let _ = cr.open_in_browser(&self.repo.root, &id).await;
    }
    Ok(StepOutcome::Completed)
}
```

- [ ] **Step 2: Implement CloseChangeRequest resolver arm**

```rust
StepAction::CloseChangeRequest { id } => {
    debug!(%id, "closing change request");
    if let Some(cr) = self.registry.change_requests.preferred() {
        let _ = cr.close_change_request(&self.repo.root, &id).await;
    }
    Ok(StepOutcome::Completed)
}
```

- [ ] **Step 3: Implement OpenIssue resolver arm**

```rust
StepAction::OpenIssue { id } => {
    debug!(%id, "opening issue in browser");
    if let Some(it) = self.registry.issue_trackers.preferred() {
        let _ = it.open_in_browser(&self.repo.root, &id).await;
    }
    Ok(StepOutcome::Completed)
}
```

- [ ] **Step 4: Implement LinkIssuesToChangeRequest resolver arm**

Extract the logic from `execute()`'s `LinkIssuesToChangeRequest` arm (lines 682-710):

```rust
StepAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
    info!(issue_ids = ?issue_ids, %change_request_id, "linking issues to change request");
    let body_result = run!(
        self.runner.as_ref(),
        "gh",
        &["pr", "view", &change_request_id, "--json", "body", "--jq", ".body"],
        &self.repo.root,
    );
    match body_result {
        Ok(current_body) => {
            let fixes_lines: Vec<String> = issue_ids.iter().map(|id| format!("Fixes #{id}")).collect();
            let new_body = if current_body.trim().is_empty() {
                fixes_lines.join("\n")
            } else {
                format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
            };
            let result = run!(
                self.runner.as_ref(),
                "gh",
                &["pr", "edit", &change_request_id, "--body", &new_body],
                &self.repo.root,
            );
            match result {
                Ok(_) => {
                    info!(%change_request_id, "linked issues to change request");
                    Ok(StepOutcome::Completed)
                }
                Err(e) => {
                    error!(err = %e, "failed to edit change request");
                    Err(e)
                }
            }
        }
        Err(e) => {
            error!(err = %e, "failed to read change request body");
            Err(e)
        }
    }
}
```

Note: the `run!` macro requires `use crate::providers::run;` — verify this is already imported in `executor.rs`.

- [ ] **Step 5: Verify it compiles**

Run: `cargo check --workspace --locked`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat: implement resolver arms for external-interaction step actions"
```

---

### Task 3: Implement resolver arms for workspace and terminal commands

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:368-487` (resolver impl)

- [ ] **Step 1: Implement SelectWorkspace resolver arm**

```rust
StepAction::SelectWorkspace { ws_ref } => {
    info!(%ws_ref, "switching to workspace");
    let workspace_orchestrator = WorkspaceOrchestrator::new(
        &self.repo.root,
        self.registry.as_ref(),
        &self.config_base,
        &self.attachable_store,
        self.daemon_socket_path.as_deref(),
        &self.local_host,
    );
    workspace_orchestrator.select_workspace(&ws_ref).await?;
    Ok(StepOutcome::Completed)
}
```

- [ ] **Step 2: Implement CreateWorkspaceFromPreparedTerminal resolver arm**

```rust
StepAction::CreateWorkspaceFromPreparedTerminal {
    target_host,
    branch,
    checkout_path,
    attachable_set_id,
    commands,
} => {
    let workspace_orchestrator = WorkspaceOrchestrator::new(
        &self.repo.root,
        self.registry.as_ref(),
        &self.config_base,
        &self.attachable_store,
        self.daemon_socket_path.as_deref(),
        &self.local_host,
    );
    workspace_orchestrator
        .create_workspace_from_prepared_terminal(
            &target_host,
            &branch,
            &checkout_path,
            attachable_set_id.as_ref(),
            &commands,
        )
        .await?;
    Ok(StepOutcome::Completed)
}
```

- [ ] **Step 3: Implement PrepareTerminalForCheckout resolver arm**

This one accesses `providers_data` for the checkout lookup and returns `CompletedWith(TerminalPrepared)`:

```rust
StepAction::PrepareTerminalForCheckout { checkout_path, commands: requested_commands } => {
    let host_key = HostPath::new(self.local_host.clone(), checkout_path.clone());
    if let Some(co) = self.providers_data.checkouts.get(&host_key).cloned() {
        let workspace_orchestrator = WorkspaceOrchestrator::new(
            &self.repo.root,
            self.registry.as_ref(),
            &self.config_base,
            &self.attachable_store,
            self.daemon_socket_path.as_deref(),
            &self.local_host,
        );
        let attachable_set_id =
            workspace_orchestrator.ensure_attachable_set_for_checkout(&self.local_host, &checkout_path);
        let terminal_preparation = TerminalPreparationService::new(
            self.registry.as_ref(),
            &self.config_base,
            &self.attachable_store,
            self.daemon_socket_path.as_deref(),
        );
        let commands = terminal_preparation
            .prepare_terminal_commands(&co.branch, &checkout_path, &requested_commands, || {
                workspace_config(&self.repo.root, &co.branch, &checkout_path, "claude", &self.config_base)
            })
            .await?;
        Ok(StepOutcome::CompletedWith(CommandValue::TerminalPrepared {
            repo_identity: self.repo.identity.clone(),
            target_host: self.local_host.clone(),
            branch: co.branch,
            checkout_path,
            attachable_set_id,
            commands,
        }))
    } else {
        Err(format!("checkout not found: {}", checkout_path.display()))
    }
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check --workspace --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat: implement resolver arms for workspace and terminal step actions"
```

---

### Task 4: Implement resolver arms for checkout and query commands

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:368-487` (resolver impl)

- [ ] **Step 1: Implement CheckoutImmediate resolver arm**

This is the forwarded-command path where the remote daemon always creates and links issues inline:

```rust
StepAction::CheckoutImmediate { target, issue_ids } => {
    let (branch, create_branch, intent) = match target {
        CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
        CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
    };
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
    info!(%branch, "creating checkout (immediate)");
    let result = checkout_flow
        .checkout_created_result(CheckoutExistingPolicy::AlwaysCreate, CheckoutIssueLinkPolicy::Inline)
        .await?;
    if let CommandValue::CheckoutCreated { path, .. } = &result {
        info!(checkout_path = %path.display(), "created checkout");
    }
    Ok(StepOutcome::CompletedWith(result))
}
```

- [ ] **Step 2: Implement FetchCheckoutStatus resolver arm**

```rust
StepAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => {
    let info = data::fetch_checkout_status(
        &branch,
        checkout_path.as_deref(),
        change_request_id.as_deref(),
        &self.repo.root,
        self.runner.as_ref(),
    )
    .await;
    Ok(StepOutcome::CompletedWith(CommandValue::CheckoutStatus(info)))
}
```

Add `use crate::data;` to the imports in `executor.rs` if not already present.

- [ ] **Step 3: Implement CreateWorkspaceForCheckout resolver arm for the standalone command**

The existing `CreateWorkspaceForCheckout` resolver arm reads the checkout path from *prior* step outcomes — it serves the step-plan path where `CreateCheckout` runs first. The standalone `CommandAction::CreateWorkspaceForCheckout` carries its own `checkout_path`. Rather than adding a second variant, reuse the existing `StepAction::CreateWorkspaceForCheckout` variant, but update the resolver arm to also accept a `checkout_path` field. Change the variant to:

```rust
CreateWorkspaceForCheckout {
    label: String,
    checkout_path: Option<PathBuf>,
},
```

When `checkout_path` is `Some`, the resolver uses it directly. When `None`, it reads from prior outcomes (existing behavior). Update the existing plan builders (`build_create_checkout_plan`) to pass `checkout_path: None`. The standalone command path passes `checkout_path: Some(path)`.

Update the resolver arm:

```rust
StepAction::CreateWorkspaceForCheckout { label, checkout_path } => {
    let path = checkout_path.or_else(|| {
        prior.iter().find_map(|o| match o {
            StepOutcome::CompletedWith(CommandValue::CheckoutCreated { path, .. }) => Some(path.clone()),
            _ => None,
        })
    });
    match path {
        Some(p) => {
            let host_key = HostPath::new(self.local_host.clone(), p.clone());
            if !self.providers_data.checkouts.contains_key(&host_key) {
                return Err(format!("checkout not found: {}", p.display()));
            }
            info!(%label, "entering workspace");
            let workspace_orchestrator = WorkspaceOrchestrator::new(
                &self.repo.root,
                self.registry.as_ref(),
                &self.config_base,
                &self.attachable_store,
                self.daemon_socket_path.as_deref(),
                &self.local_host,
            );
            workspace_orchestrator.create_workspace_for_checkout(&p, &label).await
        }
        None => Ok(StepOutcome::Skipped),
    }
}
```

Wait — the existing resolver arm for `CreateWorkspaceForCheckout` in the step-plan path does NOT check `providers_data.checkouts` because the checkout was just created by a prior step and isn't in `providers_data` yet. The standalone command path DOES check it because the checkout already exists.

Better approach: add the `checkout_path` field but only run the `contains_key` check when the path came from the field, not from prior outcomes:

```rust
StepAction::CreateWorkspaceForCheckout { label, checkout_path: explicit_path } => {
    let path = if let Some(p) = explicit_path {
        let host_key = HostPath::new(self.local_host.clone(), p.clone());
        if !self.providers_data.checkouts.contains_key(&host_key) {
            return Err(format!("checkout not found: {}", p.display()));
        }
        info!(%label, "entering workspace");
        Some(p)
    } else {
        prior.iter().find_map(|o| match o {
            StepOutcome::CompletedWith(CommandValue::CheckoutCreated { path, .. }) => Some(path.clone()),
            _ => None,
        })
    };
    match path {
        Some(p) => {
            let workspace_orchestrator = WorkspaceOrchestrator::new(
                &self.repo.root,
                self.registry.as_ref(),
                &self.config_base,
                &self.attachable_store,
                self.daemon_socket_path.as_deref(),
                &self.local_host,
            );
            workspace_orchestrator.create_workspace_for_checkout(&p, &label).await
        }
        None => Ok(StepOutcome::Skipped),
    }
}
```

- [ ] **Step 4: Update build_create_checkout_plan to pass checkout_path: None**

In `build_create_checkout_plan`, update the `CreateWorkspaceForCheckout` step:

```rust
action: StepAction::CreateWorkspaceForCheckout { label: branch, checkout_path: None },
```

- [ ] **Step 5: Run tests to verify nothing broke**

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/
git commit -m "feat: implement resolver arms for checkout-immediate, fetch-status, and standalone workspace"
```

---

### Task 5: Convert all build_plan arms to return StepPlan

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:149-239` (build_plan function)
- Modify: `crates/flotilla-core/src/executor.rs:490-524` (build_archive_session_plan, build_generate_branch_name_plan)
- Modify: `crates/flotilla-core/src/executor/session_actions.rs:42-49,71-73` (remove should_run_* methods)

- [ ] **Step 1: Convert the catchall arm in build_plan**

Replace the catchall that delegates to `execute()`:

```rust
action => {
    let result = execute(
        action, &repo, &registry, &providers_data, &*runner,
        &config_base, &attachable_store, daemon_socket_path.as_deref(), &local_host,
    ).await;
    ExecutionPlan::Immediate(result)
}
```

With individual arms that each build a single-step plan:

```rust
CommandAction::CreateWorkspaceForCheckout { checkout_path, label } => ExecutionPlan::Steps(StepPlan::new(vec![Step {
    description: format!("Create workspace for {label}"),
    host: StepHost::Local,
    action: StepAction::CreateWorkspaceForCheckout { label, checkout_path: Some(checkout_path) },
}])),

CommandAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands } => {
    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: format!("Create workspace from prepared terminal for {branch}"),
        host: StepHost::Local,
        action: StepAction::CreateWorkspaceFromPreparedTerminal {
            target_host,
            branch,
            checkout_path,
            attachable_set_id,
            commands,
        },
    }]))
}

CommandAction::SelectWorkspace { ws_ref } => ExecutionPlan::Steps(StepPlan::new(vec![Step {
    description: format!("Select workspace {ws_ref}"),
    host: StepHost::Local,
    action: StepAction::SelectWorkspace { ws_ref },
}])),

CommandAction::PrepareTerminalForCheckout { checkout_path, commands } => {
    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: "Prepare terminal for checkout".to_string(),
        host: StepHost::Local,
        action: StepAction::PrepareTerminalForCheckout { checkout_path, commands },
    }]))
}

CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => {
    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: format!("Fetch checkout status for {branch}"),
        host: StepHost::Local,
        action: StepAction::FetchCheckoutStatus { branch, checkout_path, change_request_id },
    }]))
}

CommandAction::OpenChangeRequest { id } => ExecutionPlan::Steps(StepPlan::new(vec![Step {
    description: format!("Open change request {id}"),
    host: StepHost::Local,
    action: StepAction::OpenChangeRequest { id },
}])),

CommandAction::CloseChangeRequest { id } => ExecutionPlan::Steps(StepPlan::new(vec![Step {
    description: format!("Close change request {id}"),
    host: StepHost::Local,
    action: StepAction::CloseChangeRequest { id },
}])),

CommandAction::OpenIssue { id } => ExecutionPlan::Steps(StepPlan::new(vec![Step {
    description: format!("Open issue {id}"),
    host: StepHost::Local,
    action: StepAction::OpenIssue { id },
}])),

CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: format!("Link issues to change request {change_request_id}"),
        host: StepHost::Local,
        action: StepAction::LinkIssuesToChangeRequest { change_request_id, issue_ids },
    }]))
}

// Daemon-level commands should not reach build_plan.
CommandAction::TrackRepoPath { .. }
| CommandAction::UntrackRepo { .. }
| CommandAction::Refresh { .. }
| CommandAction::SetIssueViewport { .. }
| CommandAction::FetchMoreIssues { .. }
| CommandAction::SearchIssues { .. }
| CommandAction::ClearIssueSearch { .. } => {
    ExecutionPlan::Immediate(CommandValue::Error {
        message: "bug: daemon-level command reached per-repo executor".to_string(),
    })
}
```

The catchall `action =>` only fires for actions NOT already matched by earlier arms. `CommandAction::Checkout`, `TeleportSession`, `RemoveCheckout`, `ArchiveSession`, and `GenerateBranchName` are handled by their own arms above. The catchall sees only these 9 actions plus daemon-level commands.

Note: `CheckoutImmediate` is added as a `StepAction` variant and resolver arm (Tasks 1, 4) but is not wired into any `build_plan` arm in this batch. It exists for future use when the forwarded-command path needs `AlwaysCreate/Inline` checkout policy (distinct from the interactive `CreateCheckout` which uses `ReuseKnownCheckout/Deferred`). The `AlwaysCreate/Inline` behavior from the old `execute()` function is intentionally dropped — it was only reachable via direct `execute()` calls, not via `build_plan`, and those callers don't exist after this work.

- [ ] **Step 2: Remove the should_run_*_as_step guards**

In `build_archive_session_plan` (executor.rs lines 490-506), remove the early-return `Immediate` path:

```rust
async fn build_archive_session_plan(session_id: String) -> ExecutionPlan {
    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: format!("Archive session {session_id}"),
        host: StepHost::Local,
        action: StepAction::ArchiveSession { session_id },
    }]))
}
```

The function no longer needs `registry` or `providers_data` parameters.

Similarly, `build_generate_branch_name_plan` (lines 508-524):

```rust
fn build_generate_branch_name_plan(issue_keys: Vec<String>) -> ExecutionPlan {
    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: "Generate branch name".to_string(),
        host: StepHost::Local,
        action: StepAction::GenerateBranchName { issue_keys },
    }]))
}
```

Update the call sites in `build_plan` to pass fewer arguments.

- [ ] **Step 3: Remove should_run_* methods from session_actions.rs**

In `crates/flotilla-core/src/executor/session_actions.rs`, remove:
- `should_run_archive_as_step()` (lines 42-49)
- `should_run_generate_branch_name_as_step()` (lines 71-73)

- [ ] **Step 4: Verify it compiles**

Run: `cargo check --workspace --locked`
Expected: PASS (with `execute()` now unused — clippy may warn)

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/
git commit -m "refactor: convert all build_plan arms to return StepPlan"
```

---

### Task 6: Remove execute(), ExecutionPlan::Immediate, and RemoveCheckoutFlow

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs` (remove `execute()` fn, `ExecutionPlan::Immediate`, `RemoveCheckoutFlow`)
- Modify: `crates/flotilla-core/src/in_process.rs:2256-2266` (remove Immediate branch)

- [ ] **Step 1: Remove the execute() function**

Delete the entire `execute()` function (lines 525-754). It should now have no callers.

- [ ] **Step 2: Remove ExecutionPlan::Immediate**

Change the `ExecutionPlan` enum to only have `Steps`:

Actually, there's a subtlety: the daemon-level commands still return `ExecutionPlan::Immediate(Error)` and `build_teleport_session_plan` returns `Immediate(Error)` on initial_checkout_path failure. Rather than keeping `Immediate` for error-only cases, convert these to single-step error returns or handle them differently:

For `build_teleport_session_plan`'s error path (line 314), return a zero-step plan that the caller interprets as an error. But that's not how `run_step_plan` works — an empty plan returns `CommandValue::Ok`. Better: keep `ExecutionPlan::Immediate` for error-only returns for now, OR convert the error into a step plan with a failing step.

Simplest approach: replace `ExecutionPlan` with just `StepPlan`. Where errors occur during plan building, return a plan with zero steps and use a different mechanism... No, that loses the error message.

Better approach: change `build_plan` to return `StepPlan` directly. For errors during plan building, create a wrapper that produces a plan with a single error-yielding step. Add a `StepAction::Fail { message: String }` variant:

Actually, the cleanest approach: `build_plan` returns `Result<StepPlan, CommandValue>`. Errors during plan building return the `Err` variant. The caller (`in_process.rs`) handles `Err` by emitting `CommandFinished` immediately. This eliminates `ExecutionPlan` entirely.

Do this in two sub-steps:

**Sub-step A:** Change `build_plan` return type to `Result<StepPlan, CommandValue>`. Replace all `ExecutionPlan::Steps(plan)` with `Ok(plan)` and all `ExecutionPlan::Immediate(value)` with `Err(value)`. This is a mechanical transformation.

**Sub-step B:** Update `in_process.rs` to match on `Result`:

```rust
match plan {
    Err(result) => {
        refresh_trigger.notify_one();
        let _ = event_tx.send(DaemonEvent::CommandFinished {
            command_id: id,
            host: command_host.clone(),
            repo_identity: repo_identity.clone(),
            repo: repo_path,
            result,
        });
    }
    Ok(step_plan) => {
        // ... existing Steps handling, unchanged ...
    }
}
```

- [ ] **Step 3: Remove the ExecutionPlan enum**

Delete the `ExecutionPlan` enum definition (lines 33-39). The function now returns `Result<StepPlan, CommandValue>` directly.

- [ ] **Step 4: Remove RemoveCheckoutFlow**

Delete the `RemoveCheckoutFlow` struct and its `impl` block (lines 105-147). Its logic was migrated to the `RemoveCheckout` resolver arm in batch 1. The only remaining caller was `build_plan`'s `RemoveCheckout` arm, which still uses it for `resolve_branch()` and `deleted_checkout_paths()` during plan building. Inline those calls:

In the `CommandAction::RemoveCheckout` arm of `build_plan`:

```rust
CommandAction::RemoveCheckout { checkout, terminal_keys } => {
    match resolve_checkout_branch(&checkout, &providers_data, &local_host) {
        Ok(branch) => {
            let deleted_paths: Vec<HostPath> = providers_data
                .checkouts
                .iter()
                .filter(|(hp, co)| co.branch == branch && hp.host == local_host)
                .map(|(hp, _)| hp.clone())
                .collect();
            Ok(build_remove_checkout_plan(branch, terminal_keys, deleted_paths))
        }
        Err(message) => Err(CommandValue::Error { message }),
    }
}
```

And change `build_remove_checkout_plan` to return `StepPlan` instead of `ExecutionPlan`:

```rust
fn build_remove_checkout_plan(
    branch: String,
    terminal_keys: Vec<ManagedTerminalId>,
    deleted_checkout_paths: Vec<HostPath>,
) -> StepPlan {
    StepPlan::new(vec![Step {
        description: format!("Remove checkout for branch {branch}"),
        host: StepHost::Local,
        action: StepAction::RemoveCheckout { branch, terminal_keys, deleted_checkout_paths },
    }])
}
```

- [ ] **Step 5: Remove TeleportFlow::execute()**

In `crates/flotilla-core/src/executor/session_actions.rs`, delete the `TeleportFlow::execute()` method (lines 238-243). It was the old monolithic teleport path that is no longer called.

- [ ] **Step 6: Remove unused imports**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Fix any unused import warnings. In particular:
- `executor.rs` may no longer need `error` from tracing (check)
- `executor.rs` may have unused `CheckoutExistingPolicy`, `CheckoutIssueLinkPolicy` if only the resolver uses `CheckoutFlow` now. Actually, these are still needed by the resolver — verify.
- `session_actions.rs` may have unused methods on `TeleportFlow`/`TeleportSessionActionService` after `execute()` is removed

- [ ] **Step 7: Verify it compiles and passes**

Run:
```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
Expected: Some test failures — tests that reference `ExecutionPlan::Immediate` or call `execute()` will fail. These are fixed in Task 7.

- [ ] **Step 8: Do NOT commit yet — proceed to Task 7 to fix tests first**

Tests that reference `ExecutionPlan::Immediate` or call `execute()` will fail. These are fixed in Task 7, then Tasks 6+7 are committed together.

---

### Task 7: Migrate tests to step-plan-only execution

**Files:**
- Modify: `crates/flotilla-core/src/executor/tests.rs`

The test file has two execution paths:
1. `run_execute()` — calls `execute()` directly (now deleted)
2. `run_build_plan_to_completion()` — calls `build_plan()` then runs the step plan

All tests using `run_execute()` need to migrate to `run_build_plan_to_completion()`. The paired characterization tests (that verify both paths return the same result) collapse to a single test.

- [ ] **Step 1: Update run_build_plan_to_completion to handle Result**

`build_plan` now returns `Result<StepPlan, CommandValue>`. Update the helper:

```rust
async fn run_build_plan_to_completion(
    action: CommandAction,
    registry: ProviderRegistry,
    providers_data: ProviderData,
    runner: MockRunner,
) -> CommandValue {
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::step::run_step_plan;

    let config_base = config_base();
    let attachable_store = test_attachable_store(&config_base);
    let local_host = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };
    let registry = Arc::new(registry);
    let providers_data = Arc::new(providers_data);
    let runner: Arc<dyn CommandRunner> = Arc::new(runner);

    let plan = build_plan(
        local_command(action),
        repo.clone(),
        Arc::clone(&registry),
        Arc::clone(&providers_data),
        Arc::clone(&runner),
        config_base.clone(),
        attachable_store.clone(),
        None,
        local_host.clone(),
        None,
    )
    .await;

    match plan {
        Err(result) => result,
        Ok(step_plan) => {
            let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
            let resolver = ExecutorStepResolver {
                repo,
                registry,
                providers_data,
                runner,
                config_base,
                attachable_store,
                daemon_socket_path: None,
                local_host: local_host.clone(),
            };
            run_step_plan(step_plan, 1, local_host, repo_identity(), repo_root(), cancel, tx, &resolver).await
        }
    }
}
```

- [ ] **Step 2: Remove run_execute helper and the execute import**

Delete the `run_execute` function and remove `execute` from the imports at the top of the test file.

- [ ] **Step 3: Migrate simple tests from run_execute to run_build_plan_to_completion**

For each test that calls `run_execute(action, &registry, &data, &runner)`, change to `run_build_plan_to_completion(action, registry, data, runner).await`. Note the ownership change: `run_build_plan_to_completion` takes owned values, so remove borrows.

Tests to migrate (all use `run_execute`):

Workspace:
- `create_workspace_for_checkout_not_found`
- `create_workspace_for_checkout_success_without_ws_manager`
- `create_workspace_for_checkout_success_with_ws_manager`
- `create_workspace_for_checkout_ws_manager_fails`
- `create_workspace_for_checkout_selects_existing_workspace`
- `select_workspace_no_manager`
- `select_workspace_success`
- `select_workspace_failure`

Terminal:
- `prepare_terminal_for_checkout_returns_terminal_commands`

Checkout:
- `create_checkout_no_manager`
- `create_checkout_success`
- `create_checkout_with_issue_ids_writes_git_config`
- `create_checkout_failure`
- `create_checkout_success_ws_manager_fails_still_returns_created`
- `checkout_action_does_not_create_workspace`
- `remove_checkout_no_manager`
- `remove_checkout_success`
- `remove_checkout_failure`
- `remove_checkout_kills_correlated_terminals`
- `fetch_checkout_status_returns_checkout_status`
- `fetch_checkout_status_populates_uncommitted_files`

External:
- `open_change_request_no_provider`
- `open_change_request_with_provider`
- `close_change_request_no_provider`
- `close_change_request_with_provider`
- `open_issue_no_provider`
- `open_issue_with_provider`
- `link_issues_success_with_existing_body`
- `link_issues_success_with_empty_body`
- `link_issues_view_fails`
- `link_issues_edit_fails`

Session:
- `archive_session_uses_provider_from_session_ref`
- `archive_session_not_found`
- `archive_session_no_agent_provider`
- `archive_session_success`
- `archive_session_agent_fails`
- `generate_branch_name_ai_success`
- `generate_branch_name_ai_failure_uses_fallback`
- `generate_branch_name_no_ai_provider_uses_fallback`
- `generate_branch_name_multiple_issues`
- `generate_branch_name_unknown_issue_key`

Teleport:
- `teleport_session_with_checkout_key`
- `teleport_session_uses_provider_specific_attach_command`
- `teleport_session_with_branch_creates_checkout`
- `teleport_session_no_path_no_branch`
- `teleport_session_ws_manager_fails`
- `teleport_session_uses_session_as_name_when_no_branch`
- `teleport_session_creates_workspace_even_when_one_exists`

Daemon:
- `daemon_level_commands_return_error`

- [ ] **Step 4: Migrate tests that call execute() directly with custom attachable stores**

These tests need a `run_build_plan_to_completion`-like helper that accepts a custom config_base and attachable_store. Create a more flexible variant:

```rust
async fn run_build_plan_to_completion_with(
    action: CommandAction,
    repo: RepoExecutionContext,
    registry: ProviderRegistry,
    providers_data: ProviderData,
    runner: MockRunner,
    config_base: &Path,
    attachable_store: SharedAttachableStore,
) -> CommandValue {
    // Same pattern as run_build_plan_to_completion but with injected config_base/attachable_store
    ...
}
```

Migrate:
- `create_workspace_for_checkout_persists_workspace_binding`
- `prepare_terminal_for_checkout_includes_attachable_set_id_when_present`
- `prepare_terminal_for_checkout_creates_and_persists_attachable_set`
- `create_workspace_from_prepared_terminal_wraps_remote_commands_in_ssh`
- `create_workspace_from_prepared_terminal_prefixes_name_with_host`
- `create_workspace_from_prepared_terminal_persists_remote_attachable_set_binding`
- `create_workspace_from_prepared_terminal_uses_local_fallback_for_remote_only_repo`
- `teleport_session_persists_workspace_binding`
- `remove_checkout_cascades_attachable_set_deletion`
- `remove_checkout_kills_correlated_terminals` (both the `run_execute` half and the paired characterization test)

- [ ] **Step 5: Remove paired characterization tests**

Delete these tests that existed solely to verify parity between `execute()` and `build_plan()` paths — there's now only one path:
- `checkout_create_plan_and_execute_return_same_checkout_created_result`
- `remove_checkout_plan_and_execute_both_kill_correlated_terminals`
- `teleport_plan_and_execute_both_create_new_workspace_even_when_one_exists`

The behavior is already covered by the migrated single-path tests.

- [ ] **Step 6: Update run_build_plan helper**

`run_build_plan` currently returns `ExecutionPlan`. Change it to return `Result<StepPlan, CommandValue>` to match the new `build_plan` signature.

Update tests that use `run_build_plan` and match on `ExecutionPlan::Steps`/`ExecutionPlan::Immediate` to match on `Ok(step_plan)`/`Err(value)`:
- `build_plan_create_checkout_returns_steps`
- `build_plan_create_checkout_skips_existing`
- `checkout_plan_includes_workspace_step`
- `checkout_plan_end_to_end_creates_workspace`
- `checkout_plan_creates_workspace_for_preexisting_checkout`
- `checkout_plan_preserves_checkout_created_when_workspace_step_fails`
- `build_plan_teleport_session_returns_steps`
- `build_plan_remove_checkout_returns_steps`
- `build_plan_archive_session_returns_steps`
- `build_plan_generate_branch_name_returns_steps`
- `build_plan_archive_session_missing_session_returns_immediate_error` → With the `should_run_archive_as_step` guard removed, a missing session is no longer detected at plan-build time — it goes through the step plan and the resolver returns an error at execution time. Convert to `run_build_plan_to_completion` and assert `CommandValue::Error { .. }`.
- `build_plan_generate_branch_name_without_ai_returns_immediate_fallback` → With the `should_run_generate_branch_name_as_step` guard removed, ALL branch-name requests go through step plans. Convert to `run_build_plan_to_completion` and assert `CommandValue::BranchNameGenerated { .. }`.
- `build_plan_simple_command_returns_immediate` → Now returns `Ok(StepPlan)`. Convert to `run_build_plan_to_completion` and assert the result is `CommandValue::Ok`.

- [ ] **Step 7: Run full CI suite**

Run:
```bash
cargo +nightly-2026-03-12 fmt
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
Expected: all PASS

- [ ] **Step 8: Commit Tasks 6+7 together**

```bash
git add crates/flotilla-core/
git commit -m "refactor: remove execute() and ExecutionPlan::Immediate, migrate all tests to step plans"
```

---

### Task 8: Clean up and final verification

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs` (dead imports, dead code)
- Modify: `crates/flotilla-core/src/executor/session_actions.rs` (dead code)

- [ ] **Step 1: Remove dead code**

Run `cargo clippy --workspace --all-targets --locked -- -D warnings` and fix all warnings. Expected dead items:
- Unused methods on `TeleportFlow` (`initial_checkout_path`, `resolve_attach_step`, `create_workspace_step`) — verify if they're still called from `build_teleport_session_plan`. `initial_checkout_path` is still called. `resolve_attach_step` and `create_workspace_step` are only called from the deleted `execute()` path — remove them if unused.
- Unused `TeleportFlow` struct entirely if only `TeleportSessionActionService` is used by the resolver
- The `RemoveCheckoutFlow` was deleted in Task 6 — verify no dangling references

- [ ] **Step 2: Verify build_plan no longer needs all parameters for simple commands**

`build_plan` still takes `registry`, `providers_data`, `runner`, etc. because the teleport plan builder and remove-checkout plan builder need them for pre-resolution. These parameters are still needed — no simplification possible yet. Add a comment noting this for future work (remote routing may move pre-resolution into the resolver).

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
git commit -m "chore: clean up dead code after eliminating execute()"
```

- [ ] **Step 5: Verify diff summary**

Run `git log --oneline main..HEAD` to see the commit chain. Verify it tells a clear story.
