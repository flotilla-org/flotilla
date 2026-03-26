# Stable Workspace Identity for cmux, zellij, and tmux

## Problem

Workspace manager bindings use workspace refs as keys in the attachable registry. All three workspace providers use unstable identifiers that can be reused or change, causing stale bindings to associate workspaces with the wrong repo.

**cmux:** Positional refs (`workspace:N`) get reused when workspaces are destroyed and recreated. Observed symptom: the cleat repo's main checkout was associated with a flotilla worktree workspace because the stale binding from a deleted cleat workspace matched a new flotilla workspace that reused the same ref number.

**zellij:** Tab names (used as ws_ref) can be renamed by the user, and duplicate names are possible. The current `query-tab-names` command returns only names with no stable identity.

**tmux:** Window names (used as ws_ref) can be renamed and duplicated. The current `list-windows -F #{window_name}` returns only names.

## Solution

Switch all three providers to use stable identifiers as their canonical workspace identity (ws_ref), and simplify the workspace data model by removing `directories` and the local TOML state files.

### Stable identifiers

- **cmux:** Use cmux's stable UUIDs, exposed via `--id-format uuids`. UUIDs are globally unique and never reused.
- **zellij:** Use `{session_name}:{tab_id}` where `tab_id` comes from the new `list-tabs --json` output. The `tab_id` is stable within a zellij session. Prefixing with the session name prevents cross-session collisions.
- **tmux:** Use `{start_time}:{session_name}:@{window_id}` where `start_time` is the tmux server's `#{start_time}` (Unix epoch), `session_name` is `#{session_name}`, and `window_id` is `#{window_id}` (format `@N`). Window IDs are monotonically increasing and never reused within a server instance. Including `start_time` first ensures all bindings from a dead server share a common prefix, enabling future prefix-based invalidation when the server restarts.

### Remove `directories` from `Workspace`

The `Workspace.directories` field is only consumed in one place: `select_existing_workspace()`, which lists all workspaces and matches by directory path to avoid creating duplicates. This approach is broken for remote workspaces (directories are local paths that don't exist on the presentation host), and requires tmux/zellij to maintain local TOML state files just to track which directory each workspace was created for.

The correct model is: "does an attachable set exist for this checkout, and does a workspace binding already point to that set?" This is entirely answerable from the binding system.

### Rewrite `select_existing_workspace` via bindings

Replace the current directory-matching approach with a binding-based lookup:

1. `sets_for_checkout(checkout_path)` — find the attachable set(s) for this checkout
2. Reverse-scan bindings for `(workspace_manager, provider_name, AttachableSet, *)` where `object_id == set_id` — find the ws_ref
3. If found, `select_workspace(ws_ref)` and return

This requires a small addition to the store API: a reverse binding lookup (set_id → external_ref) for a given provider category/name. Alternatively, scan the bindings list inline — it's small.

This works for both local and remote workspaces.

### Drop TOML state files

The tmux and zellij providers maintain local state files (`{state_dir}/tmux/{session}/state.toml` and `{state_dir}/zellij/{session}/state.toml`) keyed by window/tab name. These exist solely to provide `directories` for `select_existing_workspace`. With both removed, the state files are unnecessary. All three providers become stateless — identity and state come from the multiplexer and the attachable binding system.

## Scope

Changes to workspace providers, the `Workspace` struct, and `select_existing_workspace`. The broader attachable set lifecycle (stale binding cleanup, orphaned sets) is out of scope.

## Changes

### `Workspace` struct (protocol)

Remove the `directories` field. The struct retains `name`, `correlation_keys`, and `attachable_set_id`.

### `CmuxWorkspaceManager` (cmux.rs)

**`list_workspaces()`:** Pass `--id-format uuids` to `list-workspaces`. Update `parse_workspaces()` to read the `id` field (UUID) instead of `ref` as the ws_ref. Stop populating `directories`.

**`create_workspace()`:** The `new-workspace` command returns `OK workspace:N` regardless of id-format flags. After creation, issue a follow-up `list-workspaces --id-format both` call, match by the returned positional ref to find the UUID, and return the UUID as the ws_ref.

**`select_workspace()`:** No change — cmux accepts UUIDs for `--workspace` arguments.

### `ZellijWorkspaceManager` (zellij.rs)

**`list_workspaces()`:** Replace `query-tab-names` with `list-tabs --json`. Parse each tab's `tab_id` and `name`. Return ws_ref as `{session_name}:{tab_id}`. Use the `name` field for `Workspace.name`.

**`create_workspace()`:** `new-tab` now returns the tab_id to stdout. Construct ws_ref as `{session_name}:{tab_id}`.

**`select_workspace()`:** Parse the tab_id from the ws_ref (after the `:`), call `go-to-tab-by-id {tab_id}` instead of `go-to-tab-name`.

**Remove:** `ZellijState`, `TabState`, `load_state()`, `save_state()`, `state_path()`, `state_dir` field, and associated tests.

### `TmuxWorkspaceManager` (tmux.rs)

**`list_workspaces()`:** Fetch `#{start_time}`, `#{session_name}`, `#{window_id}`, and `#{window_name}` via `list-windows -F`. Return ws_ref as `{start_time}:{session_name}:@{window_id}`. Use `#{window_name}` for `Workspace.name`.

**`create_workspace()`:** Use `new-window -P -F '#{window_id}'` to capture the new window's ID directly from stdout. Query `#{start_time}` and `#{session_name}` to construct the full ws_ref.

**`select_workspace()`:** Parse the `@N` window ID from the ws_ref (after the last `:`), call `select-window -t @N`.

**Remove:** `TmuxState`, `WindowState`, `load_state()`, `save_state()`, `state_path()`, `state_dir` field, and associated tests.

### `WorkspaceOrchestrator` (executor/workspace.rs)

**`select_existing_workspace()`:** Replace directory-matching with binding-based lookup:
1. Lock the attachable store
2. `sets_for_checkout()` to find the set for this checkout
3. `lookup_workspace_ref_for_set()` to find the ws_ref bound to that set
4. Call `select_workspace(ws_ref)`
5. **On failure, fall through to create.** If the bound workspace no longer exists (dead tmux server, closed cmux workspace), `select_workspace` will fail. Log the error and proceed to create a new workspace, which writes a fresh binding replacing the stale one. This preserves the current fallback behavior.

This removes the need to call `ws_mgr.list_workspaces()` and removes the `checkout_path` parameter in favor of the checkout's `HostPath`.

### `AttachableStoreApi` (store.rs)

**Reverse binding lookup:**
```rust
fn lookup_workspace_ref_for_set(
    &self,
    provider_category: &str,
    provider_name: &str,
    set_id: &AttachableSetId,
) -> Option<String>;
```

Scans bindings where `object_kind == AttachableSet` and `object_id == set_id`, returns the `external_ref`. Linear scan over a small list.

**1:1 binding invariant for workspace→set:** When `persist_workspace_binding` writes a new binding (ws_ref → set_id), it must also remove any existing workspace binding for the same set_id (same provider category/name). The current `replace_binding` is keyed by `external_ref`, so after a workspace is recreated with a new stable ID, the old binding persists alongside the new one. Without cleanup, the reverse lookup could return the stale ref. Enforcing 1:1 (one workspace per attachable set per provider) ensures the reverse lookup is unambiguous.

### `WorkspaceManager` trait (workspace/mod.rs)

Add a method to declare the scope of `list_workspaces()`:

```rust
/// Returns a prefix that all ws_refs from this provider instance will
/// start with. Only bindings matching this prefix should be considered
/// authoritative for pruning. Returns empty string if list_workspaces()
/// is exhaustive.
fn binding_scope_prefix(&self) -> String;
```

| Provider | Return value | Meaning |
|----------|-------------|---------|
| cmux | `""` | `list_workspaces()` is exhaustive — all cmux bindings are in scope |
| zellij | `"{session_name}:"` | Only this session's bindings are in scope |
| tmux | `"{start_time}:{session_name}:"` | Only this server instance + session's bindings are in scope |

### Stale binding pruning (refresh.rs)

During `project_attachable_data`, after iterating over live workspaces:

1. Collect the set of live ws_refs from `list_workspaces()`
2. Get the provider's `binding_scope_prefix()`
3. For each workspace_manager binding in the store: if the binding's `external_ref` starts with the scope prefix but is NOT in the live set, remove it

This is safe because the scope prefix restricts pruning to bindings the current provider instance is authoritative about. Bindings from other sessions, server instances, or providers are untouched.

Note: the exact shape of this scoping mechanism may evolve (e.g., into something richer when we model multi-session properly), but the semantics are correct: "prune only what I'm the authority on."

### Downstream

The orchestrator, binding system, and correlation all treat ws_ref as an opaque string. Correlation already uses only `AttachableSet` keys for workspaces (not directories). No changes needed beyond the pruning addition in refresh.

### Known regression: loss of live directory-matching fallback

The current `select_existing_workspace` consults `list_workspaces()` and matches by directory, which serves as a live recovery path when the attachable registry is missing or desynced — particularly for cmux, which reports directories from the multiplexer itself. Under this design, a wiped or desynced registry will always create a duplicate workspace even when an exact live workspace already exists. This is acceptable: the directory-matching path is already broken for remote workspaces, and a reconciliation mechanism (matching live workspaces to sets as a fallback) can be added as part of the broader lifecycle work.

### Migration

None. We are in a no-backwards-compat phase. Existing bindings keyed on old-format refs become dead entries — and will be cleaned up by the new pruning logic on the next refresh cycle. New bindings use stable identifiers. Existing TOML state files become unused — they can be left in place or cleaned up manually.

### Tests

Re-record replay fixtures against the real systems (`REPLAY=record`) rather than editing fixture files directly. This may require human intervention to set up the workspace managers (cmux running, zellij session active, tmux session active). Add assertions that parsed ws_ref values use the new stable formats. Remove tests for the deleted TOML state management.
