# AttachableSet Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix attachable set repo-scoping, correlation stability, cascade delete on checkout removal, and orphan session reaping.

**Architecture:** Add removal methods to `AttachableStoreApi`, change projection from "referenced this cycle" to "checkout matches repo", wire cascade delete into the checkout-removal executor path, and add an orphan reaper to the shpool provider.

**Tech Stack:** Rust, tokio, flotilla-core (attachable store, executor, refresh, shpool provider)

---

### Task 1: Add removal methods to AttachableStoreApi

**Files:**
- Modify: `crates/flotilla-core/src/attachable/store.rs:30-72` (trait + `AttachableStoreState` + both impls)

- [ ] **Step 1: Write failing tests for `remove_set`**

Add to `crates/flotilla-core/src/attachable/store.rs` in the `#[cfg(test)] mod tests` block:

```rust
fn contract_remove_set_deletes_set_and_members_and_bindings(store: &mut impl AttachableStoreApi) {
    let host = HostName::new("desktop");
    let checkout = HostPath::new(host.clone(), "/repo/wt-feat");
    let set_id = store.ensure_terminal_set(Some(host), Some(checkout));

    let _shell = store.ensure_terminal_attachable(
        &set_id,
        "terminal_pool",
        "shpool",
        "flotilla/feat/shell/0",
        TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
        "bash",
        PathBuf::from("/repo/wt-feat"),
        TerminalStatus::Running,
    );
    let _agent = store.ensure_terminal_attachable(
        &set_id,
        "terminal_pool",
        "shpool",
        "flotilla/feat/agent/0",
        TerminalPurpose { checkout: "feat".into(), role: "agent".into(), index: 0 },
        "claude",
        PathBuf::from("/repo/wt-feat"),
        TerminalStatus::Running,
    );

    // Also add a workspace binding to the set
    store.replace_binding(ProviderBinding {
        provider_category: "workspace_manager".into(),
        provider_name: "cmux".into(),
        object_kind: BindingObjectKind::AttachableSet,
        object_id: set_id.to_string(),
        external_ref: "workspace:1".into(),
    });

    let removed = store.remove_set(&set_id);
    assert!(removed.is_some());
    let removed = removed.expect("should return removed set info");
    assert_eq!(removed.member_binding_refs.len(), 2);
    assert!(removed.member_binding_refs.contains(&"flotilla/feat/shell/0".to_string()));
    assert!(removed.member_binding_refs.contains(&"flotilla/feat/agent/0".to_string()));

    // Set, attachables, and all bindings should be gone
    assert!(store.registry().sets.is_empty());
    assert!(store.registry().attachables.is_empty());
    assert!(store.registry().bindings.is_empty());
}

fn contract_remove_set_returns_none_for_unknown_id(store: &mut impl AttachableStoreApi) {
    let unknown = AttachableSetId::new("nonexistent");
    assert!(store.remove_set(&unknown).is_none());
}
```

And the test functions that invoke the contracts for both implementations:

```rust
#[test]
fn file_backed_contract_remove_set_deletes_set_and_members_and_bindings() {
    contract_remove_set_deletes_set_and_members_and_bindings(&mut AttachableStore::with_base(
        tempfile::tempdir().expect("tempdir").path(),
    ));
}

#[test]
fn in_memory_contract_remove_set_deletes_set_and_members_and_bindings() {
    contract_remove_set_deletes_set_and_members_and_bindings(&mut InMemoryAttachableStore::new());
}

#[test]
fn file_backed_contract_remove_set_returns_none_for_unknown_id() {
    contract_remove_set_returns_none_for_unknown_id(&mut AttachableStore::with_base(
        tempfile::tempdir().expect("tempdir").path(),
    ));
}

#[test]
fn in_memory_contract_remove_set_returns_none_for_unknown_id() {
    contract_remove_set_returns_none_for_unknown_id(&mut InMemoryAttachableStore::new());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core --lib attachable::store::tests::file_backed_contract_remove_set -- --no-capture`
Expected: FAIL — `remove_set` method does not exist.

- [ ] **Step 3: Add `RemovedSetInfo` struct and `remove_set` to `AttachableStoreApi` trait**

In `crates/flotilla-core/src/attachable/store.rs`, add a return type struct near the top (after the `BindingKey` type alias):

```rust
/// Information returned when a set is removed, used for terminal teardown.
#[derive(Debug)]
pub struct RemovedSetInfo {
    /// Terminal pool binding external refs for the removed members (session names).
    pub member_binding_refs: Vec<String>,
}
```

Add to the `AttachableStoreApi` trait:

```rust
/// Remove a set, its member attachables, and all associated bindings.
/// Returns `None` if the set does not exist.
/// Returns terminal pool binding external refs for teardown.
fn remove_set(&mut self, id: &AttachableSetId) -> Option<RemovedSetInfo>;
```

- [ ] **Step 4: Implement `remove_set` on `AttachableStoreState`**

```rust
fn remove_set(&mut self, id: &AttachableSetId) -> Option<RemovedSetInfo> {
    let set = self.registry.sets.swap_remove(id)?;
    let mut member_binding_refs = Vec::new();

    for member_id in &set.members {
        self.registry.attachables.swap_remove(member_id);
        // Collect terminal-pool binding external refs before removing
        let member_id_str = member_id.to_string();
        for binding in &self.registry.bindings {
            if binding.object_id == member_id_str
                && binding.object_kind == BindingObjectKind::Attachable
                && binding.provider_category == "terminal_pool"
            {
                member_binding_refs.push(binding.external_ref.clone());
            }
        }
    }

    // Remove all bindings referencing the set or its members
    let set_id_str = id.to_string();
    let member_ids: std::collections::HashSet<String> = set.members.iter().map(|m| m.to_string()).collect();
    self.registry.bindings.retain(|b| b.object_id != set_id_str && !member_ids.contains(&b.object_id));

    // Rebuild the binding index
    self.binding_index = Self::build_binding_index(&self.registry);

    Some(RemovedSetInfo { member_binding_refs })
}
```

- [ ] **Step 5: Wire `remove_set` through both `AttachableStoreApi` implementations**

Add `remove_set` to the `impl AttachableStoreApi for AttachableStore` block (delegates to `self.state.remove_set(id)`), and the same for `impl AttachableStoreApi for InMemoryAttachableStore`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p flotilla-core --lib attachable::store::tests -- --no-capture`
Expected: All tests pass, including the new `remove_set` contract tests.

- [ ] **Step 7: Run full CI checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: Clean.

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-core/src/attachable/store.rs
git commit -m "feat: add remove_set to AttachableStoreApi for cascade delete"
```

---

### Task 2: Add `sets_for_checkout` query method

**Files:**
- Modify: `crates/flotilla-core/src/attachable/store.rs`

This is a simple query method needed by both the projection filter (Task 3) and the cascade delete (Task 4).

- [ ] **Step 1: Write failing test**

```rust
fn contract_sets_for_checkout_returns_matching_sets(store: &mut impl AttachableStoreApi) {
    let host = HostName::new("desktop");
    let checkout_a = HostPath::new(host.clone(), "/repo/wt-feat");
    let checkout_b = HostPath::new(host.clone(), "/repo/wt-main");

    let set_a = store.ensure_terminal_set(Some(host.clone()), Some(checkout_a.clone()));
    let _set_b = store.ensure_terminal_set(Some(host.clone()), Some(checkout_b.clone()));

    let found = store.sets_for_checkout(&checkout_a);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0], set_a);
}

fn contract_sets_for_checkout_returns_empty_for_unknown(store: &mut impl AttachableStoreApi) {
    let unknown = HostPath::new(HostName::new("desktop"), "/repo/nonexistent");
    assert!(store.sets_for_checkout(&unknown).is_empty());
}
```

Plus the test invocations for both implementations (same pattern as Task 1).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core --lib attachable::store::tests::file_backed_contract_sets_for_checkout -- --no-capture`
Expected: FAIL — method does not exist.

- [ ] **Step 3: Implement `sets_for_checkout` on `AttachableStoreState`, trait, and both impls**

Add to trait:
```rust
fn sets_for_checkout(&self, checkout: &HostPath) -> Vec<AttachableSetId>;
```

Implement on `AttachableStoreState`:
```rust
fn sets_for_checkout(&self, checkout: &HostPath) -> Vec<AttachableSetId> {
    self.registry
        .sets
        .iter()
        .filter(|(_, set)| set.checkout.as_ref() == Some(checkout))
        .map(|(id, _)| id.clone())
        .collect()
}
```

Wire through both `AttachableStore` and `InMemoryAttachableStore` implementations.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-core --lib attachable::store::tests -- --no-capture`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/attachable/store.rs
git commit -m "feat: add sets_for_checkout query to AttachableStoreApi"
```

---

### Task 3: Repo-scoped projection

**Files:**
- Modify: `crates/flotilla-core/src/refresh.rs:269-311` (`project_attachable_data`)

- [ ] **Step 1: Write failing test for repo-scoped projection**

Add a test in `crates/flotilla-core/src/refresh.rs` (or the appropriate test module — check if there's an existing test module for `project_attachable_data`). If there's no test module for refresh, add one in `crates/flotilla-core/src/refresh.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachable::{self, BindingObjectKind, TerminalPurpose};
    use crate::providers::discovery::ProviderRegistry;
    use flotilla_protocol::{
        HostName, HostPath, ManagedTerminal, ManagedTerminalId, TerminalStatus,
    };
    use std::path::PathBuf;

    fn empty_registry() -> ProviderRegistry {
        ProviderRegistry::default()
    }

    #[test]
    fn project_attachable_data_only_includes_sets_matching_repo_checkouts() {
        let store = attachable::shared_in_memory_attachable_store();
        let host = HostName::local();
        let checkout_a = HostPath::new(host.clone(), "/repo/wt-feat");
        let checkout_b = HostPath::new(host.clone(), "/repo/wt-other");

        // Create two sets in the store
        {
            let mut s = store.lock().expect("lock");
            s.ensure_terminal_set(Some(host.clone()), Some(checkout_a.clone()));
            s.ensure_terminal_set(Some(host.clone()), Some(checkout_b.clone()));
        }

        // ProviderData only has checkout_a
        let mut pd = ProviderData::default();
        pd.checkouts.insert(
            checkout_a.clone(),
            flotilla_protocol::Checkout {
                branch: "feat".into(),
                is_main: false,
                correlation_keys: vec![],
            },
        );

        let registry = empty_registry();
        project_attachable_data(&mut pd, &registry, &store);

        // Only set for checkout_a should be projected
        assert_eq!(pd.attachable_sets.len(), 1);
        let set = pd.attachable_sets.values().next().expect("one set");
        assert_eq!(set.checkout, Some(checkout_a));
    }

    #[test]
    fn project_attachable_data_set_appears_without_terminal_scan() {
        let store = attachable::shared_in_memory_attachable_store();
        let host = HostName::local();
        let checkout = HostPath::new(host.clone(), "/repo/wt-feat");

        {
            let mut s = store.lock().expect("lock");
            s.ensure_terminal_set(Some(host.clone()), Some(checkout.clone()));
        }

        // ProviderData has the checkout but NO managed_terminals
        let mut pd = ProviderData::default();
        pd.checkouts.insert(
            checkout.clone(),
            flotilla_protocol::Checkout {
                branch: "feat".into(),
                is_main: false,
                correlation_keys: vec![],
            },
        );

        let registry = empty_registry();
        project_attachable_data(&mut pd, &registry, &store);

        // Set should still be projected (stable, not scan-dependent)
        assert_eq!(pd.attachable_sets.len(), 1);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core --lib refresh::tests -- --no-capture`
Expected: The second test (`set_appears_without_terminal_scan`) should FAIL because the current logic only projects sets that are "referenced" by a terminal this cycle. The first test may also fail if sets for both checkouts get projected.

- [ ] **Step 3: Change `project_attachable_data` to use checkout-match filter**

Replace the set-selection logic in `project_attachable_data()` (around lines 277-310). Keep the enrichment step (populating `attachable_id` / `attachable_set_id` on terminals and workspaces), but change the set collection:

```rust
fn project_attachable_data(pd: &mut ProviderData, registry: &ProviderRegistry, attachable_store: &SharedAttachableStore) {
    let terminal_provider = registry.terminal_pools.preferred_with_desc().map(|(desc, _)| desc.implementation.clone());
    let workspace_provider = registry.workspace_managers.preferred_with_desc().map(|(desc, _)| desc.implementation.clone());
    let Ok(store) = attachable_store.lock() else {
        tracing::warn!("attachable store lock poisoned while projecting provider data");
        return;
    };

    // Enrichment: populate attachable_id / attachable_set_id on terminals and workspaces
    if let Some(provider_name) = terminal_provider.as_deref() {
        for terminal in pd.managed_terminals.values_mut() {
            let session_name = terminal_session_binding_ref(&terminal.id);
            let Some(attachable_id) = store.lookup_binding("terminal_pool", provider_name, BindingObjectKind::Attachable, &session_name)
            else {
                continue;
            };
            let attachable_id = flotilla_protocol::AttachableId::new(attachable_id.to_string());
            terminal.attachable_id = Some(attachable_id.clone());
            if let Some(attachable) = store.registry().attachables.get(&attachable_id) {
                terminal.attachable_set_id = Some(attachable.set_id.clone());
            }
        }
    }

    if let Some(provider_name) = workspace_provider.as_deref() {
        for (ws_ref, workspace) in &mut pd.workspaces {
            let Some(set_id) = store.lookup_binding("workspace_manager", provider_name, BindingObjectKind::AttachableSet, ws_ref.as_str())
            else {
                continue;
            };
            let set_id = flotilla_protocol::AttachableSetId::new(set_id.to_string());
            workspace.attachable_set_id = Some(set_id.clone());
        }
    }

    // Set selection: project sets whose checkout matches a repo checkout
    let checkout_paths: std::collections::HashSet<&flotilla_protocol::HostPath> = pd.checkouts.keys().collect();
    pd.attachable_sets = store
        .registry()
        .sets
        .iter()
        .filter(|(_, set)| set.checkout.as_ref().is_some_and(|co| checkout_paths.contains(co)))
        .map(|(id, set)| (id.clone(), set.clone()))
        .collect();
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-core --lib refresh::tests -- --no-capture`
Expected: Both tests pass.

- [ ] **Step 5: Fix existing test `project_attachable_data_populates_sets_and_ids`**

The existing test at `refresh.rs:811` creates a set with checkout `/tmp/wt-feat` but does not populate `pd.checkouts`. With the new checkout-match filter, this set will not be projected. Fix by adding the checkout to `pd`:

```rust
pd.checkouts.insert(
    flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), PathBuf::from("/tmp/wt-feat")),
    flotilla_protocol::Checkout { branch: "feat".into(), is_main: false, correlation_keys: vec![] },
);
```

- [ ] **Step 6: Run full CI checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: Clean.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/refresh.rs
git commit -m "feat: repo-scoped attachable set projection by checkout match"
```

---

### Task 4: Cascade delete on checkout removal

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:793-823` (immediate-mode `RemoveCheckout` handler)

- [ ] **Step 1: Write failing test for cascade delete**

Add to the existing `RemoveCheckout` test section in `crates/flotilla-core/src/executor.rs`:

```rust
#[tokio::test]
async fn remove_checkout_cascades_attachable_set_deletion() {
    let config_base = config_base();
    let attachable_store = test_attachable_store(&config_base);
    let host = HostName::local();
    let checkout_path = HostPath::new(host.clone(), "/repo/wt-feat-x");

    // Pre-populate the store with a set and members
    {
        let mut store = attachable_store.lock().expect("lock");
        let set_id = store.ensure_terminal_set(Some(host.clone()), Some(checkout_path.clone()));
        store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat-x/shell/0",
            TerminalPurpose { checkout: "feat-x".into(), role: "shell".into(), index: 0 },
            "bash",
            PathBuf::from("/repo/wt-feat-x"),
            TerminalStatus::Running,
        );
    }

    let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.terminal_pools.insert("shpool", desc("shpool"), Arc::clone(&mock_pool) as Arc<dyn TerminalPool>);
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));

    let repo = RepoExecutionContext {
        identity: repo_identity(),
        root: repo_root(),
    };
    let runner = runner_ok();
    let result = execute(
        remove_checkout_action("feat-x", vec![]),
        &repo,
        &registry,
        &data,
        &runner,
        &config_base,
        &attachable_store,
        None,
        &host,
    )
    .await;

    assert_checkout_removed_branch(result, "feat-x");

    // Verify set and members were removed from store
    let store = attachable_store.lock().expect("lock");
    assert!(store.registry().sets.is_empty(), "set should be removed");
    assert!(store.registry().attachables.is_empty(), "attachables should be removed");
    assert!(store.registry().bindings.is_empty(), "bindings should be removed");
}
```

Note: The existing `run_execute` helper creates its own attachable store, so for this test we need to call `execute` directly with our pre-populated store. Check if the test helper pattern allows this, and adapt accordingly.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core --lib executor::tests::remove_checkout_cascades -- --no-capture`
Expected: FAIL — the set is not removed (current code doesn't do cascade).

- [ ] **Step 3: Add cascade delete logic to `RemoveCheckout` handler**

In the `RemoveCheckout` handler in `execute()` (around line 805), after `remove_checkout` succeeds, add the cascade. The session refs from `RemovedSetInfo.member_binding_refs` are in the format `flotilla/{checkout}/{role}/{index}` — the same format that `terminal_session_binding_ref` produces and that `kill_terminal` expects via `terminal_session_binding_ref(&id)`. Use `ShpoolTerminalPool::parse_session_name` (already exists) to convert back to `ManagedTerminalId`. Extract it to a shared `pub fn` in the terminal module if it's currently private.

```rust
Some(Ok(())) => {
    // Cascade: remove attachable sets owned by this checkout
    let deleted_checkout_paths: Vec<HostPath> = providers_data
        .checkouts
        .iter()
        .filter(|(_, co)| co.branch == branch)
        .map(|(hp, _)| hp.clone())
        .collect();

    let mut all_session_refs = Vec::new();
    let mut any_removed = false;
    if let Ok(mut store) = attachable_store.lock() {
        for checkout_path in &deleted_checkout_paths {
            let set_ids = store.sets_for_checkout(checkout_path);
            for set_id in set_ids {
                if let Some(removed) = store.remove_set(&set_id) {
                    all_session_refs.extend(removed.member_binding_refs);
                    any_removed = true;
                }
            }
        }
        if any_removed {
            if let Err(e) = store.save() {
                warn!(err = %e, "failed to persist registry after cascade delete");
            }
        }
    }

    // Best-effort terminal teardown for cascade-removed sessions
    if let Some(tp) = registry.terminal_pools.preferred() {
        for session_ref in &all_session_refs {
            if let Some(terminal_id) = parse_session_ref_to_terminal_id(session_ref) {
                if let Err(e) = tp.kill_terminal(&terminal_id).await {
                    warn!(
                        session = %session_ref,
                        err = %e,
                        "failed to kill cascaded terminal session (best-effort)"
                    );
                }
            }
        }
        // Also kill explicitly-passed terminal keys (existing behavior)
        for terminal_id in &terminal_keys {
            if let Err(e) = tp.kill_terminal(terminal_id).await {
                warn!(
                    terminal = %terminal_id,
                    err = %e,
                    "failed to kill terminal session (best-effort)"
                );
            }
        }
    }
    CommandResult::CheckoutRemoved { branch }
}
```

Add a `parse_session_ref_to_terminal_id` helper using the same parsing logic as `ShpoolTerminalPool::parse_session_name` (which splits on `flotilla/` prefix and then splits `{checkout}/{role}/{index}` from the right). Place it in `crates/flotilla-core/src/providers/terminal/shpool.rs` as a `pub fn` (it's currently a private method) or in the terminal module root.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-core --lib executor::tests::remove_checkout_cascades -- --no-capture`
Expected: PASS.

- [ ] **Step 5: Run full CI checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: Clean.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat: cascade attachable set delete on checkout removal"
```

---

### Task 5: Orphan session reaper in shpool provider

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`

- [ ] **Step 1: Write failing test for orphan reaping**

Add to `crates/flotilla-core/src/providers/terminal/shpool.rs` tests:

```rust
#[tokio::test]
async fn list_terminals_reaps_orphan_sessions() {
    // Shpool reports a session that has no binding in the store
    let json = r#"{
        "sessions": [
            {
                "name": "flotilla/my-feature/shell/0",
                "started_at_unix_ms": 1709900000000,
                "status": "Attached"
            },
            {
                "name": "flotilla/orphan/agent/0",
                "started_at_unix_ms": 1709900001000,
                "status": "Attached"
            }
        ]
    }"#;
    let kill_json = r#"{"sessions": []}"#;
    let runner = Arc::new(MockRunner::new(vec![
        Ok(json.into()),       // list
        Ok(kill_json.into()),  // kill for orphan
    ]));
    let (pool, store, _dir) = test_pool(runner);

    // Only create a binding for my-feature/shell/0, not orphan/agent/0
    let shell_id = ManagedTerminalId { checkout: "my-feature".into(), role: "shell".into(), index: 0 };
    pool.attach_command(&shell_id, "bash", Path::new("/repo"), &vec![]).await.expect("seed binding");

    let terminals = pool.list_terminals().await.expect("list terminals");

    // Only the bound terminal should be returned
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].id.checkout, "my-feature");

    // Verify the orphan kill was issued: MockRunner should have consumed both responses
    // (list + kill). If runner.remaining() > 0, the kill was not called.
    assert_eq!(runner.remaining(), 0, "orphan session kill should have consumed the second response");
}
```

Note: `MockRunner` does not record call arguments — it only stores canned responses. Verify that the kill was issued by checking `runner.remaining() == 0` (all responses consumed). Check `MockRunner`'s API for a `remaining()` method; if it doesn't exist, add one or count the VecDeque length.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core --lib providers::terminal::shpool::tests::list_terminals_reaps_orphan -- --no-capture`
Expected: FAIL — current code doesn't reap orphans.

- [ ] **Step 3: Write test for reaper skipping when shpool is down**

```rust
#[tokio::test]
async fn list_terminals_skips_reap_when_shpool_unreachable() {
    let runner = Arc::new(MockRunner::new(vec![Err("connection refused".into())]));
    let (pool, store, _dir) = test_pool(runner);

    // Pre-populate a binding
    {
        let mut s = store.lock().expect("lock");
        let set_id = s.ensure_terminal_set(
            Some(HostName::local()),
            Some(HostPath::new(HostName::local(), "/repo/wt-feat")),
        );
        s.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/0",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "bash",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );
    }

    let terminals = pool.list_terminals().await.expect("should succeed");
    assert!(terminals.is_empty()); // shpool is down, no terminals returned

    // But the binding should still exist (reaper did NOT run)
    let s = store.lock().expect("lock");
    assert_eq!(s.registry().attachables.len(), 1, "attachable should not be reaped when shpool is down");
}
```

- [ ] **Step 4: Implement orphan reaping in `list_terminals`**

In `ShpoolTerminalPool::list_terminals()`, after processing the successful shpool response, add orphan detection:

```rust
// Reap orphan sessions: live in shpool but no binding
for terminal in &parsed_terminals {
    let session_name = terminal_session_binding_ref(&terminal.id);
    if store.lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, &session_name).is_none() {
        info!(session = session_name, "reaping orphan shpool session (no binding)");
        if let Err(e) = self.kill_terminal(&terminal.id).await {
            warn!(session = session_name, err = %e, "failed to reap orphan session");
        }
    }
}
```

Filter orphan terminals out of the returned list (only return terminals that have bindings).

The key point: this only runs on the `Ok(json)` path (shpool is reachable). The `Err` path (shpool down) returns early without reaping.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-core --lib providers::terminal::shpool::tests -- --no-capture`
Expected: All pass.

- [ ] **Step 6: Run full CI checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: Clean.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/providers/terminal/shpool.rs
git commit -m "feat: orphan session reaper in shpool terminal pool"
```

---

### Task 6: Remove missed_scans tracking

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`

- [ ] **Step 1: Simplify `list_terminals` to use binding-based liveness**

In `ShpoolTerminalPool`:
1. Remove the `missed_scans: Mutex<HashMap<String, u32>>` field.
2. Remove the `MAX_MISSED_SHPOOL_SCANS_BEFORE_REAP` constant.
3. Remove the `disconnected_known_terminals` method.
4. Replace: after processing live sessions, iterate all terminal-pool bindings in the store. For each binding whose session was NOT in the live list, emit a `ManagedTerminal` with `TerminalStatus::Disconnected` (reconstructed from the persisted attachable).
5. Update `ShpoolTerminalPool::new` to remove the `missed_scans` field initialization.

- [ ] **Step 2: Update or remove tests that depend on missed_scans**

Search for tests referencing `missed_scans`, `disconnected_known`, or `MAX_MISSED`. Update them to test the new binding-based liveness behavior, or remove if they test the old tracking mechanism only.

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-core --lib providers::terminal::shpool::tests -- --no-capture`
Expected: All pass.

- [ ] **Step 4: Run full CI checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: Clean.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/terminal/shpool.rs
git commit -m "refactor: replace missed_scans with binding-based terminal liveness"
```

---

### Task 7: Integration test — full lifecycle

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` (or a new test file if appropriate)

- [ ] **Step 1: Write an integration test covering the full lifecycle**

This test should exercise:
1. Create a checkout → creates an attachable set
2. Verify the set appears in the repo's snapshot (repo-scoped)
3. Delete the checkout → set is cascade-deleted
4. Verify the set no longer appears in the snapshot
5. Verify terminal kill was attempted

Use the `InProcessDaemon` test infrastructure if available, or the executor + mock providers if that's the established pattern.

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon -- lifecycle`
Expected: PASS.

- [ ] **Step 3: Run full CI checks**

Run: `cargo +nightly-2026-03-12 fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: Clean.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "test: attachable set lifecycle integration test"
```
