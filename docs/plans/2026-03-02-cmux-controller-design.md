# cmux-controller Design

A workspace orchestrator for cmux that creates fully-configured development workspaces from web sessions, GitHub issues, or blank branches.

## Problem

Claude Code's `--teleport` picker shows web sessions but not their git branches. You need the branch to set up a worktree (via worktrunk) before teleporting. More broadly, spinning up a multi-pane development workspace for a task involves several manual steps: fetch branch, create worktree, open cmux workspace, split panes, launch tools.

## Solution

A single tool (`cmux-controller`) that:
1. Presents a unified fzf picker of web sessions, GitHub issues, and a "new blank" option
2. Sets up a git worktree via `wt switch`
3. Creates a cmux workspace from a repo-defined template (panes, surfaces, commands)
4. Launches the appropriate command (teleport, remote, or plain claude) in the main surface

## cmux Topology Model

```
Window
+-- Workspace (sidebar tab -- one per task/branch)
    +-- Pane 1 (main, left)
    |   +-- Surface: claude --teleport <id>
    +-- Pane 2 (right, vertical split)
    |   +-- Surface 1: codex        \  tabs within
    |   +-- Surface 2: gemini       /  the pane
    +-- Pane 3 (bottom-right split)
        +-- Surface: zsh
```

- **Window**: top-level macOS window
- **Workspace**: tab in the sidebar (one per task)
- **Pane**: split region within a workspace
- **Surface**: terminal or browser tab within a pane (multiple surfaces stack as tabs)

## Workspace Templates

Stored in repo at `.cmux/workspace.yaml`:

```yaml
panes:
  - name: main
    surfaces:
      - command: "{main_command}"
  - name: ai
    split: right
    surfaces:
      - name: codex
        command: codex
      - name: gemini
        command: gemini
  - name: shell
    split: down
    parent: ai
    surfaces:
      - command: ""
```

Variables substituted at workspace creation:
- `{main_command}` -- determined by picker selection
- `{branch}`, `{repo}`, `{issue_number}`, `{session_id}`

Without a template file, defaults to a single pane with `{main_command}`.

## Unified Picker

fzf-based, with ANSI-colored section headers (non-selectable):

```
  Web Sessions
    fix-auth-bug         robert/fix-auth       myrepo   2h ago
    update-api-docs      robert/api-docs       myrepo   1d ago
  New Session
    . Blank (current repo)
  GitHub Issues -- myrepo
    #342  Fix login redirect loop           bug
    #339  Add rate limiting                  enhancement
```

- Fuzzy matching across all items
- `--preview` pane shows session summary or issue body
- Section headers filtered from selection

## Actions by Selection Type

| Selection    | Worktree                          | main_command                              |
|------------- |---------------------------------- |------------------------------------------ |
| Web session  | `wt switch <branch>`              | `claude --teleport <session_id>`          |
| New blank    | `wt switch -c <name>` (prompted)  | `claude`                                  |
| GitHub issue | `wt switch -c <issue-branch>`     | `claude --remote "Fix #N: <title>"`       |

## Workspace Creation Sequence

1. Run worktree action (`wt switch ...` with `--no-cd`)
2. Get worktree path from `wt list --format=json`
3. `cmux new-workspace` -- capture workspace ref
4. For each pane in template:
   - First pane: use the default surface in the new workspace
   - Subsequent panes: `cmux new-split <direction>`
   - Additional surfaces in a pane: `cmux new-surface --pane <ref>`
5. For each surface: `cmux send --surface <ref> "cd <worktree_path> && <command>"`

## Session Data Source

Web sessions require calling the same API that `claude --teleport` uses. Discovery approach:
1. Use Playwright to navigate to claude.ai/code, capture network requests
2. Identify the session list endpoint and auth mechanism
3. Implement direct API calls in the tool

GitHub issues: `gh issue list --json number,title,labels --limit 20`

## Tech Stack

- Python with uv shebang (`#!/usr/bin/env -S uv run --script`)
- fzf for the picker (subprocess)
- cmux CLI for workspace management
- wt CLI for worktree management
- gh CLI for GitHub issues
- requests/httpx for claude.ai API

## File Layout

```
cmux-controller          # main script (uv shebang Python)
.cmux/workspace.yaml     # example template (lives in target repos)
```

## Phase 1 Scope

1. API discovery via Playwright (spike)
2. Picker with web sessions + new blank + GitHub issues
3. Workspace template application via cmux commands
4. Worktree setup via wt switch

## Deferred

- TUI upgrade (move to textual/ratatui if fzf proves limiting)
- Local session resume (already handled by `claude --resume`)
- Multi-repo picker
- Claude Code marketplace plugin packaging
