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
3. Check liveness with `libc::kill(pid, 0)` (signal 0 = existence check, no signal sent).
4. If the process is dead, remove both the socket and pid file.
5. If no pid file exists but the socket file exists, remove the socket (can't verify liveness).
6. If the process is alive, do nothing — the daemon is healthy.

Uses raw `libc::kill` for the pid check (libc is already a transitive dependency). No shpool commands needed. Gate the entire stale socket cleanup with `#[cfg(unix)]` since this is a Unix-specific issue.

## #243 — Explicit daemon start with flotilla config

### Problem

Flotilla passes `-c <config>` to all shpool commands, but this only configures the daemon if shpool auto-starts one. If a daemon is already running (from a previous session or manual start), it keeps its old config. This means daemon-side settings like `prompt_prefix = ""` don't take effect.

### Design

`shpool daemon` runs in the foreground (blocks). The auto-daemonization (fork to background) happens inside `shpool attach`, not `shpool daemon`. So the approach is:

1. After stale socket cleanup (#244), check whether a daemon is already running by checking socket existence and pid liveness.
2. If no daemon is running, spawn `shpool --socket <path> -c <config> daemon` as a detached background process using `tokio::process::Command` with stdout/stderr redirected to the log file.
3. If a daemon IS already running (survived #244's stale check, so it's alive), kill it and respawn with the current config. This ensures daemon-side settings like `prompt_prefix` are always up to date.
4. Wait briefly for the socket to appear (poll with short sleep, timeout after ~2s).

Since this requires async operations, change `ShpoolTerminalPool::new()` to an async factory method `ShpoolTerminalPool::create()`. The call site in `discovery.rs` is already async.

If spawn fails, log a warning but don't fail discovery — shpool's built-in auto-daemonize from `attach` will still work as a fallback.

**Scope note:** Killing a running daemon to apply config is acceptable because flotilla owns this daemon instance (dedicated socket path). Active sessions are disrupted but will reconnect on next attach.

## #237 — Clean up managed terminals when checkout is removed

### Problem

`kill_terminal` is implemented on `ShpoolTerminalPool` but has zero call sites. When a checkout is removed, associated shpool sessions persist indefinitely.

### Design

Use already-correlated work item data to identify terminals to clean up, rather than re-deriving the association in the executor.

**Protocol changes:**

1. Add `terminal_keys: Vec<ManagedTerminalId>` to `WorkItem` in `flotilla-protocol/src/snapshot.rs`. Use `ManagedTerminalId` (not `String`) for type safety — it already derives `Serialize`/`Deserialize`/`Clone`. Add `#[serde(default)]` for forward compatibility with existing snapshots.
2. Add `terminal_keys: Vec<ManagedTerminalId>` to `Command::RemoveCheckout` in `flotilla-protocol/src/commands.rs`.

**Correlation changes (flotilla-core):**

3. Add `terminal_ids: Vec<ManagedTerminalId>` to `CorrelatedWorkItem` in `data.rs`. In `group_to_work_item()`, for items with `CorItemKind::ManagedTerminal`: match `ProviderItemKey::ManagedTerminal(key)` to get the string key, then `providers.managed_terminals.get(&key)` to get the `ManagedTerminal`, then `.id.clone()` to get the `ManagedTerminalId`.

**Naming convention:** Core types use `terminal_ids` (the actual struct), protocol/UI types use `terminal_keys` (following the existing `*_key`/`*_keys` convention in `WorkItem`).

**Protocol conversion (flotilla-core):**

4. In `convert.rs`, map `terminal_ids` from `CorrelatedWorkItem` to `terminal_keys` on protocol `WorkItem`.

**Two-phase delete flow (flotilla-tui):**

The `RemoveCheckout` intent resolves to `Command::FetchCheckoutStatus` first, which transitions to `UiMode::DeleteConfirm`. The actual `Command::RemoveCheckout` is constructed later in `handle_delete_confirm_key()`. Terminal keys must flow through this two-phase process:

5. Add `terminal_keys: Vec<ManagedTerminalId>` to `UiMode::DeleteConfirm` variant.
6. In TUI executor result handling, when `CheckoutStatus` arrives and transitions to `DeleteConfirm`, carry the `terminal_keys` from the current work item into the mode.
7. In `handle_delete_confirm_key()`, include `terminal_keys` from the `DeleteConfirm` mode in the `Command::RemoveCheckout`.

**Executor (flotilla-core):**

8. In `executor.rs`, `RemoveCheckout` handler: after successful checkout removal, iterate `terminal_keys` and call `terminal_pool.kill_terminal()` for each. Best-effort — log errors but don't fail the command.

This approach means if correlation rules change (e.g. terminals correlate by checkout path instead of branch name), cleanup automatically follows.

## Testing

- **#244**: Unit test `clean_stale_socket` with a fake pid file pointing to a dead pid, verify socket removal. Test the "no pid file but socket exists" case.
- **#243**: Test that `create()` spawns a daemon process. The spawn is fire-and-forget so the main test is that it doesn't fail/hang.
- **#237**: Executor test that `RemoveCheckout` with `terminal_keys` calls `kill_terminal` for each key. Correlation test that `CorrelatedWorkItem` built from a group with managed terminals has populated `terminal_ids`. Snapshot/serialization roundtrip test for `ManagedTerminalId` in `WorkItem`.

## Files changed

| File | Change |
|------|--------|
| `crates/flotilla-protocol/src/snapshot.rs` | Add `terminal_keys: Vec<ManagedTerminalId>` to `WorkItem` |
| `crates/flotilla-protocol/src/commands.rs` | Add `terminal_keys: Vec<ManagedTerminalId>` to `Command::RemoveCheckout` |
| `crates/flotilla-core/src/providers/terminal/shpool.rs` | Stale socket cleanup, async `create()`, background daemon spawn |
| `crates/flotilla-core/src/providers/discovery.rs` | Call `create()` instead of `new()` |
| `crates/flotilla-core/src/data.rs` | Add `terminal_ids` to `CorrelatedWorkItem`, populate in `group_to_work_item()` |
| `crates/flotilla-core/src/convert.rs` | Map `terminal_ids` → `terminal_keys` in core-to-protocol conversion |
| `crates/flotilla-core/src/executor.rs` | Kill terminals after checkout removal |
| `crates/flotilla-tui/src/app/ui_state.rs` | Add `terminal_keys` to `UiMode::DeleteConfirm` |
| `crates/flotilla-tui/src/app/executor.rs` | Carry `terminal_keys` from work item into `DeleteConfirm` mode |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Include `terminal_keys` in `Command::RemoveCheckout` |
| `crates/flotilla-tui/src/app/intent.rs` | No change needed (resolves to `FetchCheckoutStatus`, not `RemoveCheckout`) |
