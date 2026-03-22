# Terminal Manager Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract terminal identity management from providers into a TerminalManager, replacing ManagedTerminalId with AttachableId as the sole terminal identity.

**Architecture:** A new `TerminalManager` in `flotilla-core` owns the `SharedAttachableStore` for terminal concerns and wraps a simplified `TerminalPool` trait. Providers become pure CLI adapters. AttachableId doubles as the session name passed to cleat/shpool.

**Tech Stack:** Rust, async-trait, tokio, uuid (for AttachableId allocation)

**Spec:** `docs/superpowers/specs/2026-03-22-terminal-manager-design.md`

---

### Task 1: Create TerminalManager foundation (additive)

All new code — no existing files broken. Introduces the new types, the simplified pool trait (named `SessionPool` temporarily to avoid collision with existing `TerminalPool`), and a `TerminalManager` with tested core operations.

**Files:**
- Create: `crates/flotilla-core/src/terminal_manager.rs`
- Modify: `crates/flotilla-core/src/lib.rs` (add `pub mod terminal_manager;`)
- Modify: `crates/flotilla-core/src/providers/terminal/mod.rs` (add `TerminalSession` type)

- [ ] **Step 1: Add `TerminalSession` type**

In `crates/flotilla-core/src/providers/terminal/mod.rs`, add below the existing `TerminalPool` trait (which stays untouched for now):

```rust
/// Raw session data returned by a terminal pool CLI adapter.
/// No AttachableId — the manager handles identity mapping.
#[derive(Debug, Clone)]
pub struct TerminalSession {
    pub session_name: String,
    pub status: TerminalStatus,
    pub command: Option<String>,
    pub working_directory: Option<PathBuf>,
}
```

Add the necessary imports (`TerminalStatus` from `flotilla_protocol`, `PathBuf` from `std::path`).

- [ ] **Step 2: Define `SessionPool` trait**

In the same file, add the simplified trait below `TerminalSession`:

```rust
/// Simplified terminal pool trait — pure CLI adapter.
/// Session names are opaque strings (AttachableIds in practice).
/// No store, no identity management.
#[async_trait]
pub trait SessionPool: Send + Sync {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String>;
    async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path) -> Result<(), String>;
    async fn attach_command(
        &self,
        session_name: &str,
        command: &str,
        cwd: &Path,
        env_vars: &TerminalEnvVars,
    ) -> Result<String, String>;
    async fn kill_session(&self, session_name: &str) -> Result<(), String>;
}
```

- [ ] **Step 3: Create `terminal_manager.rs` with struct and constructor**

Create `crates/flotilla-core/src/terminal_manager.rs`:

```rust
use std::{path::Path, sync::Arc};

use flotilla_protocol::{AttachableId, AttachableSetId, HostName, HostPath, TerminalStatus};

use crate::{
    attachable::{
        AttachableContent, AttachableStoreApi, SharedAttachableStore, TerminalAttachable,
        TerminalPurpose,
    },
    providers::terminal::{SessionPool, TerminalEnvVars, TerminalSession},
};

/// Manages terminal identity and lifecycle.
///
/// Owns the mapping between AttachableIds (stable identity) and terminal pool
/// sessions. Providers are pure CLI adapters; the manager handles allocation,
/// reconciliation, and store persistence.
pub struct TerminalManager {
    pool: Arc<dyn SessionPool>,
    store: SharedAttachableStore,
}
```

Add `pub mod terminal_manager;` to `crates/flotilla-core/src/lib.rs`.

- [ ] **Step 4: Implement `allocate_set`**

```rust
impl TerminalManager {
    pub fn new(pool: Arc<dyn SessionPool>, store: SharedAttachableStore) -> Self {
        Self { pool, store }
    }

    /// Create a new AttachableSet for a checkout. Always allocates a fresh set.
    pub fn allocate_set(&self, host: HostName, checkout_path: &Path) -> Result<AttachableSetId, String> {
        let mut store = self.store.lock().map_err(|e| format!("store lock: {e}"))?;
        let set_id = store.allocate_set_id();
        let checkout = HostPath::new(host.clone(), checkout_path.to_path_buf());
        store.insert_set(flotilla_protocol::AttachableSet {
            id: set_id.clone(),
            host_affinity: Some(host),
            checkout: Some(checkout),
            template_identity: None,
            members: Vec::new(),
        });
        store.save()?;
        Ok(set_id)
    }
}
```

- [ ] **Step 5: Implement `allocate_terminal`**

```rust
    /// Create a new terminal Attachable within a set.
    /// The AttachableId is used directly as the session name when talking to the pool.
    pub fn allocate_terminal(
        &self,
        set_id: &AttachableSetId,
        role: &str,
        index: u32,
        checkout: &str,
        command: &str,
        working_directory: &Path,
    ) -> Result<AttachableId, String> {
        let mut store = self.store.lock().map_err(|e| format!("store lock: {e}"))?;
        let attachable_id = store.allocate_attachable_id();
        store.insert_attachable(flotilla_protocol::Attachable {
            id: attachable_id.clone(),
            set_id: set_id.clone(),
            content: AttachableContent::Terminal(TerminalAttachable {
                purpose: TerminalPurpose {
                    checkout: checkout.to_string(),
                    role: role.to_string(),
                    index,
                },
                command: command.to_string(),
                working_directory: working_directory.to_path_buf(),
                status: TerminalStatus::Disconnected,
            }),
        });
        store.save()?;
        Ok(attachable_id)
    }
```

- [ ] **Step 6: Implement `ensure_running`**

```rust
    /// Ensure a terminal session is running. Reads command/cwd from the stored attachable.
    pub async fn ensure_running(&self, attachable_id: &AttachableId) -> Result<(), String> {
        let (command, cwd) = {
            let store = self.store.lock().map_err(|e| format!("store lock: {e}"))?;
            let attachable = store
                .registry()
                .attachables
                .get(attachable_id)
                .ok_or_else(|| format!("unknown attachable: {attachable_id}"))?;
            match &attachable.content {
                AttachableContent::Terminal(t) => (t.command.clone(), t.working_directory.clone()),
            }
        };
        self.pool
            .ensure_session(&attachable_id.to_string(), &command, &cwd)
            .await
    }
```

- [ ] **Step 7: Implement `attach_command`**

```rust
    /// Build the attach command for a terminal, including env var injection.
    pub async fn attach_command(
        &self,
        attachable_id: &AttachableId,
        daemon_socket_path: Option<&Path>,
    ) -> Result<String, String> {
        let (command, cwd) = {
            let store = self.store.lock().map_err(|e| format!("store lock: {e}"))?;
            let attachable = store
                .registry()
                .attachables
                .get(attachable_id)
                .ok_or_else(|| format!("unknown attachable: {attachable_id}"))?;
            match &attachable.content {
                AttachableContent::Terminal(t) => (t.command.clone(), t.working_directory.clone()),
            }
        };
        let mut env_vars: TerminalEnvVars = vec![
            ("FLOTILLA_ATTACHABLE_ID".to_string(), attachable_id.to_string()),
        ];
        if let Some(socket) = daemon_socket_path {
            env_vars.push(("FLOTILLA_DAEMON_SOCKET".to_string(), socket.display().to_string()));
        }
        self.pool
            .attach_command(&attachable_id.to_string(), &command, &cwd, &env_vars)
            .await
    }
```

- [ ] **Step 8: Implement `kill_terminal`**

```rust
    pub async fn kill_terminal(&self, attachable_id: &AttachableId) -> Result<(), String> {
        self.pool.kill_session(&attachable_id.to_string()).await
    }
```

- [ ] **Step 9: Add `TerminalInfo` return type**

Define this before `refresh()` which returns it:

```rust
/// Terminal information returned by the manager, enriched with identity.
#[derive(Debug, Clone)]
pub struct TerminalInfo {
    pub attachable_id: AttachableId,
    pub attachable_set_id: AttachableSetId,
    pub role: String,
    pub checkout: String,
    pub index: u32,
    pub command: String,
    pub working_directory: std::path::PathBuf,
    pub status: TerminalStatus,
}
```

- [ ] **Step 10: Add `update_terminal_status` to `AttachableStoreApi`**

The existing store API has no direct way to update an attachable's status in place. Add a method to `AttachableStoreApi` in `crates/flotilla-core/src/attachable/store.rs`:

```rust
fn update_terminal_status(&mut self, id: &AttachableId, status: TerminalStatus) -> bool;
```

Implement it in both `AttachableStore` and `InMemoryAttachableStore`: look up the attachable by ID, if it's a `Terminal` variant and the status differs, update it and return `true`. Return `false` if unchanged or not found.

- [ ] **Step 11: Implement `refresh`**

This is the reconciliation logic currently duplicated in cleat/shpool, centralized here. Returns terminal info enriched with AttachableIds.

```rust
    /// Reconcile live sessions against stored attachables.
    /// Updates statuses, returns terminal info for each known attachable.
    pub async fn refresh(&self) -> Result<Vec<TerminalInfo>, String> {
        let live_sessions = self.pool.list_sessions().await?;
        let live_by_name: std::collections::HashMap<&str, &TerminalSession> = live_sessions
            .iter()
            .map(|s| (s.session_name.as_str(), s))
            .collect();

        let mut store = self.store.lock().map_err(|e| format!("store lock: {e}"))?;
        let mut terminals = Vec::new();
        let mut changed = false;

        // Collect attachable data first to avoid borrow issues with the store
        let terminal_entries: Vec<_> = store
            .registry()
            .attachables
            .iter()
            .filter_map(|(id, attachable)| match &attachable.content {
                AttachableContent::Terminal(t) => Some((
                    id.clone(),
                    attachable.set_id.clone(),
                    t.purpose.clone(),
                    t.command.clone(),
                    t.working_directory.clone(),
                )),
            })
            .collect();

        for (id, set_id, purpose, command, working_directory) in terminal_entries {
            let session_name = id.to_string();
            let (status, live_command, live_cwd) = match live_by_name.get(session_name.as_str()) {
                Some(session) => (
                    session.status.clone(),
                    session.command.clone().unwrap_or(command.clone()),
                    session.working_directory.clone().unwrap_or(working_directory.clone()),
                ),
                None => (TerminalStatus::Disconnected, command.clone(), working_directory.clone()),
            };

            changed |= store.update_terminal_status(&id, status.clone());

            terminals.push(TerminalInfo {
                attachable_id: id,
                attachable_set_id: set_id,
                role: purpose.role,
                checkout: purpose.checkout,
                index: purpose.index,
                command: live_command,
                working_directory: live_cwd,
                status,
            });
        }

        if changed {
            let _ = store.save();
        }

        Ok(terminals)
    }
```

- [ ] **Step 12: Implement `cascade_delete`**

```rust
    /// Remove all attachable sets for the given checkout paths and kill their sessions.
    pub async fn cascade_delete(&self, checkout_paths: &[HostPath]) -> Result<(), String> {
        let session_names = {
            let mut store = self.store.lock().map_err(|e| format!("store lock: {e}"))?;
            let mut names = Vec::new();
            let mut any_removed = false;
            for checkout_path in checkout_paths {
                let set_ids = store.sets_for_checkout(checkout_path);
                for set_id in set_ids {
                    // Collect attachable IDs (= session names) before removing
                    for (id, attachable) in store.registry().attachables.iter() {
                        if attachable.set_id == set_id {
                            names.push(id.to_string());
                        }
                    }
                    if store.remove_set(&set_id).is_some() {
                        any_removed = true;
                    }
                }
            }
            if any_removed {
                let _ = store.save();
            }
            names
        };
        // Best-effort kill of sessions
        for name in &session_names {
            if let Err(err) = self.pool.kill_session(name).await {
                tracing::debug!(session = %name, err = %err, "best-effort session kill failed");
            }
        }
        Ok(())
    }
```

- [ ] **Step 13: Write tests for TerminalManager**

Add a `#[cfg(test)] mod tests` block in `terminal_manager.rs`. Create a `MockSessionPool` that implements `SessionPool` with canned responses. Use `shared_in_memory_attachable_store()` for the store.

Test cases:
- `allocate_set_creates_store_entry` — verify set appears in registry
- `allocate_terminal_creates_attachable` — verify attachable with correct TerminalPurpose
- `ensure_running_delegates_to_pool` — verify pool receives attachable_id as session name
- `attach_command_includes_env_vars` — verify FLOTILLA_ATTACHABLE_ID is injected
- `kill_terminal_delegates_to_pool` — verify pool receives correct session name
- `refresh_updates_statuses` — pool reports session as Running, verify returned TerminalInfo reflects it
- `refresh_reports_disconnected_for_missing_sessions` — pool returns empty, known attachable reported as Disconnected
- `cascade_delete_removes_sets_and_kills_sessions` — verify store emptied and pool.kill_session called

- [ ] **Step 14: Run tests and verify**

Run: `cargo test -p flotilla-core terminal_manager`
Expected: All tests pass.

- [ ] **Step 15: Run full workspace tests**

Run: `cargo test --workspace --locked`
Expected: All existing tests still pass (this task was purely additive).

- [ ] **Step 16: Commit**

```bash
git add -A && git commit -m "feat: create TerminalManager with SessionPool trait and tests"
```

---

### Task 2: Implement SessionPool for all providers

Add `SessionPool` implementations to cleat, shpool, and passthrough. These are simpler versions of the existing `TerminalPool` impls — no store, no reconciliation. The old `TerminalPool` impls remain for now (callers still use them).

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/cleat.rs`
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`
- Modify: `crates/flotilla-core/src/providers/terminal/passthrough.rs`

- [ ] **Step 1: Implement `SessionPool` for `CleatTerminalPool`**

Add a new `impl SessionPool for CleatTerminalPool` block. The implementation is much simpler than the current `TerminalPool` impl:

- `list_sessions`: Run `cleat list --json`, parse output into `Vec<TerminalSession>`. No store interaction, no reconciliation. Just map each JSON session to `TerminalSession { session_name: session.id, status, command, working_directory }`.
- `ensure_session`: Run `cleat create --json <session_name> --cwd <cwd> --cmd <command>`. No store.
- `attach_command`: Build the `cleat attach <session_name> --cwd <cwd> --cmd <wrapped_command>` string. Same shell wrapping logic as current impl but with `session_name` parameter instead of looking up from store.
- `kill_session`: Run `cleat kill <session_name>`. No store cleanup.

- [ ] **Step 2: Add tests for cleat `SessionPool`**

Reuse the existing `MockRunner` test helper. Write tests:
- `session_pool_list_sessions_parses_json` — verify TerminalSession fields from JSON
- `session_pool_ensure_creates_session` — verify CLI args
- `session_pool_attach_wraps_command` — verify attach command string
- `session_pool_kill_calls_cli` — verify kill CLI args

- [ ] **Step 3: Implement `SessionPool` for `ShpoolTerminalPool`**

Similar simplification:

- `list_sessions`: Run `shpool list --json`, parse output. Return `Vec<TerminalSession>` with `session_name` as the shpool session name. No store, no orphan detection, no binding lookup. Note: shpool doesn't report command/cwd, so those fields are `None`.
- `ensure_session`: No-op (shpool creates on attach), same as current.
- `attach_command`: Build the `shpool attach` command string using `session_name` parameter. Same shell wrapping as current impl.
- `kill_session`: Run `shpool kill <session_name>`.

Note: The `ShpoolTerminalPool` struct still needs `runner`, `socket_path`, `config_path` fields for CLI operations. It no longer needs `attachable_store`. However, since Task 3 will handle removing the field, for now add the new trait impl alongside the old one — the struct keeps `attachable_store` temporarily.

- [ ] **Step 4: Add tests for shpool `SessionPool`**

- `session_pool_list_sessions_parses_json` — verify parsing
- `session_pool_attach_builds_command` — verify command string
- `session_pool_kill_calls_cli` — verify kill args

- [ ] **Step 5: Implement `SessionPool` for `PassthroughTerminalPool`**

Trivial:

- `list_sessions`: Return `Ok(vec![])`.
- `ensure_session`: Return `Ok(())`.
- `attach_command`: Same as current — return command as-is, or prepend env vars.
- `kill_session`: Return `Ok(())`.

- [ ] **Step 6: Run tests**

Run: `cargo test --workspace --locked`
Expected: All tests pass (old and new).

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: implement SessionPool for cleat, shpool, and passthrough"
```

---

### Task 3: Wire TerminalManager into executor and refresh

Replace the scattered store logic in `executor/terminals.rs` and `refresh.rs` with TerminalManager. Remove the old `TerminalPool` trait. Update the Factory system. This is the big swap — many files change but the behavior is preserved.

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/mod.rs` (remove old `TerminalPool` trait, rename `SessionPool` → `TerminalPool`)
- Modify: `crates/flotilla-core/src/providers/terminal/cleat.rs` (remove old `TerminalPool` impl, remove `attachable_store` field, remove store helper methods)
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs` (remove old `TerminalPool` impl, remove `attachable_store` field, remove store helper methods)
- Modify: `crates/flotilla-core/src/providers/terminal/passthrough.rs` (remove old `TerminalPool` impl)
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs` (remove `attachable_store` from `Factory::probe` and `probe_all`)
- Modify: `crates/flotilla-core/src/providers/discovery/factories/*.rs` (all 11 factory files — remove `attachable_store` parameter)
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs` (update test helpers)
- Modify: `crates/flotilla-core/src/executor/terminals.rs` (rewrite to use TerminalManager)
- Modify: `crates/flotilla-core/src/executor/checkout.rs` (use TerminalManager for cascade delete)
- Modify: `crates/flotilla-core/src/executor.rs` (pass TerminalManager instead of store+pool separately)
- Modify: `crates/flotilla-core/src/refresh.rs` (use TerminalManager.refresh() instead of tp.list_terminals() + project_attachable_data terminal section)
- Modify: `crates/flotilla-core/src/model.rs` or wherever ProviderRegistry is assembled (TerminalManager construction)
- Modify: `crates/flotilla-core/src/executor/tests.rs` (update mock terminal pools)

- [ ] **Step 1: Remove old `TerminalPool` trait, rename `SessionPool` → `TerminalPool`**

In `crates/flotilla-core/src/providers/terminal/mod.rs`:
- Delete the old `TerminalPool` trait (the one with `ManagedTerminalId` parameters)
- Rename `SessionPool` to `TerminalPool`
- Update `TerminalSession` if needed

Steps 1–10 must be completed as a unit before compilation is possible — do not attempt intermediate `cargo check`.

- [ ] **Step 2: Strip store from cleat provider**

In `crates/flotilla-core/src/providers/terminal/cleat.rs`:
- Remove `attachable_store: SharedAttachableStore` from struct fields and `new()`
- Delete helper methods: `find_persisted_session_id`, `persist_attachable`, `reconcile_listed_session`, `disconnected_known_terminals`
- Remove the old `TerminalPool` impl block
- Update `impl TerminalPool for CleatTerminalPool` to use the renamed trait (was `SessionPool`)
- Remove all `attachable` imports
- Update constructor calls in tests

- [ ] **Step 3: Strip store from shpool provider**

In `crates/flotilla-core/src/providers/terminal/shpool.rs`:
- Remove `attachable_store: SharedAttachableStore` from struct fields, `create()`, and test `new()`
- Delete helper methods: `persist_expected_attachable`, `reconcile_known_attachable`, `disconnected_terminals_from_bindings`
- Remove the old `TerminalPool` impl block
- Update `impl TerminalPool for ShpoolTerminalPool` to use the renamed trait
- Remove orphan reaping logic from `list_sessions` (TerminalManager handles this)
- Remove all `attachable` imports

- [ ] **Step 4: Strip ManagedTerminalId from passthrough provider**

In `crates/flotilla-core/src/providers/terminal/passthrough.rs`:
- Remove old `TerminalPool` impl
- Update `impl TerminalPool for PassthroughTerminalPool` to use renamed trait
- Remove `ManagedTerminalId` import

- [ ] **Step 5: Remove `attachable_store` from Factory trait and all factories**

In `crates/flotilla-core/src/providers/discovery/mod.rs`:
- Remove `attachable_store: SharedAttachableStore` from `Factory::probe` signature
- Remove it from `probe_all` function signature
- Update the `TerminalPoolFactory` type alias if needed
- Update all call sites of `probe_all`

In each factory file under `crates/flotilla-core/src/providers/discovery/factories/`:
- `cleat.rs`: Remove `attachable_store` param, stop passing it to `CleatTerminalPool::new()`
- `shpool.rs`: Remove `attachable_store` param, stop passing it to `ShpoolTerminalPool::create()`
- `passthrough.rs`, `cmux.rs`, `git.rs`, `github.rs`, `claude.rs`, `codex.rs`, `cursor.rs`, `tmux.rs`, `zellij.rs`: Remove `_attachable_store` param. Note: some files contain multiple `Factory` impls that all need updating — `git.rs` has 3, `claude.rs` has 3, `cmux.rs` has 2, `github.rs` has 2.
- Update all test functions that call `probe()` to drop the argument

In `crates/flotilla-core/src/providers/discovery/test_support.rs`:
- Remove `test_attachable_store` helper if no longer needed
- Update `DiscoveryMockRunner` if it references attachable store

- [ ] **Step 6: Rewrite `executor/terminals.rs` to use TerminalManager**

The `TerminalPreparationService` should take a `&TerminalManager` instead of `&ProviderRegistry` + `&SharedAttachableStore`. Key changes:

- `resolve_workspace_commands`: Use `terminal_manager.allocate_set()` + `allocate_terminal()` for each template entry, then `attach_command()` for each allocated terminal.
- `prepare_terminal_commands`: Same — allocate terminals, get attach commands.
- Remove `build_terminal_env_vars` function entirely (moved into TerminalManager.attach_command).
- Remove `resolve_terminal_pool` function (replaced by TerminalManager operations).

The executor constructs a `TerminalManager` from the registry's preferred terminal pool + attachable store, and passes it to `TerminalPreparationService`.

- [ ] **Step 7: Update `executor/checkout.rs` to use TerminalManager**

- Remove `cascade_delete_attachable_sets` function
- The step resolver should use `terminal_manager.cascade_delete()` instead
- Update `RemoveCheckoutFlow` and step resolution to use TerminalManager

- [ ] **Step 8: Update `executor.rs` and step resolver**

- `ExecutorStepResolver` should hold a `TerminalManager` (or `Arc<TerminalManager>`) instead of separate pool + store references for terminal concerns
- Update `StepAction::RemoveCheckout` handling to use TerminalManager

- [ ] **Step 9: Update `refresh.rs` to use TerminalManager**

- Replace the terminal pool future (`tp_fut`) with `terminal_manager.refresh()`
- `TerminalManager::refresh()` returns `Vec<TerminalInfo>` — convert these to whatever `ProviderData` needs (likely `ManagedTerminal` still, for now — Task 4 removes this)
- Remove or simplify the terminal section of `project_attachable_data()` — the manager already handles enrichment
- Update refresh tests that create mock `TerminalPool` instances and test `project_attachable_data` behavior (set projection, orphan stripping, set matching). These tests need to use the new `TerminalPool` trait (formerly `SessionPool`) and may need a `TerminalManager` for setup.

- [ ] **Step 10: Update executor tests**

In `crates/flotilla-core/src/executor/tests.rs`:
- Replace mock `TerminalPool` implementations with mock `TerminalPool` (new trait)
- Remove imports of deleted functions (`build_terminal_env_vars`, `resolve_terminal_pool`)
- Update test setup to create `TerminalManager` with mock pool + in-memory store
- Ensure all executor tests pass

- [ ] **Step 11: Run full test suite**

Run: `cargo test --workspace --locked`
Expected: All tests pass.

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: No warnings.

- [ ] **Step 12: Commit**

```bash
git add -A && git commit -m "refactor: wire TerminalManager into executor and refresh, remove old TerminalPool trait"
```

---

### Task 4: Remove ManagedTerminalId from protocol and clean up

Remove `ManagedTerminalId` and `ManagedTerminal` from the protocol. Update all downstream: correlation, data, steps, TUI, snapshots, delta. Clean up the attachable module (remove terminal_session_binding_ref and related).

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs` (remove `ManagedTerminalId`, `ManagedTerminal`, update `ProviderData`)
- Modify: `crates/flotilla-protocol/src/commands.rs` (update `RemoveCheckout`)
- Modify: `crates/flotilla-protocol/src/snapshot.rs` (update `WorkItem`)
- Modify: `crates/flotilla-protocol/src/delta.rs` (update `Change` enum)
- Modify: `crates/flotilla-protocol/src/lib.rs` (remove re-exports)
- Modify: `crates/flotilla-core/src/data.rs` (update `CorrelatedWorkItem`, correlation logic)
- Modify: `crates/flotilla-core/src/step.rs` (update `StepAction::RemoveCheckout`)
- Modify: `crates/flotilla-core/src/providers/correlation.rs` (update `ItemKind`, `ProviderItemKey`)
- Modify: `crates/flotilla-core/src/refresh.rs` (update ProviderData terminal handling)
- Modify: `crates/flotilla-core/src/convert.rs` (update core→protocol conversion)
- Modify: `crates/flotilla-core/src/delta.rs` (update delta tracking)
- Modify: `crates/flotilla-core/src/attachable/mod.rs` (remove `terminal_session_binding_ref`, `parse_terminal_session_binding_ref`, `TERMINAL_SESSION_BINDING_PREFIX`)
- Modify: `crates/flotilla-tui/src/widgets/delete_confirm.rs` (update terminal key handling)
- Modify: `crates/flotilla-tui/src/widgets/preview_panel.rs` (update terminal display)
- Modify: `crates/flotilla-daemon/src/peer/merge.rs` (if it references ManagedTerminalId)
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` (update integration tests)

- [ ] **Step 1: Grep for all references to build the complete file list**

Run: `grep -r "ManagedTerminalId\|ManagedTerminal" --include="*.rs" crates/ src/`

This captures every file that needs updating. The file list in this task covers the known references, but the grep may reveal additional ones (e.g., `executor.rs`, `executor/terminals.rs`, intent.rs). Fix every reference found.

- [ ] **Step 2: Remove `ManagedTerminalId` and `ManagedTerminal` from protocol**

In `crates/flotilla-protocol/src/provider_data.rs`:
- Delete `ManagedTerminalId` struct and its `Display` impl
- Delete `ManagedTerminal` struct
- Change `ProviderData.managed_terminals: IndexMap<String, ManagedTerminal>` to use `AttachableId` as key and a simpler terminal type, or remove the field entirely if terminals are now accessed through the attachable registry

In `crates/flotilla-protocol/src/lib.rs`:
- Remove `ManagedTerminalId` and `ManagedTerminal` from re-exports

- [ ] **Step 3: Update commands**

In `crates/flotilla-protocol/src/commands.rs`:
- Change `RemoveCheckout.terminal_keys: Vec<ManagedTerminalId>` to `terminal_keys: Vec<AttachableId>`

- [ ] **Step 4: Update snapshot and delta**

In `crates/flotilla-protocol/src/snapshot.rs`:
- Change `WorkItem.terminal_keys: Vec<ManagedTerminalId>` to `terminal_keys: Vec<AttachableId>`

In `crates/flotilla-protocol/src/delta.rs`:
- Update `Change::ManagedTerminal` to use `AttachableId` as key, or rename to `Change::Terminal`

- [ ] **Step 5: Update correlation**

In `crates/flotilla-core/src/providers/correlation.rs`:
- Change `ItemKind::ManagedTerminal(String)` to use `AttachableId`
- Change `ProviderItemKey::ManagedTerminal(String)` to use `AttachableId`
- Update correlation key extraction — terminals now correlate via `AttachableSetId`'s checkout, not `ManagedTerminalId.checkout`

- [ ] **Step 6: Update `data.rs`**

In `crates/flotilla-core/src/data.rs`:
- Change `CorrelatedWorkItem.terminal_ids: Vec<ManagedTerminalId>` to `terminal_ids: Vec<AttachableId>`
- Update `group_to_work_item()` terminal collection logic
- Update `terminal_ids()` accessor

- [ ] **Step 7: Update `step.rs`**

In `crates/flotilla-core/src/step.rs`:
- Change `StepAction::RemoveCheckout.terminal_keys: Vec<ManagedTerminalId>` to `Vec<AttachableId>`

- [ ] **Step 8: Update TUI widgets**

In `crates/flotilla-tui/src/widgets/delete_confirm.rs`:
- Update terminal key display to use `AttachableId`
- Adjust formatting — AttachableId.to_string() will show the UUID, which is fine for now

In `crates/flotilla-tui/src/widgets/preview_panel.rs`:
- Update terminal status display
- Use role/command from TerminalInfo or attachable content instead of ManagedTerminalId fields

- [ ] **Step 9: Remove `terminal_session_binding_ref` and related**

In `crates/flotilla-core/src/attachable/mod.rs`:
- Delete `TERMINAL_SESSION_BINDING_PREFIX`
- Delete `terminal_session_binding_ref()`
- Delete `parse_terminal_session_binding_ref()`
- Delete the tests for these functions
- Remove from `pub use` in `mod.rs`

- [ ] **Step 10: Fix remaining references**

Re-run the grep from Step 1 to catch anything missed. Known files that may still have references:
- `crates/flotilla-core/src/executor.rs`
- `crates/flotilla-core/src/executor/terminals.rs`
- `crates/flotilla-core/src/convert.rs`
- `crates/flotilla-core/src/delta.rs`
- `crates/flotilla-daemon/src/peer/merge.rs`
- `crates/flotilla-tui/src/app/intent.rs`
- `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 11: Run full test suite**

Run: `cargo test --workspace --locked`
Expected: All tests pass.

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: No warnings.

Run: `cargo +nightly-2026-03-12 fmt --check`
Expected: No formatting issues.

- [ ] **Step 12: Commit**

```bash
git add -A && git commit -m "refactor: remove ManagedTerminalId, use AttachableId as sole terminal identity"
```

---

### Post-implementation notes

**Snapshot format**: This changes the wire format (`ManagedTerminalId` → `AttachableId` in snapshots and commands). Per CLAUDE.md, we are in a no-backwards-compatibility phase, so no migration logic is needed.

**Store API cleanup**: The `ensure_terminal_set_with_change` and `ensure_terminal_attachable_with_change` methods on `AttachableStoreApi` may now be unused by terminal code but are still used by `executor/workspace.rs` for workspace bindings. Don't remove them yet — they can be cleaned up in a separate pass if workspace management is refactored similarly.

**Terminal bindings in store**: After this change, terminal-related bindings are no longer written. Existing persisted registries may still contain them from before this change. Since we're in no-backwards-compat mode, this is fine — old data is simply ignored. If it causes issues, a one-time migration to strip terminal bindings can be added to store loading.
