# Remoteable Daemon Startup

## Problem

The shpool terminal pool provider manages its daemon lifecycle through direct local operations: `tokio::process::Command::spawn()`, `std::fs` file I/O, `libc::kill` signals, and `sysinfo` process lookups. None of these go through the `CommandRunner` abstraction, so they fail when a daemon manages a remote host or Docker sandbox via `EnvironmentRunner`.

Other providers (cleat, cmux, tmux, zellij) avoid this problem by delegating everything to their tool's CLI through `CommandRunner`. Shpool duplicates ~250 lines of daemon lifecycle machinery that shpool itself already provides via auto-daemonize.

## Design

Two changes: add `ensure_file` to `CommandRunner`, and simplify the shpool provider to rely on shpool's built-in auto-daemonize.

### 1. `CommandRunner::ensure_file`

Add a method to write a file with content, creating parent directories as needed:

```rust
async fn ensure_file(&self, path: &Path, content: &str) -> Result<(), String>;
```

**`ProcessCommandRunner` (local):** `create_dir_all(parent)` then `fs::write(path, content)`.

**`EnvironmentRunner` (Docker/remote):** Delegates to the inner runner: `sh -c "mkdir -p '<parent>' && printf '%s' '<content>' > '<path>'"`.

**`MockRunner` (tests):** Records the call (path + content) for assertion. Succeeds by default.

**Replay:** Recorded as a `command` channel interaction, like any other runner call.

This is a general-purpose primitive. Any provider that needs to place a config file in the execution environment can use it.

### 2. Simplified shpool provider

**`create()` reduces to:**
1. Call `ensure_file` to write the managed config to `{state_dir}/shpool/config.toml`.
2. Return the provider. No daemon probing, no PID tracking, no socket cleanup.

**Daemon startup happens naturally.** The first `ensure_session` call passes `--socket` and `-c` to shpool. If no daemon is running, shpool auto-daemonizes: it spawns a background daemon with the same socket and config flags, writes its own PID file and log file, polls until the socket is ready, then proceeds with the attach. All through `CommandRunner`.

**Config changes take effect on next natural daemon restart.** No restart-on-config-change logic. The managed config controls `prompt_prefix` (disabled) and `forward_env` (terminal variables). Neither justifies killing existing sessions — `forward_env` only affects new sessions anyway.

### Removed code (~250 lines)

| Removed | Reason |
|---------|--------|
| `start_daemon()` | Shpool auto-daemonizes from `attach` |
| `stop_daemon()` | No config-restart, no manual lifecycle |
| `detect_daemon_state()` | No daemon probing needed |
| `clean_stale_socket()` | Shpool handles stale sockets in its own `maybe_fork_daemon` |
| `is_process_alive()`, `is_expected_process()` | No PID-based liveness checks |
| `probe_daemon_without_pid_file()`, `probe_failure_is_definitely_stale()` | No daemon health probing |
| `ShpoolDaemonState`, `ShpoolNoPidProbe` enums | No daemon state machine |
| Config-restart logic (two-phase temp file dance) | Config changes wait for natural restart |
| `config_needs_update()`, `write_config()` | Replaced by single `ensure_file` call |

### Kept as-is

`ensure_session()`, `list_sessions()`, `kill_session()`, `attach_args()`, `session_exists()`, `parse_list_json()` — these already go through `CommandRunner` and are fully remoteable.

## Dependencies

Check whether `sysinfo` is used elsewhere in the crate before removing it. `libc` likely stays (used outside shpool).

## Risk

Shpool's auto-daemonize has a known macOS quirk: `connect()` to a stale Unix socket succeeds (unlike Linux where it returns `ConnectionRefused`). Shpool's own `maybe_fork_daemon` attempts to handle this, but flotilla previously had more robust PID-based cleanup. If shpool's handling has a gap, the first `ensure_session` after a crash could fail. This is acceptable — cleat is the long-term terminal pool target, and shpool is the bridge.

## Scope

**Changes:**
- `flotilla-core`: `CommandRunner` trait, `ProcessCommandRunner`, `EnvironmentRunner`, `MockRunner`, replay infra
- `flotilla-core`: `ShpoolTerminalPool` — gut lifecycle, simplify `create()`
- `flotilla-core`: shpool factory test (already passes `DiscoveryMockRunner`)

**No changes:** protocol, TUI, daemon server, CLI. The `TerminalPool` trait interface is unchanged.
