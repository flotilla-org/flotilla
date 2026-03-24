# Terminal Pool ensure_session Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move session setup (env vars, shell resolution, command) into `ensure_session` for both shpool and cleat, so `attach_args` becomes a simple reattach. Fixes the `${SHELL:-/bin/sh}` expansion bug in shpool, and fixes silently-ignored env vars in cleat.

**Architecture:** Add `env_vars` parameter to the `TerminalPool::ensure_session` trait method. Both shpool and cleat bake env vars and the command into the session at creation time. `attach_args` for both simplifies to a plain reattach (no `--cmd`, no env, no shell expansion). For shpool, `ensure_session` spawns `shpool attach --cmd ...` then `shpool detach` to create-and-release. For cleat, `ensure_session` passes env vars into `cleat create --cmd`.

**Tech Stack:** Rust, async-trait, tokio, shpool CLI, cleat CLI

**Key design note — PTY requirement:** Shpool `attach` may require a TTY on the attaching process. If `ensure_session` runs from the daemon (piped I/O, no TTY), shpool might reject the attach. The plan includes an early verification step. If it fails, we'll need to allocate a PTY via the `pty-process` crate or use `script -q /dev/null` as a wrapper.

**Key design note — cleat `--cmd` on attach:** Investigation of the cleat codebase confirms that `--cmd` on `cleat attach` to an existing session is silently ignored. The command and env vars must be set at `cleat create` time. The current flotilla code's `attach_args` with `--cmd` + env vars was dead code for cleat all along.

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/flotilla-core/src/providers/terminal/mod.rs` | Modify | Add `env_vars` to `ensure_session` trait |
| `crates/flotilla-core/src/providers/terminal/shpool.rs` | Modify | Implement real `ensure_session`, simplify `attach_args` |
| `crates/flotilla-core/src/providers/terminal/shpool/tests.rs` | Modify | Update tests for new behavior |
| `crates/flotilla-core/src/providers/terminal/cleat.rs` | Modify | Inject env vars in `ensure_session`, simplify `attach_args` |
| `crates/flotilla-core/src/providers/terminal/passthrough.rs` | Modify | Accept new `env_vars` param (no behavioral change) |
| `crates/flotilla-core/src/terminal_manager.rs` | Modify | Thread `daemon_socket_path` into `ensure_running` |
| `crates/flotilla-core/src/terminal_manager/tests.rs` | Modify | Update `ensure_running` call sites |
| `crates/flotilla-core/src/executor/terminals.rs` | Modify | Pass `daemon_socket_path` to `ensure_running` |
| `crates/flotilla-core/src/executor/tests.rs` | Modify | Update test call sites |
| `crates/flotilla-core/src/hop_chain/tests.rs` | Modify | Update mock `ensure_session` signatures |
| `crates/flotilla-core/src/hop_chain/snapshots/*.snap` | Modify | Snapshots will change for simplified attach args |
| `crates/flotilla-core/src/providers/discovery/test_support.rs` | Modify | Update mock `ensure_session` signature |
| `crates/flotilla-core/src/refresh/tests.rs` | Modify | Update mock `ensure_session` signature |
| `crates/flotilla-core/tests/in_process_daemon.rs` | Modify | Update mock `ensure_session` signature |

---

### Task 1: Verify shpool attach works without a TTY

Before building the implementation, verify that shpool can create a session when spawned from a non-interactive process.

**Files:**
- None (manual verification)

- [ ] **Step 1: Test shpool attach from a non-TTY context**

Run from a shell to simulate what the daemon would do:

```bash
# Spawn shpool attach with piped I/O (no TTY), then detach
shpool attach --cmd 'sleep 300' test-verify-no-tty </dev/null &
ATTACH_PID=$!
sleep 1
shpool list --json
shpool detach test-verify-no-tty
wait $ATTACH_PID 2>/dev/null
shpool kill test-verify-no-tty
```

If `shpool list` shows `test-verify-no-tty` as a session, the approach works.
If it fails with a TTY error, we'll need PTY allocation — flag this before proceeding.

---

### Task 2: Add `env_vars` to `TerminalPool::ensure_session` trait

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/mod.rs:29`

- [ ] **Step 1: Update the trait signature**

```rust
async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path, env_vars: &TerminalEnvVars) -> Result<(), String>;
```

- [ ] **Step 2: Verify compilation fails**

Run: `cargo check -p flotilla-core 2>&1 | head -30`
Expected: compilation errors in all `TerminalPool` implementations (shpool, cleat, passthrough) and mocks.

- [ ] **Step 3: Fix passthrough — accept new param**

In `crates/flotilla-core/src/providers/terminal/passthrough.rs`:

```rust
async fn ensure_session(&self, _session_name: &str, _command: &str, _cwd: &std::path::Path, _env_vars: &TerminalEnvVars) -> Result<(), String> {
```

Add the import for `TerminalEnvVars` if not already present.

- [ ] **Step 4: Fix shpool — accept new param temporarily**

In `crates/flotilla-core/src/providers/terminal/shpool.rs`, update the no-op `ensure_session` to accept the new param (we'll implement it fully in Task 4):

```rust
async fn ensure_session(&self, _session_name: &str, _command: &str, _cwd: &Path, _env_vars: &TerminalEnvVars) -> Result<(), String> {
    // No-op: shpool creates sessions on first `attach`.
    Ok(())
}
```

- [ ] **Step 5: Fix cleat — accept new param temporarily**

In `crates/flotilla-core/src/providers/terminal/cleat.rs`, update `ensure_session` to accept `_env_vars: &TerminalEnvVars`. No behavioral change yet (Task 5 handles that):

```rust
async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path, _env_vars: &TerminalEnvVars) -> Result<(), String> {
```

- [ ] **Step 6: Fix all mock implementations**

Search for all `fn ensure_session` in test files and update signatures. Key locations:
- `crates/flotilla-core/src/terminal_manager/tests.rs` (multiple SharedMock impls)
- `crates/flotilla-core/src/hop_chain/tests.rs` (if any mock)
- `crates/flotilla-core/src/executor/tests.rs` (if any mock)
- `crates/flotilla-core/src/providers/discovery/test_support.rs`
- `crates/flotilla-core/src/refresh/tests.rs`
- `crates/flotilla-core/tests/in_process_daemon.rs`

Each mock should accept the new `_env_vars: &TerminalEnvVars` parameter.

- [ ] **Step 7: Verify it compiles**

Run: `cargo check -p flotilla-core 2>&1 | tail -5`
Expected: success (or only warnings)

- [ ] **Step 8: Run tests**

Run: `cargo test --workspace --locked 2>&1 | tail -10`
Expected: all tests pass (no behavioral change yet)

- [ ] **Step 9: Commit**

```
feat: add env_vars parameter to TerminalPool::ensure_session
```

---

### Task 3: Thread `daemon_socket_path` into `TerminalManager::ensure_running`

**Files:**
- Modify: `crates/flotilla-core/src/terminal_manager.rs:110-121`
- Modify: `crates/flotilla-core/src/executor/terminals.rs:61,118`
- Modify: `crates/flotilla-core/src/terminal_manager/tests.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Write failing test — ensure_running passes env vars to pool**

In `crates/flotilla-core/src/terminal_manager/tests.rs`, update the `ensure_running_uses_attachable_id_as_session_name` test. Update `PoolCall::EnsureSession` to include env_vars:

```rust
EnsureSession { session_name: String, command: String, cwd: PathBuf, env_vars: TerminalEnvVars },
```

Update the SharedMock impl to record them. Then assert that env_vars contains `FLOTILLA_ATTACHABLE_ID` when `ensure_running` is called with a socket path.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core ensure_running_uses_attachable_id 2>&1 | tail -10`
Expected: FAIL — `ensure_running` doesn't pass env_vars yet.

- [ ] **Step 3: Update `ensure_running` to accept `daemon_socket_path` and build env vars**

In `crates/flotilla-core/src/terminal_manager.rs`:

```rust
pub async fn ensure_running(&self, attachable_id: &AttachableId, daemon_socket_path: Option<&str>) -> Result<(), String> {
    let (command, cwd) = {
        let store = self.store.lock().map_err(|e| format!("failed to lock store: {e}"))?;
        let attachable =
            store.registry().attachables.get(attachable_id).ok_or_else(|| format!("attachable not found: {attachable_id}"))?;
        match &attachable.content {
            AttachableContent::Terminal(t) => (t.command.clone(), t.working_directory.clone()),
        }
    };
    let mut env_vars: TerminalEnvVars = vec![("FLOTILLA_ATTACHABLE_ID".to_string(), attachable_id.to_string())];
    if let Some(socket) = daemon_socket_path {
        env_vars.push(("FLOTILLA_DAEMON_SOCKET".to_string(), socket.to_string()));
    }
    let session_name = attachable_id.to_string();
    self.pool.ensure_session(&session_name, &command, &cwd, &env_vars).await
}
```

Add import for `TerminalEnvVars`:
```rust
use crate::providers::terminal::TerminalEnvVars;
```

- [ ] **Step 4: Update callers of `ensure_running`**

In `crates/flotilla-core/src/executor/terminals.rs`, both call sites (lines ~61 and ~118) need to pass the socket path:

```rust
if let Err(err) = self.terminal_manager.ensure_running(&attachable_id, socket_str.as_deref()).await {
```

The `socket_str` is already available at both call sites from `self.daemon_socket_path.map(|p| p.display().to_string())`.

- [ ] **Step 5: Fix remaining test compilation**

Update all test call sites for `ensure_running` to pass the new parameter. Key locations:
- `crates/flotilla-core/src/terminal_manager/tests.rs` — `ensure_running(&att_id, Some("/tmp/flotilla.sock"))` or `ensure_running(&att_id, None)`
- `crates/flotilla-core/src/executor/tests.rs` — if any direct calls

- [ ] **Step 6: Run tests**

Run: `cargo test --workspace --locked 2>&1 | tail -10`
Expected: all pass

- [ ] **Step 7: Commit**

```
feat: thread daemon_socket_path into ensure_running for env var injection
```

---

### Task 4: Implement shpool's `ensure_session` — create session via attach + detach

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs:479-482`
- Modify: `crates/flotilla-core/src/providers/terminal/shpool/tests.rs`

- [ ] **Step 1: Write failing test — ensure_session runs attach then detach**

In `crates/flotilla-core/src/providers/terminal/shpool/tests.rs`:

```rust
#[tokio::test]
async fn ensure_session_creates_via_attach_then_detach() {
    let runner = Arc::new(MockRunner::new(vec![
        Ok(String::new()), // attach returns (process exits after detach)
        Ok(String::new()), // detach returns
    ]));
    let (pool, _dir) = test_pool(runner.clone());
    let env = vec![
        ("FLOTILLA_ATTACHABLE_ID".to_string(), "test-uuid".to_string()),
    ];

    pool.ensure_session("test-session", "claude", Path::new("/repo"), &env)
        .await
        .expect("ensure_session");

    let calls = runner.calls();
    assert_eq!(calls.len(), 2, "should call attach then detach: {calls:?}");

    // First call: shpool attach with --cmd
    assert_eq!(calls[0].0, "shpool");
    let attach_args = &calls[0].1;
    assert!(attach_args.contains(&"attach".to_string()), "first call should be attach: {attach_args:?}");
    assert!(attach_args.contains(&"--cmd".to_string()), "attach should have --cmd: {attach_args:?}");
    assert!(attach_args.contains(&"test-session".to_string()), "attach should include session name: {attach_args:?}");

    // The --cmd value should contain the resolved shell (not ${SHELL:-/bin/sh})
    let cmd_idx = attach_args.iter().position(|a| a == "--cmd").expect("--cmd present");
    let cmd_val = &attach_args[cmd_idx + 1];
    assert!(!cmd_val.contains("${SHELL"), "should not contain unresolved shell variable: {cmd_val}");
    assert!(cmd_val.contains("FLOTILLA_ATTACHABLE_ID"), "should contain env var: {cmd_val}");

    // Second call: shpool detach
    assert_eq!(calls[1].0, "shpool");
    let detach_args = &calls[1].1;
    assert!(detach_args.contains(&"detach".to_string()), "second call should be detach: {detach_args:?}");
    assert!(detach_args.contains(&"test-session".to_string()), "detach should include session name: {detach_args:?}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core ensure_session_creates_via_attach 2>&1 | tail -10`
Expected: FAIL — `ensure_session` is still a no-op.

- [ ] **Step 3: Implement `ensure_session`**

In `crates/flotilla-core/src/providers/terminal/shpool.rs`, replace the no-op:

```rust
async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path, env_vars: &TerminalEnvVars) -> Result<(), String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    // Build the --cmd value: env K=V ... /resolved/shell [-lic command]
    // shpool uses the shell-words crate to tokenize --cmd, so no shell
    // expansion occurs — all values must be literal.
    let mut cmd_parts: Vec<String> = Vec::new();
    if !env_vars.is_empty() {
        cmd_parts.push("env".to_string());
        for (k, v) in env_vars {
            cmd_parts.push(format!("{k}={v}"));
        }
    }
    cmd_parts.push(shell);
    if !command.is_empty() {
        cmd_parts.push("-lic".to_string());
        cmd_parts.push(command.to_string());
    }
    let cmd_str = cmd_parts.join(" ");

    let socket_str = self.socket_path.display().to_string();
    let config_str = self.config_path.display().to_string();
    let cwd_str = cwd.display().to_string();

    // Create the session by attaching (shpool creates on first attach)
    run!(
        self.runner,
        "shpool",
        &["--socket", &socket_str, "-c", &config_str, "attach", "--cmd", &cmd_str, "--dir", &cwd_str, session_name],
        Path::new("/")
    )?;

    // Detach to release the session — it keeps running in the shpool daemon
    run!(
        self.runner,
        "shpool",
        &["--socket", &socket_str, "-c", &config_str, "detach", session_name],
        Path::new("/")
    )?;

    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-core ensure_session_creates_via_attach 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Write test — ensure_session with empty command**

```rust
#[tokio::test]
async fn ensure_session_empty_command_starts_login_shell() {
    let runner = Arc::new(MockRunner::new(vec![
        Ok(String::new()),
        Ok(String::new()),
    ]));
    let (pool, _dir) = test_pool(runner.clone());

    pool.ensure_session("test-session", "", Path::new("/repo"), &vec![])
        .await
        .expect("ensure_session");

    let calls = runner.calls();
    let cmd_idx = calls[0].1.iter().position(|a| a == "--cmd").expect("--cmd present");
    let cmd_val = &calls[0].1[cmd_idx + 1];
    // Should be just the resolved shell path, no -lic
    assert!(!cmd_val.contains("-lic"), "empty command should not have -lic: {cmd_val}");
}
```

- [ ] **Step 6: Run test**

Run: `cargo test -p flotilla-core ensure_session_empty_command 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 7: Commit**

```
feat: implement shpool ensure_session — create session via attach + detach
```

---

### Task 5: Implement cleat's `ensure_session` with env vars

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/cleat.rs:57-65`

Cleat's `ensure_session` currently calls `cleat create --cmd <command>`. We need to bake env vars and the resolved shell into the command, matching what cleat's daemon actually executes: `$SHELL -lc <cmd>`. Since cleat resolves `$SHELL` internally, we just need to prefix env vars.

- [ ] **Step 1: Write failing test — ensure_session passes env vars in --cmd**

In `crates/flotilla-core/src/providers/terminal/cleat.rs` tests section:

```rust
#[tokio::test]
async fn ensure_session_includes_env_vars_in_cmd() {
    let json = r#"{"id":"my-session","cwd":"/repo","cmd":"env FOO=bar claude","status":"Detached"}"#;
    let runner = Arc::new(MockRunner::new(vec![Ok(json.into())]));
    let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat");
    let env = vec![("FOO".to_string(), "bar".to_string())];

    pool.ensure_session("my-session", "claude", Path::new("/repo"), &env).await.expect("ensure session");

    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "cleat");
    let cmd_idx = calls[0].1.iter().position(|a| a == "--cmd").expect("--cmd present");
    let cmd_val = &calls[0].1[cmd_idx + 1];
    assert!(cmd_val.starts_with("env "), "should prefix with env: {cmd_val}");
    assert!(cmd_val.contains("FOO=bar"), "should contain env var: {cmd_val}");
    assert!(cmd_val.ends_with("claude"), "should end with command: {cmd_val}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core ensure_session_includes_env 2>&1 | tail -10`
Expected: FAIL — current `ensure_session` passes raw command without env vars.

- [ ] **Step 3: Update cleat's `ensure_session` to inject env vars**

```rust
async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path, env_vars: &TerminalEnvVars) -> Result<(), String> {
    // Build effective command with env vars baked in.
    // Cleat resolves $SHELL internally, so we just need to prefix env vars.
    let effective_cmd = if env_vars.is_empty() {
        command.to_string()
    } else {
        let mut parts = vec!["env".to_string()];
        for (k, v) in env_vars {
            parts.push(format!("{k}={v}"));
        }
        parts.push(command.to_string());
        parts.join(" ")
    };

    let cmd_arg = if effective_cmd.is_empty() { command } else { &effective_cmd };
    run!(
        self.runner,
        &self.binary,
        &["create", "--json", session_name, "--cwd", &cwd.display().to_string(), "--cmd", cmd_arg],
        Path::new("/")
    )?;
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-core ensure_session_includes_env 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Update existing `ensure_creates_session` test**

The existing test passes empty env vars — it should still work as-is since empty env means no prefix. Verify:

Run: `cargo test -p flotilla-core ensure_creates_session 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 6: Commit**

```
feat: inject env vars into cleat ensure_session at create time
```

---

### Task 6: Simplify shpool's `attach_args` to plain reattach

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs:484-513`
- Modify: `crates/flotilla-core/src/providers/terminal/shpool/tests.rs`

- [ ] **Step 1: Write failing test — attach_args produces simple reattach**

In `crates/flotilla-core/src/providers/terminal/shpool/tests.rs`:

```rust
#[test]
fn attach_args_simple_reattach() {
    let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
    let socket = pool.socket_path.display().to_string();
    let config = pool.config_path.display().to_string();
    let env = vec![("FLOTILLA_ATTACHABLE_ID".to_string(), "uuid".to_string())];
    let args = pool.attach_args("test-session", "claude", Path::new("/repo"), &env).expect("attach_args");

    // Should be a simple reattach — no --cmd, no NestedCommand
    assert_eq!(args, vec![
        Arg::Quoted("shpool".into()),
        Arg::Literal("--socket".into()),
        Arg::Quoted(socket),
        Arg::Literal("-c".into()),
        Arg::Quoted(config),
        Arg::Literal("attach".into()),
        Arg::Literal("--force".into()),
        Arg::Literal("--dir".into()),
        Arg::Quoted("/repo".into()),
        Arg::Quoted("test-session".into()),
    ]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core attach_args_simple_reattach 2>&1 | tail -10`
Expected: FAIL

- [ ] **Step 3: Simplify `attach_args`**

Replace the current `attach_args` in `shpool.rs`:

```rust
fn attach_args(&self, session_name: &str, _command: &str, cwd: &Path, _env_vars: &TerminalEnvVars) -> Result<Vec<Arg>, String> {
    Ok(vec![
        Arg::Quoted("shpool".into()),
        Arg::Literal("--socket".into()),
        Arg::Quoted(self.socket_path.display().to_string()),
        Arg::Literal("-c".into()),
        Arg::Quoted(self.config_path.display().to_string()),
        Arg::Literal("attach".into()),
        Arg::Literal("--force".into()),
        Arg::Literal("--dir".into()),
        Arg::Quoted(cwd.display().to_string()),
        Arg::Quoted(session_name.into()),
    ])
}
```

- [ ] **Step 4: Run new test**

Run: `cargo test -p flotilla-core attach_args_simple_reattach 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Replace old attach_args tests with new ones**

Replace the old tests that expected `--cmd` + NestedCommand with tests verifying the simplified reattach. All variations (with/without command, with/without env vars) should produce the same simple structure. Also update `attach_builds_command`.

- [ ] **Step 6: Run all shpool tests**

Run: `cargo test -p flotilla-core shpool 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 7: Commit**

```
feat: simplify shpool attach_args to plain reattach
```

---

### Task 7: Simplify cleat's `attach_args` to plain reattach

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/cleat.rs:67-94`

Since env vars and the command are now baked in at `ensure_session` / `cleat create` time, `attach_args` no longer needs `--cmd` with env + shell wrapping.

- [ ] **Step 1: Write failing test — cleat attach_args is simple reattach**

```rust
#[test]
fn attach_args_simple_reattach() {
    let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat");
    let env = vec![("FOO".to_string(), "bar".to_string())];
    let args = pool.attach_args("my-session", "bash", Path::new("/repo"), &env).expect("attach_args");

    // No --cmd, no NestedCommand — env vars were baked in at create time
    assert_eq!(args, vec![
        Arg::Quoted("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Quoted("my-session".into()),
        Arg::Literal("--cwd".into()),
        Arg::Quoted("/repo".into()),
    ]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core attach_args_simple_reattach 2>&1 | tail -10`
Expected: FAIL — current `attach_args` still builds `--cmd` with NestedCommand.

- [ ] **Step 3: Simplify cleat's `attach_args`**

```rust
fn attach_args(&self, session_name: &str, _command: &str, cwd: &Path, _env_vars: &TerminalEnvVars) -> Result<Vec<Arg>, String> {
    Ok(vec![
        Arg::Quoted(self.binary.clone()),
        Arg::Literal("attach".into()),
        Arg::Quoted(session_name.into()),
        Arg::Literal("--cwd".into()),
        Arg::Quoted(cwd.display().to_string()),
    ])
}
```

- [ ] **Step 4: Run new test**

Run: `cargo test -p flotilla-core attach_args_simple_reattach 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Replace old cleat attach_args tests**

Replace all old tests that expected `--cmd` + `${SHELL:-/bin/sh}` + NestedCommand with tests verifying the simplified structure. Key tests to rewrite:
- `attach_args_with_command_no_env` → simple reattach
- `attach_args_flatten_with_command_no_env` → flat string without --cmd
- `attach_args_empty_command_no_env` → same simple structure
- `attach_args_with_env_vars` → env vars ignored in attach_args
- `attach_args_with_env_vars_empty_command` → same
- `attach_args_flatten_roundtrip_env_vars` → remove or simplify
- `attach_wraps_command` → update to verify no --cmd

- [ ] **Step 6: Run all cleat tests**

Run: `cargo test -p flotilla-core cleat 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 7: Commit**

```
feat: simplify cleat attach_args to plain reattach
```

---

### Task 8: Update hop chain snapshots and remaining tests

**Files:**
- Modify: `crates/flotilla-core/src/hop_chain/tests.rs`
- Modify: `crates/flotilla-core/src/hop_chain/snapshots/*.snap`
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Run full test suite to find failures**

Run: `cargo test --workspace --locked 2>&1 | grep 'FAILED\|failures'`

Identify all failing tests. The hop chain snapshots for local workspace (which use cleat) will change. Executor tests that check for `--cmd` may need updating.

- [ ] **Step 2: Fix each failing test**

For each failure, investigate whether the change is an intended consequence of the new behavior. If yes, update the test/snapshot. If no, investigate the bug.

**Remember:** Never blindly accept snapshot changes. Each changed snapshot should be explainable by the design change. The `e2e_local_workspace_flattened` snapshot should change from `'cleat' attach 'main__shell__0' --cwd '/home/alice/dev/my-repo'` to the same (actually this one was already simple — verify it stays unchanged).

- [ ] **Step 3: Run full test suite**

Run: `cargo test --workspace --locked 2>&1 | tail -10`
Expected: all pass

- [ ] **Step 4: Run CI checks**

Run: `cargo +nightly-2026-03-12 fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no errors

- [ ] **Step 5: Commit**

```
test: update snapshots and tests for ensure_session model
```

---

### Task 9: Remove `${SHELL:-/bin/sh}` from both shpool and cleat

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`
- Modify: `crates/flotilla-core/src/providers/terminal/shpool/tests.rs`
- Modify: `crates/flotilla-core/src/providers/terminal/cleat.rs`

- [ ] **Step 1: Verify no remaining references to `${SHELL:-/bin/sh}` in shpool and cleat code**

Run: `grep -rn 'SHELL:-' crates/flotilla-core/src/providers/terminal/`
Expected: no matches (all removed by Tasks 4-7)

If any remain, remove them.

- [ ] **Step 2: Run full test suite**

Run: `cargo test --workspace --locked 2>&1 | tail -10`
Expected: all pass

- [ ] **Step 3: Commit (if changes were made)**

```
chore: remove residual ${SHELL:-/bin/sh} references from terminal pools
```
