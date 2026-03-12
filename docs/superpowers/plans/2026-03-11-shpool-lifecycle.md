# Shpool Daemon Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix three shpool issues: stale socket cleanup on macOS (#244), explicit daemon start with managed config (#243), and terminal cleanup on checkout removal (#237).

**Architecture:** #244 and #243 are both in `ShpoolTerminalPool` initialization — stale socket cleanup runs first, then daemon spawn. #237 threads `terminal_keys` through the protocol (`WorkItem`, `Command`), correlation engine, two-phase delete UI flow, and executor. All three issues share no code dependencies beyond being in the shpool ecosystem.

**Tech Stack:** Rust, tokio, libc (pid check), serde, ratatui (TUI)

**Spec:** `docs/superpowers/specs/2026-03-11-shpool-lifecycle-design.md`

---

## Chunk 1: Protocol changes (#237)

### Task 1: Add `terminal_keys` to `WorkItem`

**Files:**
- Modify: `crates/flotilla-protocol/src/snapshot.rs:79-97` (WorkItem struct)
- Modify: `crates/flotilla-protocol/src/snapshot.rs:129-404` (tests)

- [ ] **Step 1: Add the field to `WorkItem`**

In `crates/flotilla-protocol/src/snapshot.rs`, add after the `source` field (line 96):

```rust
    #[serde(default)]
    pub terminal_keys: Vec<crate::ManagedTerminalId>,
```

- [ ] **Step 2: Update all `WorkItem` construction sites in tests**

Every test that constructs a `WorkItem` literal needs `terminal_keys: vec![]`. Search for `WorkItem {` in `snapshot.rs` tests. There are constructions at approximately lines 239, 256, 297, 311. Add `terminal_keys: vec![],` to each.

- [ ] **Step 3: Add a serde backward-compat test**

Add to the existing `work_item_debug_group_defaults_when_missing` test pattern (around line 344):

```rust
    #[test]
    fn work_item_terminal_keys_defaults_when_missing() {
        let json = r#"{
            "kind": "Issue",
            "identity": {"Issue": "X"},
            "branch": null,
            "description": "test",
            "checkout": null,
            "change_request_key": null,
            "session_key": null,
            "issue_keys": [],
            "workspace_refs": [],
            "is_main_checkout": false
        }"#;
        let decoded: WorkItem = serde_json::from_str(json).expect("deserialize");
        assert!(decoded.terminal_keys.is_empty());
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-protocol`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/snapshot.rs
git commit -m "feat: add terminal_keys to WorkItem (#237)"
```

### Task 2: Add `terminal_keys` to `Command::RemoveCheckout`

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs:20-22` (RemoveCheckout variant)
- Modify: `crates/flotilla-protocol/src/commands.rs:128-338` (tests)

- [ ] **Step 1: Add the field**

Change `RemoveCheckout` from:

```rust
    RemoveCheckout {
        branch: String,
    },
```

to:

```rust
    RemoveCheckout {
        branch: String,
        #[serde(default)]
        terminal_keys: Vec<crate::ManagedTerminalId>,
    },
```

- [ ] **Step 2: Update all `RemoveCheckout` construction sites**

Find every `Command::RemoveCheckout { branch: ... }` in the crate. In `commands.rs` tests there's one at approximately line 152 and one at line 289. Add `terminal_keys: vec![]` to each.

Also update the pattern match in `Command::description()` — the existing `Command::RemoveCheckout { .. }` wildcard pattern is fine, no change needed.

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-protocol`
Expected: All pass

- [ ] **Step 4: Compile full workspace to find other sites**

Run: `cargo build --workspace 2>&1 | head -80`

This will show compilation errors from **both** Task 1 (WorkItem) and Task 2 (RemoveCheckout). Fix all:

**RemoveCheckout construction/pattern sites:**
- `crates/flotilla-core/src/executor.rs:119` — pattern match, uses `..` so should be fine
- `crates/flotilla-tui/src/app/key_handlers.rs:443` — construction site, add `terminal_keys: vec![]` temporarily (wired up in Task 7)
- `crates/flotilla-tui/src/app/key_handlers.rs:975,990` — test pattern matches, add `..`
- `crates/flotilla-core/src/executor.rs` tests (~lines 1236, 1258, 1280) — construction sites, add `terminal_keys: vec![]`

**WorkItem construction sites (from Task 1, fix now with temporary `vec![]`):**
- `crates/flotilla-core/src/convert.rs:33-46` — add `terminal_keys: vec![]` (Task 4 will replace with real value)
- `crates/flotilla-tui/src/app/test_support.rs` — search for `WorkItem {` constructions, add `terminal_keys: vec![]`

- [ ] **Step 5: Run full test suite**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: add terminal_keys to Command::RemoveCheckout (#237)"
```

### Task 3: Add `terminal_ids` to `CorrelatedWorkItem` and populate in correlation

**Files:**
- Modify: `crates/flotilla-core/src/data.rs:60-71` (CorrelatedWorkItem struct)
- Modify: `crates/flotilla-core/src/data.rs:263-371` (group_to_work_item function)
- Modify: `crates/flotilla-core/src/data.rs` (tests)

- [ ] **Step 1: Add `terminal_ids` field to `CorrelatedWorkItem`**

In `crates/flotilla-core/src/data.rs`, add to the struct (after `source`):

```rust
    pub terminal_ids: Vec<flotilla_protocol::ManagedTerminalId>,
```

- [ ] **Step 2: Populate `terminal_ids` in `group_to_work_item()`**

In the loop over `group.items` (around line 273), change the `ManagedTerminal` arm from:

```rust
            (CorItemKind::ManagedTerminal, ProviderItemKey::ManagedTerminal(_key)) => {
                // Managed terminals contribute to correlation but don't need
                // explicit tracking on work items yet — their presence in the
                // group is enough. The terminal pool provider shows them in
                // ProviderData.managed_terminals.
            }
```

to:

```rust
            (CorItemKind::ManagedTerminal, ProviderItemKey::ManagedTerminal(key)) => {
                if let Some(terminal) = providers.managed_terminals.get(key.as_str()) {
                    terminal_ids.push(terminal.id.clone());
                }
            }
```

Add `let mut terminal_ids: Vec<flotilla_protocol::ManagedTerminalId> = Vec::new();` alongside the other `let mut` declarations at the top of the function (around line 268-271).

Add `terminal_ids` to the `CorrelatedWorkItem` construction at line 360-370:

```rust
    Some(CorrelationResult::Correlated(CorrelatedWorkItem {
        anchor,
        branch,
        description,
        linked_change_request,
        linked_session,
        linked_issues: Vec::new(),
        workspace_refs,
        correlation_group_idx: group_idx,
        source,
        terminal_ids,
    }))
```

- [ ] **Step 3: Fix any other `CorrelatedWorkItem` construction sites**

Run: `cargo build -p flotilla-core 2>&1 | head -40`

There will be compilation errors wherever `CorrelatedWorkItem` is constructed. Add `terminal_ids: vec![]` to each:

- `crates/flotilla-core/src/convert.rs` tests (~4 construction sites around lines 119, 180, 200, 207)
- `crates/flotilla-core/src/data.rs` test helper `correlated()` (~line 801-812) — this feeds all `data.rs` tests via `..correlated(...)`

- [ ] **Step 4: Add accessor method on `CorrelationResult`**

Add alongside the existing accessor methods (after `workspace_refs()` around line 178):

```rust
    pub fn terminal_ids(&self) -> &[flotilla_protocol::ManagedTerminalId] {
        match self {
            CorrelationResult::Correlated(c) => &c.terminal_ids,
            _ => &[],
        }
    }
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p flotilla-core`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/data.rs crates/flotilla-core/src/convert.rs
git commit -m "feat: populate terminal_ids from correlation groups (#237)"
```

### Task 4: Map `terminal_ids` through `convert.rs`

**Files:**
- Modify: `crates/flotilla-core/src/convert.rs:15-47` (correlation_result_to_work_item)

- [ ] **Step 1: Add `terminal_keys` to the WorkItem construction**

In `correlation_result_to_work_item()`, add to the `WorkItem { ... }` construction (after the `source` line):

```rust
        terminal_keys: item.terminal_ids().to_vec(),
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p flotilla-core`
Expected: All pass

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/convert.rs
git commit -m "feat: map terminal_ids to terminal_keys in protocol conversion (#237)"
```

## Chunk 2: Stale socket cleanup and daemon start (#244, #243)

### Task 5: Add libc dependency and implement stale socket cleanup

**Files:**
- Modify: `crates/flotilla-core/Cargo.toml` (add libc)
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`

- [ ] **Step 1: Add libc dependency**

In `crates/flotilla-core/Cargo.toml`, add under `[dependencies]`:

```toml
libc = "0.2"
```

- [ ] **Step 2: Write test for stale socket cleanup**

In `crates/flotilla-core/src/providers/terminal/shpool.rs`, add to the `tests` module:

```rust
    #[test]
    fn clean_stale_socket_removes_dead_pid_artifacts() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        let pid_path = dir.path().join("daemonized-shpool.pid");

        // Create a socket file and a pid file pointing to a dead process
        std::fs::write(&socket_path, b"").expect("create fake socket");
        // PID 99999999 is almost certainly not running
        std::fs::write(&pid_path, "99999999").expect("create fake pid");

        ShpoolTerminalPool::clean_stale_socket(&socket_path);

        assert!(!socket_path.exists(), "stale socket should be removed");
        assert!(!pid_path.exists(), "stale pid file should be removed");
    }

    #[test]
    fn clean_stale_socket_removes_orphan_socket_without_pid_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");

        // Socket exists but no pid file
        std::fs::write(&socket_path, b"").expect("create fake socket");

        ShpoolTerminalPool::clean_stale_socket(&socket_path);

        assert!(!socket_path.exists(), "orphan socket should be removed");
    }

    #[test]
    fn clean_stale_socket_noop_when_nothing_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");

        // Nothing exists — should not panic
        ShpoolTerminalPool::clean_stale_socket(&socket_path);
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p flotilla-core clean_stale_socket`
Expected: FAIL — `clean_stale_socket` method not found

- [ ] **Step 4: Implement `clean_stale_socket`**

Add to the `impl ShpoolTerminalPool` block, before `ensure_config`:

```rust
    /// Remove stale shpool socket and pid files when the daemon is dead.
    ///
    /// On macOS, `connect()` to a stale Unix socket succeeds (unlike Linux
    /// where it returns ConnectionRefused), causing shpool's auto-daemonize
    /// to think a daemon is running when it isn't.
    #[cfg(unix)]
    fn clean_stale_socket(socket_path: &Path) {
        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

        if !socket_path.exists() {
            return;
        }

        match std::fs::read_to_string(&pid_path) {
            Ok(contents) => {
                if let Ok(pid) = contents.trim().parse::<i32>() {
                    // Signal 0 checks process existence without sending a signal
                    let alive = unsafe { libc::kill(pid, 0) } == 0;
                    if alive {
                        tracing::debug!(%pid, "shpool daemon is alive, keeping socket");
                        return;
                    }
                    tracing::info!(%pid, "shpool daemon is dead, removing stale socket");
                }
                // PID file exists but daemon is dead (or unparseable) — remove both
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&pid_path);
            }
            Err(_) => {
                // No pid file but socket exists — can't verify liveness, remove it
                tracing::info!("no pid file found, removing orphaned shpool socket");
                let _ = std::fs::remove_file(socket_path);
            }
        }
    }

    #[cfg(not(unix))]
    fn clean_stale_socket(_socket_path: &Path) {
        // Unix sockets don't exist on non-Unix platforms
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-core clean_stale_socket`
Expected: All 3 pass

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/Cargo.toml crates/flotilla-core/src/providers/terminal/shpool.rs
git commit -m "feat: stale socket cleanup for macOS (#244)"
```

### Task 6: Convert `ShpoolTerminalPool::new()` to async `create()` with daemon spawn

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`
- Modify: `crates/flotilla-core/src/providers/discovery.rs:344-354`

- [ ] **Step 1: Write test for create**

Add to the `tests` module in `shpool.rs`:

```rust
    /// Create a ShpoolTerminalPool via the async factory method.
    async fn test_pool_async(runner: Arc<MockRunner>) -> (ShpoolTerminalPool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create tempdir for shpool test");
        let socket_path = dir.path().join("shpool.socket");
        let pool = ShpoolTerminalPool::create(runner, socket_path).await;
        (pool, dir)
    }

    #[tokio::test]
    async fn create_writes_config_and_returns_pool() {
        // No mock responses needed — start_daemon spawns shpool directly
        // (not through MockRunner), and will fail gracefully in test.
        let runner = Arc::new(MockRunner::new(vec![]));
        let (pool, dir) = test_pool_async(runner).await;
        let config_path = dir.path().join("config.toml");
        assert!(config_path.exists(), "config should be written");
        assert_eq!(pool.display_name(), "shpool");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core create_writes_config`
Expected: FAIL — `create` method not found

- [ ] **Step 3: Implement `create()` and refactor `new()`**

Replace the existing `new()` method with:

```rust
    /// Create a new ShpoolTerminalPool, cleaning up stale sockets and
    /// spawning the daemon with flotilla's managed config.
    pub async fn create(runner: Arc<dyn CommandRunner>, socket_path: PathBuf) -> Self {
        let config_path = socket_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("config.toml");
        Self::ensure_config(&config_path);
        Self::clean_stale_socket(&socket_path);
        Self::start_daemon(&socket_path, &config_path).await;
        Self {
            runner,
            socket_path,
            config_path,
        }
    }

    /// Spawn the shpool daemon as a background process with flotilla's config.
    ///
    /// If a daemon is already running, kills it first so the new one picks up
    /// the latest config. If the spawn fails, logs a warning — shpool's
    /// built-in auto-daemonize from `attach` will still work as a fallback.
    async fn start_daemon(socket_path: &Path, config_path: &Path) {
        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

        // Kill existing daemon if running (to apply fresh config)
        if let Ok(contents) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = contents.trim().parse::<i32>() {
                #[cfg(unix)]
                if unsafe { libc::kill(pid, 0) } == 0 {
                    tracing::info!(%pid, "killing existing shpool daemon to apply fresh config");
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    // Give it a moment to shut down
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    // Clean up after killed daemon
                    let _ = std::fs::remove_file(socket_path);
                    let _ = std::fs::remove_file(&pid_path);
                }
            }
        }

        let socket_str = socket_path.display().to_string();
        let config_str = config_path.display().to_string();
        let log_path = socket_path.with_file_name("daemonized-shpool.log");

        match std::fs::File::create(&log_path) {
            Ok(log_file) => {
                // Clone for stderr before consuming for stdout
                let log_stderr = match log_file.try_clone() {
                    Ok(f) => f.into(),
                    Err(_) => std::process::Stdio::null(),
                };
                let result = tokio::process::Command::new("shpool")
                    .args(["--socket", &socket_str, "-c", &config_str, "daemon"])
                    .stdin(std::process::Stdio::null())
                    .stdout(log_file)
                    .stderr(log_stderr)
                    .spawn();

                match result {
                    Ok(_child) => {
                        tracing::info!("spawned shpool daemon");
                        // Wait for socket to appear (up to 2s)
                        for _ in 0..20 {
                            if socket_path.exists() {
                                tracing::debug!("shpool socket is ready");
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        tracing::warn!("shpool socket did not appear within 2s");
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "failed to spawn shpool daemon");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "failed to create shpool log file");
            }
        }
    }
```

Keep the sync `new()` for tests that don't need daemon spawn:

```rust
    /// Sync constructor for tests — skips daemon lifecycle.
    #[cfg(test)]
    pub(crate) fn new(runner: Arc<dyn CommandRunner>, socket_path: PathBuf) -> Self {
        let config_path = socket_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("config.toml");
        Self::ensure_config(&config_path);
        Self {
            runner,
            socket_path,
            config_path,
        }
    }
```

- [ ] **Step 4: Update discovery.rs to use `create()`**

In `crates/flotilla-core/src/providers/discovery.rs`, change the shpool block (around line 344-354) from:

```rust
        registry.terminal_pool = Some((
            "shpool".into(),
            Arc::new(crate::providers::terminal::shpool::ShpoolTerminalPool::new(
                Arc::clone(&runner),
                shpool_socket,
            )),
        ));
```

to:

```rust
        registry.terminal_pool = Some((
            "shpool".into(),
            Arc::new(crate::providers::terminal::shpool::ShpoolTerminalPool::create(
                Arc::clone(&runner),
                shpool_socket,
            ).await),
        ));
```

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/providers/terminal/shpool.rs crates/flotilla-core/src/providers/discovery.rs
git commit -m "feat: explicit daemon start with managed config (#243)"
```

## Chunk 3: Terminal cleanup on checkout removal (#237)

### Task 7: Thread `terminal_keys` through the TUI delete confirmation flow

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs:52-55` (UiMode::DeleteConfirm)
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:290-297` (resolve_and_push for RemoveCheckout)
- Modify: `crates/flotilla-tui/src/app/executor.rs:80-84` (handle_result for CheckoutStatus)
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:432-455` (handle_delete_confirm_key)

- [ ] **Step 1: Add `terminal_keys` to `UiMode::DeleteConfirm`**

In `crates/flotilla-tui/src/app/ui_state.rs`, change:

```rust
    DeleteConfirm {
        info: Option<CheckoutStatus>,
        loading: bool,
    },
```

to:

```rust
    DeleteConfirm {
        info: Option<CheckoutStatus>,
        loading: bool,
        terminal_keys: Vec<flotilla_protocol::ManagedTerminalId>,
    },
```

- [ ] **Step 2: Fix compilation — add `terminal_keys` to all DeleteConfirm sites**

Run: `cargo build -p flotilla-tui 2>&1 | head -80`

Fix **all** construction and pattern-match sites:

**Production code — wire up real terminal_keys:**

In `key_handlers.rs` `resolve_and_push` (around line 293-297), capture terminal_keys from the work item:

```rust
                Intent::RemoveCheckout => {
                    self.ui.mode = UiMode::DeleteConfirm {
                        info: None,
                        loading: true,
                        terminal_keys: item.terminal_keys.clone(),
                    };
                }
```

In `executor.rs` `handle_result` (around line 80-84), preserve existing terminal_keys from loading state:

```rust
        CommandResult::CheckoutStatus(info) => {
            // Preserve terminal_keys from the loading state
            let terminal_keys = match &app.ui.mode {
                UiMode::DeleteConfirm { terminal_keys, .. } => terminal_keys.clone(),
                _ => vec![],
            };
            app.ui.mode = UiMode::DeleteConfirm {
                info: Some(info),
                loading: false,
                terminal_keys,
            };
        }
```

**Pattern match in ui.rs** — add `..` to ignore terminal_keys:

In `crates/flotilla-tui/src/ui.rs` (~line 807), change:
```rust
let UiMode::DeleteConfirm { ref info, loading } = ui.mode else {
```
to:
```rust
let UiMode::DeleteConfirm { ref info, loading, .. } = ui.mode else {
```

**Test constructions — add `terminal_keys: vec![]`:**

- `crates/flotilla-tui/src/app/ui_state.rs:265` (test `is_config_returns_true_only_for_config_variant`)
- `crates/flotilla-tui/src/app/key_handlers.rs:489-501` (test helper `delete_confirm_mode`)
- `crates/flotilla-tui/src/app/key_handlers.rs:1000-1003` (test `delete_confirm_ignores_while_loading`)
- `crates/flotilla-tui/src/app/key_handlers.rs:1320-1323` (test `delete_confirm_y_with_no_info_does_not_push_command`)
- `crates/flotilla-tui/tests/snapshots.rs:258` (test `delete_confirm_safe_to_delete`)
- `crates/flotilla-tui/tests/snapshots.rs:276` (test `delete_confirm_with_uncommitted_files`)
- `crates/flotilla-tui/tests/snapshots.rs:301` (test `delete_confirm_with_many_uncommitted_files`)

- [ ] **Step 3: Wire `terminal_keys` into `Command::RemoveCheckout`**

In `key_handlers.rs` `handle_delete_confirm_key` (around line 432-455), change:

```rust
                    if let UiMode::DeleteConfirm {
                        info: Some(ref info),
                        ..
                    } = self.ui.mode
                    {
                        self.proto_commands.push(Command::RemoveCheckout {
                            branch: info.branch.clone(),
                        });
                    }
```

to:

```rust
                    if let UiMode::DeleteConfirm {
                        info: Some(ref info),
                        ref terminal_keys,
                        ..
                    } = self.ui.mode
                    {
                        self.proto_commands.push(Command::RemoveCheckout {
                            branch: info.branch.clone(),
                            terminal_keys: terminal_keys.clone(),
                        });
                    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/
git commit -m "feat: thread terminal_keys through delete confirmation flow (#237)"
```

### Task 8: Kill terminals in executor after checkout removal

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:119-133` (RemoveCheckout handler)

- [ ] **Step 1: Write test for terminal cleanup in executor**

`kill_terminal` is called on `registry.terminal_pool` (the `TerminalPool` trait), NOT through `MockRunner`. We need a `MockTerminalPool` to verify the call. Add to the executor test module:

```rust
    use crate::providers::terminal::TerminalPool;
    use flotilla_protocol::{ManagedTerminal, ManagedTerminalId};

    struct MockTerminalPool {
        killed: tokio::sync::Mutex<Vec<ManagedTerminalId>>,
    }

    #[async_trait::async_trait]
    impl TerminalPool for MockTerminalPool {
        fn display_name(&self) -> &str { "mock-pool" }
        async fn list_terminals(&self) -> Result<Vec<ManagedTerminal>, String> { Ok(vec![]) }
        async fn ensure_running(&self, _id: &ManagedTerminalId, _cmd: &str, _cwd: &Path) -> Result<(), String> { Ok(()) }
        async fn attach_command(&self, _id: &ManagedTerminalId, _cmd: &str, _cwd: &Path) -> Result<String, String> { Ok(String::new()) }
        async fn kill_terminal(&self, id: &ManagedTerminalId) -> Result<(), String> {
            self.killed.lock().await.push(id.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn remove_checkout_kills_correlated_terminals() {
        let terminal_id = ManagedTerminalId {
            checkout: "feat-x".into(),
            role: "shell".into(),
            index: 0,
        };
        let mock_pool = Arc::new(MockTerminalPool {
            killed: tokio::sync::Mutex::new(vec![]),
        });

        let mut registry = empty_registry();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")),
        );
        registry.terminal_pool = Some((
            "shpool".into(),
            Arc::clone(&mock_pool) as Arc<dyn TerminalPool>,
        ));

        let result = run_execute(
            Command::RemoveCheckout {
                branch: "feat-x".into(),
                terminal_keys: vec![terminal_id.clone()],
            },
            &registry,
            &empty_data(),
            &runner_ok(),
        )
        .await;

        assert_ok(result);
        let killed = mock_pool.killed.lock().await;
        assert_eq!(killed.len(), 1);
        assert_eq!(killed[0], terminal_id);
    }
```

Note: Adapt the test helper names (`empty_registry`, `MockCheckoutManager::succeeding`, `runner_ok`, `run_execute`, `empty_data`, `assert_ok`) to match whatever is actually in the executor test module. Run `cargo build` after writing the test to check compilation — fix any helper name mismatches.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core remove_checkout_kills`
Expected: FAIL — terminal_keys not destructured / kill not called

- [ ] **Step 3: Implement terminal cleanup in RemoveCheckout handler**

In `crates/flotilla-core/src/executor.rs`, change the `RemoveCheckout` handler. After Task 2, the pattern already uses `..` to ignore `terminal_keys`. Change it to destructure `terminal_keys` explicitly and use it after successful removal:

```rust
        Command::RemoveCheckout {
            branch,
            terminal_keys,
        } => {
            info!(%branch, "removing checkout");
            let result = if let Some(cm) = registry.checkout_managers.values().next() {
                Some(cm.remove_checkout(repo_root, &branch).await)
            } else {
                None
            };
            match result {
                Some(Ok(())) => {
                    // Best-effort cleanup of correlated terminal sessions
                    if let Some((_, tp)) = &registry.terminal_pool {
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
                    CommandResult::Ok
                }
                Some(Err(e)) => CommandResult::Error { message: e },
                None => CommandResult::Error {
                    message: "No checkout manager available".to_string(),
                },
            }
        }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core remove_checkout_kills`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat: kill correlated terminals on checkout removal (#237)"
```

## Chunk 4: Final verification

### Task 9: Lint, format, and full test run

- [ ] **Step 1: Format**

Run: `cargo fmt`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`

Fix any warnings.

- [ ] **Step 3: Full test suite**

Run: `cargo test --locked`

- [ ] **Step 4: Commit any fixes**

```bash
git add -A
git commit -m "chore: lint and format fixes"
```
