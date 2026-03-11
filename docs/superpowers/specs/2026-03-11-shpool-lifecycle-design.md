# Shpool Daemon Lifecycle: Issues #243, #244, #237

Three related fixes to shpool daemon startup, stale socket handling, and terminal cleanup on checkout removal.

## Context

Flotilla manages a shpool daemon with a dedicated socket at `~/.config/flotilla/shpool/shpool.socket` and a managed config at `~/.config/flotilla/shpool/config.toml`. Currently:

- The daemon auto-starts on first `attach`, which means a pre-existing daemon keeps its old config (#243).
- On macOS, `connect()` to a stale Unix socket succeeds (unlike Linux), so shpool's auto-daemonize thinks a dead daemon is alive (#244).
- `RemoveCheckout` doesn't clean up associated shpool sessions (#237).

## #244 — Stale socket cleanup (macOS workaround)

### Problem

When the shpool daemon dies, its socket file persists. On macOS, `connect()` to a stale socket succeeds, causing shpool's auto-daemonize to think a daemon is running. The subsequent attach fails with "control socket never came up".

### Design

Add a `clean_stale_socket` helper that runs during `ShpoolTerminalPool` initialization, before daemon start:

1. Derive the pid file path: `socket_path.with_file_name("daemonized-shpool.pid")`.
2. Read and parse the pid.
3. Check liveness with `kill(pid, 0)` (signal 0 = existence check, no signal sent).
4. If the process is dead, remove both the socket and pid file.
5. If no pid file exists but the socket file exists, remove the socket (can't verify liveness).
6. If the process is alive, do nothing — the daemon is healthy.

Uses `nix::sys::signal::kill` or raw `libc::kill` for the pid check. No shpool commands needed.

## #243 — Explicit daemon start with flotilla config

### Problem

Flotilla passes `-c <config>` to all shpool commands, but this only configures the daemon if shpool auto-starts one. If a daemon is already running (from a previous session or manual start), it keeps its old config. This means daemon-side settings like `prompt_prefix = ""` don't take effect.

### Design

After stale socket cleanup, explicitly start the daemon in `ShpoolTerminalPool` initialization:

1. Run `shpool --socket <path> -c <config> daemon`.
2. Shpool's `daemon` command forks to background and returns. If a healthy daemon is already running on the socket, shpool detects this and exits cleanly (no-op).
3. This ensures the daemon always runs with flotilla's managed config.

Since this requires running a command (async), change `ShpoolTerminalPool::new()` from sync to an async factory method `ShpoolTerminalPool::create()`. The call site in `discovery.rs` is already async.

If the daemon command fails, log a warning but don't fail discovery — the existing graceful degradation (empty terminal list on connection errors) still applies.

## #237 — Clean up managed terminals when checkout is removed

### Problem

`kill_terminal` is implemented on `ShpoolTerminalPool` but has zero call sites. When a checkout is removed, associated shpool sessions persist indefinitely.

### Design

Use already-correlated work item data to identify terminals to clean up, rather than re-deriving the association in the executor.

**Protocol changes:**

1. Add `terminal_keys: Vec<String>` to `WorkItem` in `flotilla-protocol/src/snapshot.rs`.
2. Add `terminal_keys: Vec<String>` to `Command::RemoveCheckout` in `flotilla-protocol/src/commands.rs`.

**Correlation changes (flotilla-core):**

3. In `data.rs`, when building `WorkItem` from `CorrelationResult`, collect `ManagedTerminal` source keys from the correlation group and populate `terminal_keys`.

**Intent resolution (flotilla-tui):**

4. In `intent.rs`, `RemoveCheckout` resolution copies `terminal_keys` from the `WorkItem` into the `Command::RemoveCheckout`.

**Executor (flotilla-core):**

5. In `executor.rs`, `RemoveCheckout` handler: after successful checkout removal, iterate `terminal_keys` and call `terminal_pool.kill_terminal()` for each. Best-effort — log errors but don't fail the command.

This approach means if correlation rules change (e.g. terminals correlate by checkout path instead of branch name), cleanup automatically follows.

## Testing

- **#244**: Unit test `clean_stale_socket` with a fake pid file pointing to a dead pid, verify socket removal.
- **#243**: The daemon start is a side effect during discovery. Test that `create()` succeeds when shpool binary exists. Integration test (if shpool available) that daemon is running after create.
- **#237**: Executor test that `RemoveCheckout` with `terminal_keys` calls `kill_terminal` for each key. Correlation test that `WorkItem` built from a group with managed terminals has populated `terminal_keys`.

## Files changed

| File | Change |
|------|--------|
| `crates/flotilla-protocol/src/snapshot.rs` | Add `terminal_keys` to `WorkItem` |
| `crates/flotilla-protocol/src/commands.rs` | Add `terminal_keys` to `Command::RemoveCheckout` |
| `crates/flotilla-core/src/providers/terminal/shpool.rs` | Stale socket cleanup, async `create()`, explicit daemon start |
| `crates/flotilla-core/src/providers/discovery.rs` | Call `create()` instead of `new()` |
| `crates/flotilla-core/src/data.rs` | Populate `terminal_keys` on `WorkItem` |
| `crates/flotilla-core/src/convert.rs` | Map `terminal_keys` in core-to-protocol conversion |
| `crates/flotilla-core/src/executor.rs` | Kill terminals after checkout removal |
| `crates/flotilla-tui/src/app/intent.rs` | Pass `terminal_keys` through `RemoveCheckout` command |
| `Cargo.toml` (flotilla-core) | Add `libc` or `nix` dependency for pid check |
