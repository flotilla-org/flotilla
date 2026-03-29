# Remoteable Daemon Startup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make shpool daemon startup work across remote hosts and sandboxes by adding `ensure_file` to `CommandRunner` and simplifying the shpool provider to rely on shpool's built-in auto-daemonize.

**Architecture:** Add `ensure_file(path, content)` to the `CommandRunner` trait. Local impl uses `fs::write`; `EnvironmentRunner` delegates via shell. Then gut shpool's ~250-line manual daemon lifecycle, leaving a provider that looks like cleat — just `CommandRunner` calls.

**Tech Stack:** Rust, async-trait, tokio

---

### Task 1: Add `ensure_file` to `CommandRunner` trait and `ProcessCommandRunner`

**Files:**
- Modify: `crates/flotilla-core/src/providers/mod.rs:148-207` (trait + ProcessCommandRunner)

- [ ] **Step 1: Add the method to the `CommandRunner` trait with a default impl**

In `crates/flotilla-core/src/providers/mod.rs`, add `ensure_file` to the trait. Provide a default implementation so that test mocks that don't care about file writes just succeed without having to implement the method:

```rust
// Add to the CommandRunner trait (after the `exists` method):

    /// Write `content` to `path`, creating parent directories as needed.
    /// Default implementation succeeds silently — override in production runners.
    async fn ensure_file(&self, _path: &Path, _content: &str) -> Result<(), String> {
        Ok(())
    }
```

- [ ] **Step 2: Implement `ensure_file` on `ProcessCommandRunner`**

In the `impl CommandRunner for ProcessCommandRunner` block, override the default:

```rust
    async fn ensure_file(&self, path: &Path, content: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;
        }
        std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
    }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build --workspace --locked 2>&1 | tail -5`
Expected: Compiles. The default impl means all existing `CommandRunner` impls (MockRunner, ReplayRunner, RecordingRunner, DiscoveryMockRunner, clone.rs RecordingRunner) still compile without changes.

- [ ] **Step 4: Write a test for `ProcessCommandRunner::ensure_file`**

Add a test in the `testing` module at the bottom of `crates/flotilla-core/src/providers/mod.rs`:

```rust
    #[tokio::test]
    async fn process_runner_ensure_file_creates_parents_and_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/dir/config.toml");
        let runner = super::ProcessCommandRunner;
        runner.ensure_file(&path, "hello = true\n").await.expect("ensure_file");
        let content = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(content, "hello = true\n");
    }
```

- [ ] **Step 5: Run the test**

Run: `cargo test -p flotilla-core process_runner_ensure_file --locked`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/providers/mod.rs
git commit -m "feat: add ensure_file to CommandRunner trait"
```

---

### Task 2: Implement `ensure_file` on `EnvironmentRunner`

**Files:**
- Modify: `crates/flotilla-core/src/providers/environment/runner.rs`

- [ ] **Step 1: Write a test for EnvironmentRunner's ensure_file**

The `EnvironmentRunner` wraps an inner `CommandRunner`. Its `ensure_file` should delegate to the inner runner via `sh -c "mkdir -p ... && printf '%s' ... > ..."`, wrapped in `docker exec`. We can verify this by checking what the inner runner receives.

Add a test module to the bottom of `crates/flotilla-core/src/providers/environment/runner.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::EnvironmentRunner;
    use crate::providers::{testing::MockRunner, CommandRunner};

    #[tokio::test]
    async fn ensure_file_delegates_via_docker_exec_sh() {
        let inner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let runner = EnvironmentRunner::new("my-container".into(), inner.clone());

        runner.ensure_file(Path::new("/app/config/shpool.toml"), "key = true\n").await.expect("ensure_file");

        let calls = inner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "docker");
        let args = &calls[0].1;
        assert!(args.contains(&"exec".to_string()));
        assert!(args.contains(&"my-container".to_string()));
        assert!(args.contains(&"sh".to_string()));
        // The sh -c script should create the parent dir and write the file
        let script = args.last().expect("should have script arg");
        assert!(script.contains("mkdir -p"), "script should create parent dirs: {script}");
        assert!(script.contains("/app/config/shpool.toml"), "script should reference target path: {script}");
        assert!(script.contains("key = true"), "script should contain file content: {script}");
    }
}
```

- [ ] **Step 2: Run the test to see it fail**

Run: `cargo test -p flotilla-core ensure_file_delegates_via_docker --locked`
Expected: FAIL — `EnvironmentRunner` inherits the default no-op `ensure_file`

- [ ] **Step 3: Implement `ensure_file` on `EnvironmentRunner`**

In the `impl CommandRunner for EnvironmentRunner` block in `crates/flotilla-core/src/providers/environment/runner.rs`, add:

```rust
    async fn ensure_file(&self, path: &Path, content: &str) -> Result<(), String> {
        let parent = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| ".".to_string());
        let path_str = path.to_string_lossy();
        // Use printf with %s to avoid echo's backslash interpretation.
        // Content is passed as a shell argument — single-quote it and escape
        // embedded single quotes with the '\'' idiom.
        let escaped = content.replace('\'', "'\\''");
        let script = format!("mkdir -p '{parent}' && printf '%s' '{escaped}' > '{path_str}'");
        let docker_args = vec!["exec", &self.container_name, "sh", "-c", &script];
        self.inner.run("docker", &docker_args, Path::new("/"), &ChannelLabel::Noop).await.map(|_| ())
    }
```

- [ ] **Step 4: Run the test to see it pass**

Run: `cargo test -p flotilla-core ensure_file_delegates_via_docker --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/environment/runner.rs
git commit -m "feat: implement ensure_file on EnvironmentRunner via docker exec"
```

---

### Task 3: Simplify `ShpoolTerminalPool::create()`

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`

- [ ] **Step 1: Replace `create()` with a simple ensure_file call**

Replace the entire `create` method (lines 50–93) with:

```rust
    pub async fn create(runner: Arc<dyn CommandRunner>, socket_path: DaemonHostPath, terminal_env_defaults: TerminalEnvVars) -> Self {
        let config_path = DaemonHostPath::new(socket_path.as_path().parent().unwrap_or(Path::new(".")).join("config.toml"));
        if let Err(e) = runner.ensure_file(config_path.as_path(), FLOTILLA_SHPOOL_CONFIG).await {
            tracing::warn!(err = %e, "failed to write shpool config");
        }
        Self { runner, socket_path, config_path, terminal_env_defaults }
    }
```

- [ ] **Step 2: Remove the daemon lifecycle code**

Delete these items from the `impl ShpoolTerminalPool` block:

- `ShpoolDaemonState` enum (lines 31–38)
- `ShpoolNoPidProbe` enum (lines 40–45)
- `SHPOOL_DAEMON_PROBE_TIMEOUT` constant (line 29)
- `is_process_alive()` (lines 106–113)
- `is_expected_process()` (lines 119–125)
- `clean_stale_socket()` — both `#[cfg(unix)]` and `#[cfg(not(unix))]` variants (lines 132–164)
- `detect_daemon_state()` — both variants (lines 166–199)
- `probe_daemon_without_pid_file()` — both variants (lines 201–231)
- `probe_failure_is_definitely_stale()` (lines 233–240)
- `start_daemon()` — both variants (lines 248–321)
- `stop_daemon()` — both variants (lines 331–402)
- `config_needs_update()` (lines 405–410)
- `write_config()` (lines 413–425)

- [ ] **Step 3: Remove unused imports**

After deleting the lifecycle code, remove any imports that are no longer needed. The `sysinfo` and `libc` imports in the function bodies are gone with the functions. Check for unused `std::fs`, `std::io`, `tokio::process` etc.

Also remove `run_output` from the use statement at line 9 if `ensure_session` is the only remaining user — check whether it's still needed. (`ensure_session` uses `run_output!` so it stays.)

- [ ] **Step 4: Update the test constructor**

The `#[cfg(test)] fn new()` (lines 97–101) currently calls `Self::write_config()`. Replace it to use the simpler form:

```rust
    #[cfg(test)]
    pub(crate) fn new(runner: Arc<dyn CommandRunner>, socket_path: DaemonHostPath) -> Self {
        let config_path = DaemonHostPath::new(socket_path.as_path().parent().unwrap_or(Path::new(".")).join("config.toml"));
        Self { runner, socket_path, config_path, terminal_env_defaults: vec![] }
    }
```

The test constructor doesn't need to write the config file — tests mock out the runner and never talk to a real shpool daemon.

- [ ] **Step 5: Verify it compiles**

Run: `cargo build --workspace --locked 2>&1 | tail -10`
Expected: Compiles with no errors. There may be warnings about unused code which we address next.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/providers/terminal/shpool.rs
git commit -m "refactor: gut shpool manual daemon lifecycle, rely on auto-daemonize"
```

---

### Task 4: Update shpool tests

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool/tests.rs`

- [ ] **Step 1: Remove tests for deleted code**

Delete these tests that test removed functionality:
- `write_config_writes_expected_content` (lines 27–35)
- `config_needs_update_tracks_staleness` (lines 37–52)

- [ ] **Step 2: Verify remaining tests pass**

Run: `cargo test -p flotilla-core --locked -- shpool`
Expected: All remaining shpool tests pass. The `parse_list_json_*`, `list_sessions_*`, `attach_*`, `kill_*`, and `ensure_session_*` tests should be unaffected.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/terminal/shpool/tests.rs
git commit -m "test: remove tests for deleted shpool lifecycle code"
```

---

### Task 5: Update shpool factory test

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/factories/shpool.rs`

- [ ] **Step 1: Verify factory test still passes**

The factory test at line 57 calls `ShpoolTerminalPoolFactory.probe(...)` which calls `ShpoolTerminalPool::create()`. The simplified `create()` calls `runner.ensure_file()` — which uses the default no-op impl on `DiscoveryMockRunner`, so it succeeds silently.

Run: `cargo test -p flotilla-core --locked -- shpool_factory`
Expected: All three factory tests pass.

- [ ] **Step 2: Commit if any changes were needed**

If no changes needed, skip this commit.

---

### Task 6: Run full CI checks

- [ ] **Step 1: Format check**

Run: `cargo +nightly-2026-03-12 fmt --check`
Expected: No formatting issues.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -20`
Expected: No warnings. Removing the lifecycle code may surface unused-import warnings — fix any that appear.

- [ ] **Step 3: Full test suite**

Run: `cargo test --workspace --locked 2>&1 | tail -20`
Expected: All tests pass.

- [ ] **Step 4: Fix any issues and commit**

If clippy or tests flag issues (unused imports, dead code), fix them and commit:

```bash
git commit -am "chore: fix clippy warnings from shpool cleanup"
```
