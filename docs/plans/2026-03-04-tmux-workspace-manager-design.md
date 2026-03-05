# Tmux Workspace Manager Design

## Summary

Add a `WorkspaceManager` implementation for tmux, mapping flotilla workspaces to tmux windows within the current session. Follows the same patterns as the cmux and zellij providers.

## Workspace Mapping

Each workspace is a **tmux window** in the current session. Panes are splits within that window. Switching workspaces means selecting a different window.

## Detection

Register only when the `TMUX` env var is set (proving we're inside tmux). Priority in discovery: cmux > zellij > tmux.

## Core Operations

| Operation | tmux command |
|-----------|-------------|
| `list_workspaces()` | `tmux list-windows -F '#{window_index} #{window_name}'` |
| `create_workspace()` | `tmux new-window -n <name> -c <dir>`, then `split-window` for panes, `send-keys` for commands |
| `select_workspace()` | `tmux select-window -t <name>` |

## Template Mapping

Uses the existing `.flotilla/workspace.yaml` template system:

- **Panes** — `split-window` with `-h` (left/right) or `-v` (up/down)
- **Surfaces** — tmux has no tabbed or stacked panes. All surfaces become additional splits. A warning is logged when multiple surfaces exist in a pane, since the layout will differ from tabbed (cmux) or stacked (zellij) behavior.
- **Commands** — `send-keys '<command>' Enter`
- **Focus** — `select-pane -t <index>`

## State Persistence

Saved to `~/.config/flotilla/tmux/<session-name>/state.toml`:

```toml
[windows.feature-auth]
working_directory = "/path/to/worktree"
created_at = "2026-03-04T12:00:00Z"
```

Loaded during `list_workspaces()` to provide `CorrelationKey::CheckoutPath` for correlating windows with worktrees.

## Future Considerations

People configure tmux very differently. This design provides a reasonable default. Different layout strategies (e.g., using tmux layouts like `even-horizontal`, `tiled`) could be added later.
