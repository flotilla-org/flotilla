# Shpool Config-Change Restart Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Only restart the shpool daemon when the managed config file actually changes, avoiding unnecessary session disruption.

**Architecture:** Make `ensure_config()` return a bool indicating whether the config was written, add a `stop_daemon()` helper that sends SIGTERM and waits for exit, and gate the restart on both conditions (config changed AND daemon alive).

**Tech Stack:** Rust, libc (existing dependency), tokio (async runtime), tracing

**Spec:** `docs/superpowers/specs/2026-03-12-shpool-config-restart-design.md`

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/flotilla-core/src/providers/terminal/shpool.rs` | Modify | All changes: `ensure_config() -> bool`, `stop_daemon()`, updated `create()` flow, new tests |

---

### Task 1: Make `ensure_config()` return bool

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs:172-188` (ensure_config)
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs:32` (call site in create)
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs:49` (call site in test-only new)

- [ ] **Step 1: Write test for ensure_config return value**

Add this test after the existing `ensure_config_writes_expected_content` test (around line 373):

```rust
#[test]
fn ensure_config_returns_true_on_first_write_false_on_second() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config_path = dir.path().join("config.toml");

    // First call: file doesn't exist → should write and return true
    assert!(ShpoolTerminalPool::ensure_config(&config_path));

    // Second call: file matches → should return false
    assert!(!ShpoolTerminalPool::ensure_config(&config_path));

    // Modify the file externally → should return true again
    std::fs::write(&config_path, "stale config").expect("write stale");
    assert!(ShpoolTerminalPool::ensure_config(&config_path));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core ensure_config_returns_true`
Expected: FAIL — `ensure_config` returns `()`, not `bool`

- [ ] **Step 3: Change ensure_config to return bool**

Change `ensure_config` at line 173 from:

```rust
fn ensure_config(path: &Path) {
    let needs_write = match std::fs::read_to_string(path) {
        Ok(existing) => existing != FLOTILLA_SHPOOL_CONFIG,
        Err(_) => true,
    };
    if needs_write {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(path = %parent.display(), err = %e, "failed to create shpool config dir");
            }
        }
        if let Err(e) = std::fs::write(path, FLOTILLA_SHPOOL_CONFIG) {
            tracing::warn!(path = %path.display(), err = %e, "failed to write shpool config");
        }
    }
}
```

To:

```rust
fn ensure_config(path: &Path) -> bool {
    let needs_write = match std::fs::read_to_string(path) {
        Ok(existing) => existing != FLOTILLA_SHPOOL_CONFIG,
        Err(_) => true,
    };
    if needs_write {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(path = %parent.display(), err = %e, "failed to create shpool config dir");
                return false;
            }
        }
        if let Err(e) = std::fs::write(path, FLOTILLA_SHPOOL_CONFIG) {
            tracing::warn!(path = %path.display(), err = %e, "failed to write shpool config");
            return false;
        }
        return true;
    }
    false
}
```

Update the call site in `create()` (line 32) — no change needed yet, just ignore the return value with `let _config_changed =` for now. We'll use it in Task 3.

Update the call site in test-only `new()` (line 49) — discard the return with `let _ =`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-core -- shpool`
Expected: All shpool tests pass, including the new one.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/terminal/shpool.rs
git commit -m "feat: make ensure_config return whether config changed (#251)"
```

---

### Task 2: Add `stop_daemon()` helper

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs` — add `stop_daemon` method after `start_daemon` (around line 165)

- [ ] **Step 1: Write test for stop_daemon with dead pid**

Add this test in the tests module:

```rust
#[tokio::test]
async fn stop_daemon_cleans_up_dead_pid() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let socket_path = dir.path().join("shpool.socket");
    let pid_path = dir.path().join("daemonized-shpool.pid");

    // Create fake socket and pid file pointing to a dead process
    std::fs::write(&socket_path, b"").expect("create fake socket");
    std::fs::write(&pid_path, "99999999").expect("create fake pid");

    ShpoolTerminalPool::stop_daemon(&socket_path).await;

    assert!(!socket_path.exists(), "socket should be removed");
    assert!(!pid_path.exists(), "pid file should be removed");
}

#[tokio::test]
async fn stop_daemon_handles_missing_pid_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let socket_path = dir.path().join("shpool.socket");

    // Socket exists but no pid file — should not panic
    std::fs::write(&socket_path, b"").expect("create fake socket");

    ShpoolTerminalPool::stop_daemon(&socket_path).await;

    // Socket should still be removed (best-effort cleanup)
    assert!(!socket_path.exists(), "socket should be removed");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core stop_daemon`
Expected: FAIL — `stop_daemon` method doesn't exist

- [ ] **Step 3: Implement stop_daemon**

Add after the `start_daemon` non-unix stub (around line 170):

```rust
/// Gracefully stop a running shpool daemon by sending SIGTERM and
/// waiting for it to exit. Removes the socket and pid files afterward.
/// This is load-bearing: `start_daemon()` checks socket existence as
/// its first guard, so the socket must be gone for a replacement to spawn.
#[cfg(unix)]
async fn stop_daemon(socket_path: &Path) {
    let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

    // Read and parse the pid — if we can't, just clean up files
    let pid = match std::fs::read_to_string(&pid_path) {
        Ok(contents) => match contents.trim().parse::<i32>().ok().filter(|&p| p > 0) {
            Some(pid) => pid,
            None => {
                tracing::warn!("shpool pid file unparseable, removing socket");
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&pid_path);
                return;
            }
        },
        Err(_) => {
            tracing::warn!("no shpool pid file found, removing socket");
            let _ = std::fs::remove_file(socket_path);
            return;
        }
    };

    // Send SIGTERM
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    if rc != 0 {
        tracing::warn!(%pid, "failed to send SIGTERM to shpool daemon");
        let _ = std::fs::remove_file(socket_path);
        let _ = std::fs::remove_file(&pid_path);
        return;
    }

    // Wait for process to exit (up to 2s)
    for _ in 0..20 {
        if !Self::is_process_alive(pid) {
            tracing::debug!(%pid, "shpool daemon exited after SIGTERM");
            let _ = std::fs::remove_file(socket_path);
            let _ = std::fs::remove_file(&pid_path);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    tracing::warn!(%pid, "shpool daemon did not exit within 2s after SIGTERM");
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(&pid_path);
}

#[cfg(not(unix))]
async fn stop_daemon(_socket_path: &Path) {
    // shpool is Unix-only
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-core -- shpool`
Expected: All shpool tests pass, including both new stop_daemon tests.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/terminal/shpool.rs
git commit -m "feat: add stop_daemon helper for graceful shpool shutdown (#251)"
```

---

### Task 3: Wire up conditional restart in `create()`

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs:27-40` (create method)

- [ ] **Step 1: Update create() to conditionally stop daemon**

Change the `create()` method body from:

```rust
Self::ensure_config(&config_path);
Self::clean_stale_socket(&socket_path);
Self::start_daemon(&socket_path, &config_path).await;
```

To:

```rust
let config_changed = Self::ensure_config(&config_path);
Self::clean_stale_socket(&socket_path);
if config_changed && socket_path.exists() {
    tracing::info!("shpool config changed, restarting daemon");
    Self::stop_daemon(&socket_path).await;
}
Self::start_daemon(&socket_path, &config_path).await;
```

- [ ] **Step 2: Run all tests**

Run: `cargo test -p flotilla-core -- shpool`
Expected: All shpool tests pass. The existing `create_writes_config_and_returns_pool` test still works — config_changed is true but no socket exists, so stop_daemon is not called.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy -p flotilla-core --all-targets --locked -- -D warnings`
Expected: No warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/providers/terminal/shpool.rs
git commit -m "feat: restart shpool daemon only when config changes (#251)"
```

---

### Task 4: Final verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test --locked`
Expected: All tests pass.

- [ ] **Step 2: Run clippy on entire workspace**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: No warnings.

- [ ] **Step 3: Run fmt check**

Run: `cargo fmt -- --check`
Expected: No formatting issues.
