# Workspace Templates

Place a `.flotilla/workspace.yaml` in your repo root to define the pane layout for new workspaces.

## Format

```yaml
panes:
  - name: <string>           # Unique pane identifier (required)
    split: <direction>        # "right", "left", "up", "down" (omit for first pane)
    parent: <pane-name>       # Which pane to split from (omit for first pane)
    focus: <bool>             # Set keyboard focus to this pane (default: false)
    surfaces:
      - command: <string>     # Shell command to run in this tab
        active: <bool>        # Make this tab the selected one (default: false)
```

The variable `{main_command}` is substituted with the primary command (typically `claude`, or `claude --teleport <id>` for session teleport).

## Example

Three panes: Claude on the left with focus, Codex + Gemini as tabs on the top-right (Codex active), and a shell on the bottom-right.

```yaml
panes:
  - name: left
    focus: true
    surfaces:
      - command: "{main_command}"

  - name: top-right
    split: right
    parent: left
    surfaces:
      - command: "codex"
        active: true
      - command: "gemini"

  - name: bottom-right
    split: down
    parent: top-right
    surfaces:
      - command: ""
```

## Default

If no template exists, a single pane with `{main_command}` is created.
