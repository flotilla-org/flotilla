# Zellij Workspace Manager Design

## Summary

Implement `ZellijWorkspaceManager` as a new provider implementing the existing `WorkspaceManager` trait, using zellij CLI actions for tab/pane orchestration. Workspace = top-level tab in zellij.

## Concept Mapping

| Template Concept | cmux | zellij |
|---|---|---|
| Workspace | workspace | tab |
| Pane (split region) | split/surface | pane (tiled) |
| Surface (pane-level tab) | surface within pane | stacked pane |
| Focus | focus-pane | move-focus |
| ws_ref | `workspace:N` | tab name (string) |

## Environment Detection

Detected via `ZELLIJ` env var presence + version check (>= 0.40).

Available env vars in a zellij session:
- `ZELLIJ` — always set (value is pane ID, e.g. `0`)
- `ZELLIJ_PANE_ID` — current pane ID
- `ZELLIJ_SESSION_NAME` — session name (e.g. `erudite-cactus`)

Priority: if both cmux and zellij env vars are present, use whichever matches the actual terminal environment (check `CMUX_SOCKET_PATH` vs `ZELLIJ`).

## Implementation

### File: `src/providers/workspace/zellij.rs`

Struct `ZellijWorkspaceManager` with a helper `zellij_action(args)` method (mirrors `CmuxWorkspaceManager::cmux_cmd`).

### `create_workspace(config) -> Workspace`

1. Create tab: `zellij action new-tab --name "{config.name}" --cwd "{working_dir}"`
2. First pane gets the tab's initial pane. For its command, use `zellij action new-pane --in-place -- {command}` or `write-chars` if just a `cd`.
3. For each subsequent pane in the template:
   - `zellij action new-pane -d {right|down} -- {command}` — splits with direct command execution
   - Direction comes from `pane.split` field (default: `right`)
4. For additional surfaces within a pane (stacks):
   - `zellij action new-pane --stacked -- {command}` — creates stacked pane
5. If a pane has no command (empty string), create a plain shell pane and `write-chars` a `cd {working_dir}\n`.
6. Save tab metadata to state file.
7. Return `Workspace` with tab name as `ws_ref`.

### `list_workspaces() -> Vec<Workspace>`

1. `zellij action query-tab-names` — returns newline-separated tab names
2. Load state file for the current session (from `ZELLIJ_SESSION_NAME`)
3. For each tab name:
   - If state file has metadata, use it (working directory, correlation keys)
   - Otherwise, return minimal Workspace with just name/ws_ref
4. Return vec of workspaces

### `select_workspace(ws_ref) -> ()`

1. `zellij action go-to-tab-name "{ws_ref}"`

### Version Check

```
zellij --version
```

Parse output (e.g. `zellij 0.43.1`), check major.minor >= 0.40.

## State File

**Location:** `~/.config/flotilla/zellij/{session_name}/state.toml`

**Format (TOML):**

```toml
[tabs.my-feature]
working_directory = "/Users/robert/dev/myproject"
created_at = "2026-03-04T10:00:00Z"

[[tabs.my-feature.correlation_keys]]
type = "CheckoutPath"
value = "/Users/robert/dev/myproject"
```

The state file is advisory — tab names from `query-tab-names` are the source of truth for what exists. The state file enriches with metadata zellij doesn't track.

If the state file is missing or corrupt, degrade gracefully: list tabs without enrichment, create new state on workspace creation.

## Discovery Registration

In `src/providers/discovery.rs`, after the cmux check:

```rust
if std::env::var("ZELLIJ").is_ok() && registry.workspace_manager.is_none() {
    if ZellijWorkspaceManager::check_version().await.is_ok() {
        registry.workspace_manager = Some((
            "zellij".to_string(),
            Box::new(ZellijWorkspaceManager::new()),
        ));
        info!("{repo_name}: Workspace mgr -> zellij");
    }
}
```

The `is_none()` check ensures only one workspace manager is registered. Since cmux is checked first and zellij second, being inside cmux takes priority. This is correct because if you're running flotilla inside cmux, you want cmux workspace management even if zellij happens to be set.

## Template Compatibility

The existing `WorkspaceTemplate` YAML format works unchanged:

- `panes[].split` -> zellij split direction (`right` or `down`)
- `panes[].surfaces[]` -> stacked panes within a split
- `panes[].surfaces[].command` -> passed to `new-pane -- command`
- `panes[].parent` -> controls which pane to split from (sequential creation handles this)
- `panes[].focus` -> `move-focus` after creation

The `parent` field maps less directly in zellij since focus determines which pane gets split. We handle this by tracking creation order and focusing the correct pane before splitting.

## Error Handling

- CLI failures propagate as `Err(String)`, matching cmux pattern
- Missing state file: create on first write, skip enrichment on read
- Corrupt state file: log warning, treat as empty
- Version check failure: skip registration with warning log

## Future Extensions

- KDL layout generation for more precise layout control
- Attach richer metadata to tabs if zellij adds plugin-based metadata storage
- Floating pane support in templates
