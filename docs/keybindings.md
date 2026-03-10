# Keybindings

## Navigation

| Key | Action |
|-----|--------|
| `j` / `k` / `↑` / `↓` | Navigate list |
| `[` / `]` | Switch tabs |
| `{` / `}` | Reorder tabs |
| Click | Select item |
| Scroll wheel | Navigate list |
| Drag tab | Reorder tabs |

## Actions

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

## Multi-select (issues)

| Key | Action |
|-----|--------|
| Shift+Enter | Toggle selection on current item |
| Shift+Click | Toggle selection on clicked item |
| Enter | Generate combined branch name for all selected issues |
| Esc | Clear selection |

## General

| Key | Action |
|-----|--------|
| `?` | Toggle help overlay |
| `D` | Toggle debug panel |
| `q` / Esc | Quit |

## Action menu

The action menu (Space or right-click) shows context-sensitive options based on the selected item:

| Action | When available |
|--------|---------------|
| Switch to workspace | Item has an existing workspace |
| Create workspace | Worktree exists but no workspace |
| Create worktree + workspace | Branch exists but no local worktree |
| Remove worktree | Local worktree exists |
| Generate branch name | Issue with no branch (uses AI) |
| Open PR in browser | Item has an associated PR |
| Open issue in browser | Item has associated issues |
| Teleport session | Cloud agent session (opens in terminal) |
| Archive session | Cloud agent session (marks as archived) |
