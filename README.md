# flotilla

TUI dashboard for managing development workspaces across [cmux](https://cmux.dev), git worktrees, and GitHub.

![splash](assets/splash.png)

Provides a unified view of worktrees, pull requests, issues, Claude Code sessions, and remote branches. Enter on any item gets you into a workspace — creating whatever is missing (worktree, workspace) along the way.

## Usage

```
cargo run -- [--repo-root <path>]
```

Repo root is auto-detected from the current directory if omitted. Multiple repos can be managed as tabs.

## Dependencies

Requires these tools on your system:

| Tool | Purpose |
|------|---------|
| [cmux](https://cmux.dev) | Terminal workspace manager |
| [worktrunk](https://github.com/max-sixty/worktrunk) (`wt`) | Git worktree manager |
| [`gh`](https://cli.github.com/) | GitHub CLI (PRs, issues, browser opening) |
| [`git`](https://git-scm.com/) | Repo detection, remote branches |
| [`claude`](https://docs.anthropic.com/en/docs/claude-code) | Branch name generation via AI, session teleport |
| [`security`](https://ss64.com/mac/security.html) | macOS Keychain (OAuth token for Claude sessions API) |

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
| `[` / `]` | Switch tabs |
| `{` / `}` | Reorder tabs |
| Click | Select item |
| Scroll wheel | Navigate list |
| Drag tab | Reorder tabs |

### Actions

| Key | Action |
|-----|--------|
| Enter / Double-click | Open workspace (switch to existing, or create worktree + workspace as needed) |
| Space / Right-click | Action menu (shows all available actions for selected item) |
| `n` | New branch — enter name, creates worktree + workspace |
| `d` | Remove worktree (with safety confirmation) |
| `p` | Open PR in browser |
| `r` | Refresh data |
| `a` | Add repo tab |
| `c` | Toggle providers panel |

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
| `D` | Toggle debug panel |
| `q` / Esc | Quit |

## Workspace template

Place a `.flotilla/workspace.yaml` in your repo root to define the pane layout for new workspaces.

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
