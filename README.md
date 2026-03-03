# cmux-controller

TUI dashboard for managing development workspaces across cmux, git worktrees, and GitHub.

Provides a unified view of worktrees, pull requests, issues, Claude Code sessions, and remote branches. Enter on any item gets you into a workspace — creating whatever is missing (worktree, workspace) along the way.

## Usage

```
cargo run -- [--repo-root <path>]
```

Repo root is auto-detected from the current directory if omitted.

## Dependencies

Requires these tools on your system:

| Tool | Purpose |
|------|---------|
| [cmux](https://cmux.app) | Terminal workspace manager |
| `wt` | Git worktree helper (`wt list --format=json`, `wt switch --create`) |
| `gh` | GitHub CLI (PRs, issues, browser opening) |
| `git` | Repo detection, remote branches |
| `claude` | Branch name generation via AI, session teleport |
| `curl` | Claude Code sessions API |
| `security` | macOS Keychain (OAuth token for sessions API) |

## Data sources

The dashboard fetches and correlates data from multiple sources into a single table:

- **Worktrees** — `wt list`, sorted alphabetically by branch
- **Pull requests** — `gh pr list`, linked to worktrees by branch name
- **GitHub issues** — `gh issue list`, linked to PRs via "Fixes/Closes/Resolves #N"
- **Claude Code sessions** — Anthropic API, matched to branches
- **Remote branches** — `git ls-remote`, filtered to exclude known/merged branches
- **cmux workspaces** — `cmux --json list-workspaces`, matched to worktrees by directory

Data auto-refreshes every 10 seconds. Press `r` to refresh manually.

## Keybindings

### Navigation

| Key | Action |
|-----|--------|
| `j` / `k` / `↑` / `↓` | Navigate list |
| Click | Select item |
| Scroll wheel | Navigate list |

### Actions

| Key | Action |
|-----|--------|
| Enter / Double-click | Open workspace (switch to existing, or create worktree + workspace as needed) |
| Space / Right-click | Action menu (shows all available actions for selected item) |
| `n` | New branch — enter name, creates worktree + workspace |
| `d` | Remove worktree (with safety confirmation) |
| `p` | Open PR in browser |
| `r` | Refresh data |

### Multi-select (issues)

| Key | Action |
|-----|--------|
| Shift+Enter | Toggle selection on current item |
| Shift+Click | Toggle selection on clicked item |
| Enter | Generate combined branch name for all selected issues |
| Esc | Clear selection |

### General

| Key | Action |
|-----|--------|
| `?` | Toggle help overlay |
| `q` / Esc | Quit |

## Workspace template

Place a `.cmux/workspace.yaml` in your repo root to define the pane layout for new workspaces.

### Format

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

### Example

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

### Default

If no template exists, a single pane with `{main_command}` is created.

## Action menu

The action menu (Space or right-click) shows context-sensitive options based on the selected item:

| Action | When available |
|--------|---------------|
| Switch to workspace | Item has an existing cmux workspace |
| Create workspace | Worktree exists but no workspace |
| Create worktree + workspace | Branch exists but no local worktree |
| Remove worktree | Local worktree exists |
| Generate branch name | Issue with no branch (uses Claude AI) |
| Open PR in browser | Item has an associated PR |
| Open issue in browser | Item has associated issues |
| Teleport session | Claude Code session (opens in terminal) |
| Archive session | Claude Code session (marks as archived) |
