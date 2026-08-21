# Workspace Templates

Place a `.flotilla/workspace.yaml` in your repo root (or `~/.config/flotilla/workspace.yaml` globally) to define workspace layout for new workspaces.

## Format

```yaml
# What terminal sessions to create
content:
  - role: <string>         # Unique role identifier (required)
    command: <string>       # Shell command to run (empty = default shell)
    type: terminal          # Content type (default: "terminal")
    count: <number>         # How many instances (default: 1)

# How to arrange them in the workspace
layout:
  - slot: <role>            # Which content role to place here (required)
    split: <direction>      # "right", "left", "up", "down" (omit for first slot)
    parent: <role>          # Which slot to split from (omit for first slot)
    focus: <bool>           # Set keyboard focus to this slot (default: false)
```

The variable `{main_command}` is substituted with the primary command (typically `claude`, or `claude --teleport <id>` for session teleport).

## Example

Three panes: Claude on the left with focus, Codex on the top-right, and a shell on the bottom-right.

```yaml
content:
  - role: main
    command: "{main_command}"
  - role: ai
    command: codex
  - role: shell
    command: ""

layout:
  - slot: main
    focus: true
  - slot: ai
    split: right
  - slot: shell
    split: down
    parent: ai
```

## Terminal Pool Integration

When a terminal pool (e.g. shpool) is available, content entries are resolved through it. Each role becomes a persistent terminal session that survives workspace disconnects. The session name follows the pattern `flotilla/{branch}/{role}/{index}`.

## Default

If no template exists, a single pane with `{main_command}` is created.
