# cmux-controller Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a workspace orchestrator that creates fully-configured cmux workspaces from a unified fzf picker (web sessions, GitHub issues, blank branches).

**Architecture:** A single Python script (`cmux-controller`) with uv shebang. It shells out to `fzf` for the picker, `wt` for worktree management, `cmux` for workspace/pane/surface creation, `gh` for GitHub issues. Web session fetching is stubbed initially (API TBD) and wired up later.

**Tech Stack:** Python 3.12+, uv, fzf, cmux CLI, wt CLI, gh CLI, PyYAML

---

### Task 1: Scaffold the script with uv shebang and arg parsing

**Files:**
- Create: `cmux-controller`

**Step 1: Write the script skeleton**

```python
#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["pyyaml"]
# ///
"""cmux-controller: workspace orchestrator for cmux."""

import argparse
import json
import subprocess
import sys
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description="cmux workspace orchestrator")
    parser.add_argument("--repo-root", type=Path, default=None,
                        help="Git repo root (auto-detected if omitted)")
    parser.add_argument("--template", type=Path, default=None,
                        help="Workspace template file (default: .cmux/workspace.yaml)")
    parser.add_argument("--dry-run", action="store_true",
                        help="Print commands without executing")
    args = parser.parse_args()

    repo_root = args.repo_root or detect_repo_root()
    if not repo_root:
        print("Error: not in a git repository", file=sys.stderr)
        sys.exit(1)

    print(f"Repo: {repo_root}")


def detect_repo_root() -> Path | None:
    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True, text=True
    )
    if result.returncode == 0:
        return Path(result.stdout.strip())
    return None


if __name__ == "__main__":
    main()
```

**Step 2: Make executable and test**

Run: `chmod +x cmux-controller && ./cmux-controller --help`
Expected: help text with --repo-root, --template, --dry-run options

**Step 3: Commit**

```bash
git init && git add cmux-controller
git commit -m "feat: scaffold cmux-controller with uv shebang and arg parsing"
```

---

### Task 2: Implement workspace template loading

**Files:**
- Modify: `cmux-controller`
- Create: `example-workspace.yaml` (example template for reference)

**Step 1: Write the template loader**

Add to `cmux-controller` after the imports:

```python
import yaml

DEFAULT_TEMPLATE = {
    "panes": [
        {"name": "main", "surfaces": [{"command": "{main_command}"}]}
    ]
}


def load_template(repo_root: Path, override: Path | None = None) -> dict:
    if override:
        path = override
    else:
        path = repo_root / ".cmux" / "workspace.yaml"

    if path.exists():
        with open(path) as f:
            return yaml.safe_load(f)
    return DEFAULT_TEMPLATE


def render_template(template: dict, variables: dict[str, str]) -> dict:
    """Substitute {variables} in all command strings."""
    import copy
    rendered = copy.deepcopy(template)
    for pane in rendered.get("panes", []):
        for surface in pane.get("surfaces", []):
            cmd = surface.get("command", "")
            for key, value in variables.items():
                cmd = cmd.replace(f"{{{key}}}", value)
            surface["command"] = cmd
    return rendered
```

**Step 2: Create example template**

```yaml
# example-workspace.yaml
# Copy to .cmux/workspace.yaml in your repo
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

**Step 3: Test template loading**

Add a temporary test at the bottom of main():
```python
template = load_template(repo_root, args.template)
rendered = render_template(template, {"main_command": "claude", "branch": "test"})
print(json.dumps(rendered, indent=2))
```

Run: `./cmux-controller --template example-workspace.yaml`
Expected: JSON with "claude" substituted into main pane command

**Step 4: Commit**

```bash
git add cmux-controller example-workspace.yaml
git commit -m "feat: add workspace template loading and variable substitution"
```

---

### Task 3: Implement data sources (GitHub issues + stub web sessions)

**Files:**
- Modify: `cmux-controller`

**Step 1: Implement GitHub issues fetcher**

```python
def fetch_github_issues(repo_root: Path) -> list[dict]:
    """Fetch open issues from the current repo via gh CLI."""
    result = subprocess.run(
        ["gh", "issue", "list", "--json", "number,title,labels,updatedAt",
         "--limit", "20", "--state", "open"],
        capture_output=True, text=True, cwd=repo_root
    )
    if result.returncode != 0:
        return []
    issues = json.loads(result.stdout)
    return [
        {
            "type": "issue",
            "id": str(issue["number"]),
            "title": issue["title"],
            "labels": ",".join(l["name"] for l in issue.get("labels", [])),
            "age": issue.get("updatedAt", ""),
        }
        for issue in issues
    ]
```

**Step 2: Implement stub web sessions fetcher**

```python
def fetch_web_sessions() -> list[dict]:
    """Fetch web sessions from claude.ai API. Stubbed until API is discovered."""
    # TODO: Wire up real API when claude.ai/code is back
    # Expected shape per session:
    # {
    #     "type": "web_session",
    #     "id": "<session-uuid>",
    #     "title": "Fix auth bug",
    #     "branch": "robert/fix-auth-bug",
    #     "repo": "myorg/myrepo",
    #     "age": "2h ago",
    # }
    return []
```

**Step 3: Implement the "new session" options**

```python
def new_session_options() -> list[dict]:
    return [
        {
            "type": "new_blank",
            "id": "blank",
            "title": "Blank session (current repo)",
        },
    ]
```

**Step 4: Test data sources**

Add to main():
```python
issues = fetch_github_issues(repo_root)
sessions = fetch_web_sessions()
new_opts = new_session_options()
print(f"Issues: {len(issues)}, Sessions: {len(sessions)}, New: {len(new_opts)}")
```

Run: `./cmux-controller` (from a repo with GitHub issues)
Expected: `Issues: N, Sessions: 0, New: 1`

**Step 5: Commit**

```bash
git add cmux-controller
git commit -m "feat: add GitHub issues fetcher and stub web session source"
```

---

### Task 4: Build the fzf picker

**Files:**
- Modify: `cmux-controller`

**Step 1: Implement picker formatting and fzf invocation**

```python
import os
import tempfile

# ANSI color helpers
DIM = "\033[2m"
CYAN = "\033[36m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
RESET = "\033[0m"


def format_picker_lines(sessions: list[dict], new_opts: list[dict],
                        issues: list[dict]) -> list[str]:
    """Format all items as fzf-compatible lines with hidden metadata prefix."""
    lines = []

    if sessions:
        lines.append(f"{DIM}  Web Sessions{RESET}")
        for s in sessions:
            # Hidden prefix: type:id — used to parse selection
            meta = f"web_session:{s['id']}"
            display = (f"    {s['title']:<30s} {CYAN}{s.get('branch', ''):<30s}{RESET} "
                       f"{s.get('repo', ''):<20s} {DIM}{s.get('age', '')}{RESET}")
            lines.append(f"{meta}\t{display}")

    lines.append(f"{DIM}  New Session{RESET}")
    for n in new_opts:
        meta = f"new_blank:{n['id']}"
        lines.append(f"{meta}\t    {GREEN}\u2219 {n['title']}{RESET}")

    if issues:
        lines.append(f"{DIM}  GitHub Issues{RESET}")
        for i in issues:
            meta = f"issue:{i['id']}"
            labels = f" {DIM}{i['labels']}{RESET}" if i.get("labels") else ""
            lines.append(f"{meta}\t    {YELLOW}#{i['id']:<5s}{RESET} {i['title']:<45s}{labels}")

    return lines


def run_picker(lines: list[str]) -> str | None:
    """Run fzf with formatted lines, return selected line's metadata prefix."""
    input_text = "\n".join(lines)

    result = subprocess.run(
        ["fzf", "--ansi", "--no-sort", "--delimiter=\t", "--with-nth=2",
         "--header=↑↓ navigate  enter select  ctrl-c cancel",
         "--prompt=  workspace> ",
         "--pointer=▶",
         "--color=pointer:cyan,prompt:cyan"],
        input=input_text, capture_output=True, text=True
    )

    if result.returncode != 0:
        return None

    selected = result.stdout.strip()
    # Extract metadata prefix (before tab)
    if "\t" in selected:
        return selected.split("\t")[0]
    return None
```

**Step 2: Wire picker into main**

Replace the test code in main() with:

```python
    sessions = fetch_web_sessions()
    new_opts = new_session_options()
    issues = fetch_github_issues(repo_root)

    lines = format_picker_lines(sessions, new_opts, issues)
    selection = run_picker(lines)
    if not selection:
        print("Cancelled.")
        sys.exit(0)

    print(f"Selected: {selection}")
```

**Step 3: Test the picker**

Run: `./cmux-controller` (from a repo with GH issues)
Expected: fzf picker appears with "New Session" and "GitHub Issues" sections. Selecting an item prints its metadata.

**Step 4: Commit**

```bash
git add cmux-controller
git commit -m "feat: add unified fzf picker with sections"
```

---

### Task 5: Implement action dispatch (worktree + cmux workspace)

**Files:**
- Modify: `cmux-controller`

**Step 1: Implement worktree setup**

```python
def setup_worktree(action_type: str, action_id: str, repo_root: Path,
                   dry_run: bool = False) -> tuple[str, Path]:
    """Create/switch worktree and return (branch_name, worktree_path)."""
    if action_type == "web_session":
        # For web sessions, we need the branch from session data
        # TODO: get branch from session metadata
        branch = action_id  # placeholder
        cmd = ["wt", "switch", branch, "--no-cd"]
    elif action_type == "issue":
        branch = f"issue-{action_id}"
        cmd = ["wt", "switch", "--create", branch, "--no-cd"]
    elif action_type == "new_blank":
        # Prompt for branch name
        branch = input("Branch name: ").strip()
        if not branch:
            print("No branch name provided", file=sys.stderr)
            sys.exit(1)
        cmd = ["wt", "switch", "--create", branch, "--no-cd"]
    else:
        print(f"Unknown action type: {action_type}", file=sys.stderr)
        sys.exit(1)

    if dry_run:
        print(f"[dry-run] {' '.join(cmd)}")
        return branch, repo_root

    result = subprocess.run(cmd, capture_output=True, text=True, cwd=repo_root)
    if result.returncode != 0:
        print(f"wt switch failed: {result.stderr}", file=sys.stderr)
        sys.exit(1)

    # Get worktree path from wt list
    list_result = subprocess.run(
        ["wt", "list", "--format=json"],
        capture_output=True, text=True, cwd=repo_root
    )
    if list_result.returncode == 0:
        worktrees = json.loads(list_result.stdout)
        for wt in worktrees:
            if wt.get("branch", "").endswith(branch) or wt.get("branch") == branch:
                return branch, Path(wt["path"])

    # Fallback: assume sibling directory pattern
    return branch, repo_root.parent / f"{repo_root.name}.{branch}"
```

**Step 2: Implement cmux workspace creation from template**

```python
def cmux_run(*args: str, json_output: bool = False) -> str:
    """Run a cmux CLI command and return stdout."""
    cmd = ["/Applications/cmux.app/Contents/Resources/bin/cmux"] + list(args)
    if json_output:
        cmd.insert(1, "--json")
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        print(f"cmux error: {' '.join(cmd)}: {result.stderr}", file=sys.stderr)
    return result.stdout.strip()


def create_cmux_workspace(template: dict, worktree_path: Path,
                          dry_run: bool = False) -> None:
    """Create a cmux workspace from a rendered template."""
    panes = template.get("panes", [])
    if not panes:
        return

    # Create workspace
    if dry_run:
        print("[dry-run] cmux new-workspace")
        for pane in panes:
            for surface in pane.get("surfaces", []):
                cmd = surface.get("command", "")
                print(f"[dry-run] send: cd {worktree_path} && {cmd}")
        return

    ws_output = cmux_run("new-workspace", "--json", json_output=True)
    # Parse workspace ref from output
    try:
        ws_data = json.loads(ws_output)
        ws_ref = ws_data.get("workspace", ws_data.get("id", ""))
    except (json.JSONDecodeError, KeyError):
        ws_ref = ws_output.strip()

    pane_refs: dict[str, str] = {}
    first_surface_ref = None

    for i, pane in enumerate(panes):
        if i == 0:
            # First pane is the default one in the new workspace
            # List panes to get its ref
            panes_output = cmux_run("list-panes", "--json", json_output=True)
            try:
                pane_list = json.loads(panes_output)
                current_ref = pane_list[-1].get("ref", pane_list[-1].get("id", ""))
            except (json.JSONDecodeError, KeyError, IndexError):
                current_ref = "pane:1"
            pane_refs[pane.get("name", f"pane_{i}")] = current_ref
        else:
            direction = pane.get("split", "right")
            parent_name = pane.get("parent")
            target = pane_refs.get(parent_name, "") if parent_name else ""
            split_args = ["new-split", direction]
            if target:
                split_args.extend(["--panel", target])
            split_output = cmux_run(*split_args, "--json", json_output=True)
            try:
                split_data = json.loads(split_output)
                current_ref = split_data.get("ref", split_data.get("id", ""))
            except (json.JSONDecodeError, KeyError):
                current_ref = f"pane:{i+1}"
            pane_refs[pane.get("name", f"pane_{i}")] = current_ref

        surfaces = pane.get("surfaces", [])
        for j, surface in enumerate(surfaces):
            if i == 0 and j == 0:
                # First surface of first pane already exists
                pass
            else:
                # Create additional surface in this pane
                cmux_run("new-surface", "--type", "terminal",
                         "--pane", pane_refs[pane.get("name", f"pane_{i}")])

            cmd = surface.get("command", "")
            if cmd:
                full_cmd = f"cd {worktree_path} && {cmd}"
            else:
                full_cmd = f"cd {worktree_path}"
            # Send command to the surface
            cmux_run("send", full_cmd + "\n")
```

**Step 3: Wire dispatch into main**

```python
    # Parse selection
    action_type, action_id = selection.split(":", 1)

    # Determine main_command
    if action_type == "web_session":
        main_command = f"claude --teleport {action_id}"
    elif action_type == "issue":
        # Look up issue title
        title = next((i["title"] for i in issues if i["id"] == action_id), "")
        main_command = f'claude --remote "Fix #{action_id}: {title}"'
    elif action_type == "new_blank":
        main_command = "claude"
    else:
        print(f"Unknown selection: {selection}", file=sys.stderr)
        sys.exit(1)

    # Setup worktree
    branch, wt_path = setup_worktree(action_type, action_id, repo_root, args.dry_run)

    # Load and render template
    template = load_template(repo_root, args.template)
    rendered = render_template(template, {
        "main_command": main_command,
        "branch": branch,
        "repo": repo_root.name,
        "issue_number": action_id if action_type == "issue" else "",
        "session_id": action_id if action_type == "web_session" else "",
    })

    # Create cmux workspace
    create_cmux_workspace(rendered, wt_path, args.dry_run)
    print(f"Workspace created for {branch} at {wt_path}")
```

**Step 4: Test with --dry-run**

Run: `./cmux-controller --dry-run`
Expected: pick an item, see dry-run output of wt and cmux commands

**Step 5: Commit**

```bash
git add cmux-controller
git commit -m "feat: add worktree setup and cmux workspace creation"
```

---

### Task 6: End-to-end test with cmux

**Files:**
- Modify: `cmux-controller` (minor fixes from testing)

**Step 1: Test from a real git repo with GitHub issues**

Run from a repo that has GitHub issues:
```bash
cd ~/dev/some-repo-with-issues
~/dev/scratch/cmux-controller
```

Expected: picker shows issues and "new blank" option. Selecting "new blank" prompts for branch name, creates worktree, creates cmux workspace with panes.

**Step 2: Test with a workspace template**

Create `.cmux/workspace.yaml` in the test repo with the example template, then run again.

**Step 3: Fix any issues found during testing**

**Step 4: Commit**

```bash
git add cmux-controller
git commit -m "fix: polish from end-to-end testing"
```

---

### Task 7: Wire up web session API (deferred — depends on claude.ai/code being available)

**Files:**
- Modify: `cmux-controller` — replace `fetch_web_sessions()` stub

**Step 1: API Discovery**

Use Playwright to navigate to claude.ai/code, open network tab, trigger the teleport/session list, and capture:
- The endpoint URL
- Auth headers (OAuth token source)
- Response shape

**Step 2: Implement real fetch_web_sessions()**

Replace the stub with actual API calls using `urllib.request` (to avoid adding httpx dependency) or subprocess curl.

**Step 3: Test with real sessions**

Run the picker and verify web sessions appear with branch info.

**Step 4: Commit**

```bash
git add cmux-controller
git commit -m "feat: wire up claude.ai web session API"
```

---

## Summary

| Task | What | Depends on |
|------|------|------------|
| 1 | Script scaffold with uv shebang | Nothing |
| 2 | Template loading + rendering | Task 1 |
| 3 | Data sources (GH issues + stub sessions) | Task 1 |
| 4 | fzf picker with sections | Task 3 |
| 5 | Action dispatch (wt + cmux) | Tasks 2, 4 |
| 6 | End-to-end test | Task 5 |
| 7 | Web session API (deferred) | claude.ai being up |

Tasks 2 and 3 can run in parallel after Task 1.
