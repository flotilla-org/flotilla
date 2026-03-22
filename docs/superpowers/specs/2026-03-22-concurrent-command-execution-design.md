# Concurrent Command Execution

Addresses a regression introduced by the all-symbolic step execution refactor (batch 2): every command now routes through the single-slot `active_command` gate in `InProcessDaemon`, blocking lightweight commands (open issue, fetch status) while a long-running command (checkout, teleport) is active.

## Problem

Before batch 2, `build_plan()` returned `ExecutionPlan::Immediate` for lightweight commands. `InProcessDaemon` ran those outside the single-slot gate — they fired immediately regardless of what else was running. After batch 2, every command returns `Ok(StepPlan)` and enters the gate. A running checkout now blocks `OpenIssue`.

The single-slot gate was introduced for cancellation support — `Esc` needs to target "the running command." A single slot made that trivial. But the gate was never a correctness mechanism: the resolver holds `Arc` clones, `providers_data` is a snapshot, and the `attachable_store` is already behind a `Mutex`. Two concurrent commands don't corrupt Rust-level shared state. Concurrent mutating commands targeting the same repo (e.g., two checkouts) could conflict at the filesystem/git level, but that's a provider-level concern — the executor gate is not the right place to prevent it.

The daemon server's forwarded-command path (`spawn_forwarded_command` in `remote_commands.rs`) already runs commands concurrently via `tokio::spawn`. Local commands dispatched through the daemon server still route through `InProcessDaemon` and hit the single-slot gate, so the inconsistency exists within the daemon server itself — not between daemon types.

## Design

### Daemon: Replace single-slot with concurrent command map

Replace `active_command: Arc<Mutex<Option<ActiveCommand>>>` with `active_commands: Arc<Mutex<HashMap<u64, CancellationToken>>>`.

Drop the rejection logic. Every step plan runs in its own `tokio::spawn`. On entry, insert the command's `(id, token)` into the map. On completion, remove it unconditionally (the current code has a conditional guard — `if guard.command_id == id` — which becomes unnecessary with a map keyed by ID). `cancel(command_id)` looks up the token by ID and cancels it.

Three sites change in `in_process.rs`:

1. **Registration (before `run_step_plan`):** Replace the check-and-reject with an unconditional insert.
2. **Cleanup (after `run_step_plan`):** Replace the conditional `Option` clear with `HashMap::remove(&id)`.
3. **`cancel()` method:** Replace `Option` match with `HashMap::get(&command_id)`.

```rust
// Registration — no rejection, just insert:
{
    let mut guard = active_ref.lock().await;
    guard.insert(id, token.clone());
}

// Cleanup — unconditional remove:
{
    let mut guard = active_ref.lock().await;
    guard.remove(&id);
}

// cancel():
async fn cancel(&self, command_id: u64) -> Result<(), String> {
    let guard = self.active_commands.lock().await;
    match guard.get(&command_id) {
        Some(token) => { token.cancel(); Ok(()) }
        None => Err("no matching active command".into()),
    }
}
```

The `ActiveCommand` struct is no longer needed — the map key carries the command ID and the value carries the token.

### TUI: Esc cancels most recent command (stack order)

Change `dismiss()` in `repo_page.rs` from `in_flight.keys().next()` (arbitrary — `HashMap` does not guarantee insertion order) to `in_flight.keys().max().copied()` (highest command ID = most recently started). Command IDs are monotonically increasing (`AtomicU64`), so max ID is always the most recent.

This gives stack/LIFO behavior: Esc cancels the top command. When it finishes (or is cancelled), the next most recent becomes the top. No special logic needed — once the cancelled command's `CommandFinished` event removes it from `in_flight`, the next `max()` naturally becomes the target.

### Status bar: show most recent command for active repo

The status bar's `active_task()` filters `in_flight` to the active repo, then shows the first command's description plus a `(+N)` count suffix when multiple commands are active. Update it to show the most recent command (highest ID among those for the active repo) and preserve the `(+N)` suffix. This matches what Esc would cancel — the user sees what they'd be cancelling.

### What does NOT change

- `DaemonHandle` trait — `execute()` and `cancel()` signatures stay the same
- `DaemonEvent::CommandStarted`/`CommandFinished` — unchanged
- Command ID generation — still `AtomicU64` per daemon, still local scope
- `run_step_plan` — unchanged, each plan runs independently with its own `CancellationToken`
- TUI's `in_flight: HashMap<u64, InFlightCommand>` — unchanged, already supports multiple

### Follow-up (not this work)

- Surfacing multiple running commands in the TUI (palette, commands tab, or status bar). When multiple commands are visible, Esc should only work on an explicitly selected command rather than auto-falling-through.
- Global command IDs across multi-host daemons.
