# Terminal Manager: Extract Terminal Identity from Providers

## Problem

Terminal providers (cleat, shpool) do two jobs: talk to CLIs and manage terminal identity in the attachable store. These concerns are tangled together, duplicated across providers, and complicated by `ManagedTerminalId` — a type that predates `AttachableId` and encodes the same information as `TerminalPurpose`.

The current flow:

1. The executor constructs a `ManagedTerminalId` (checkout + role + index) from the workspace template.
2. `terminal_session_binding_ref()` encodes it as a session name string (`flotilla/{checkout}/{role}/{index}`).
3. The provider talks to the CLI, then constructs a `TerminalPurpose` (same three fields) and calls `ensure_terminal_attachable_with_change` to allocate an `AttachableId` as a side effect.
4. `build_terminal_env_vars` in the executor does the *same* store mutation redundantly.
5. On refresh, the provider reconciles live sessions against store bindings — logic duplicated between cleat and shpool.

`ManagedTerminalId` serves as both a human-readable descriptor and an identity key, but the real stable identity should be `AttachableId`.

## Design

### AttachableId as the sole terminal identity

`AttachableId` becomes the terminal's identity everywhere: in commands, snapshots, session names, and store lookups. `ManagedTerminalId` is removed from the protocol. Human-readable information (checkout, role, command, status) lives on the attachable content, not on the ID.

The AttachableId doubles as the session name passed to cleat/shpool. Since flotilla always creates a private terminal pool (dedicated socket/daemon), every session in the pool belongs to us. No prefix or naming convention is needed, and orphan detection logic disappears.

Terminal bindings are eliminated. Bindings existed to map an external session ref to an AttachableId. When the session name *is* the AttachableId, that mapping is an identity function. Attachables are looked up directly by ID. (Workspace manager bindings remain — they map workspace refs to sets.)

### Simplified TerminalPool trait

The trait becomes a pure CLI adapter. No store, no `ManagedTerminalId`, no `AttachableId` awareness:

```rust
pub struct TerminalSession {
    pub session_name: String,
    pub status: TerminalStatus,
    pub command: Option<String>,
    pub working_directory: Option<PathBuf>,
}

#[async_trait]
pub trait TerminalPool: Send + Sync {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String>;
    async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path) -> Result<(), String>;
    async fn attach_command(&self, session_name: &str, command: &str, cwd: &Path, env_vars: &TerminalEnvVars) -> Result<String, String>;
    async fn kill_session(&self, session_name: &str) -> Result<(), String>;
}
```

Cleat and shpool implementations become pure CLI wrappers — parse output, shell out commands. The passthrough provider remains trivially simple.

### TerminalManager

A new module at `flotilla-core/src/terminal_manager.rs`. Owns the `SharedAttachableStore` for terminal concerns and wraps a `dyn TerminalPool`.

```rust
pub struct TerminalManager {
    pool: Arc<dyn TerminalPool>,
    store: SharedAttachableStore,
}
```

**Operations:**

- **`allocate_set(host, checkout) -> AttachableSetId`** — Creates a new AttachableSet in the store.

- **`allocate_terminal(set_id, role, command, cwd) -> AttachableId`** — Creates a new Attachable within the set. The AttachableId is used directly as the session name when talking to the pool.

- **`ensure_running(attachable_id) -> Result<()>`** — Reads command/cwd from the stored attachable, calls `pool.ensure_session()`.

- **`attach_command(attachable_id, daemon_socket_path) -> Result<String>`** — Reads command/cwd from the attachable, builds env vars, calls `pool.attach_command()`.

- **`kill_terminal(attachable_id) -> Result<()>`** — Calls `pool.kill_session()` with the AttachableId as session name.

- **`refresh() -> Vec<TerminalInfo>`** — Calls `pool.list_sessions()`, matches session names to AttachableIds by direct lookup, updates statuses, emits disconnected entries for known attachables absent from the live list.

- **`cascade_delete(checkout_paths)`** — Removes sets for the given checkouts and kills their sessions.

### Factory changes

The `Factory::probe` trait method currently takes `SharedAttachableStore`. Only cleat and shpool factories use it (to pass into the provider constructor). Since providers no longer hold the store, `attachable_store` is removed from the `Factory::probe` signature. Every factory that currently takes `_attachable_store` drops the parameter.

## Impact

| Area | Change |
|------|--------|
| `TerminalPool` trait | Simplified to session-name-based operations, no store |
| cleat/shpool providers | Pure CLI adapters, drop all store/reconciliation code |
| passthrough provider | Unchanged in spirit, drops `ManagedTerminalId` from signatures |
| `TerminalManager` (new) | Owns store + pool; allocation, reconciliation, env vars, cascade delete |
| `ManagedTerminalId` | Removed from protocol |
| `ManagedTerminal` | Replaced — terminals are attachables with `TerminalContent` |
| `terminal_session_binding_ref` / `parse_*` | Deleted |
| Terminal bindings | Eliminated — AttachableId is the session name |
| `Factory::probe` | Drops `attachable_store` parameter |
| `executor/terminals.rs` | Delegates to TerminalManager |
| `build_terminal_env_vars` | Moves into TerminalManager |
| `cascade_delete_attachable_sets` | Moves into TerminalManager |
| `refresh.rs` | Calls `terminal_manager.refresh()` instead of `tp.list_terminals()` + `project_attachable_data` |
| `ProviderData.managed_terminals` | Derived from attachables, or removed |
| Snapshot/protocol | `ManagedTerminalId` references become `AttachableId` |
| Commands (`RemoveCheckout`) | `terminal_keys: Vec<ManagedTerminalId>` becomes `terminal_keys: Vec<AttachableId>` |

## Testing

The TerminalManager is testable in isolation: inject a mock `TerminalPool` (returns canned `TerminalSession` lists) and an in-memory attachable store. Tests verify:

- `allocate_set` + `allocate_terminal` create the right store entries
- `ensure_running` and `attach_command` delegate to the pool with the AttachableId as session name
- `refresh` reconciles live sessions against stored attachables (status updates, disconnected detection)
- `cascade_delete` removes sets and kills sessions

Provider tests become simpler — cleat/shpool tests verify CLI parsing and command construction without any store setup.
