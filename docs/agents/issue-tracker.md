# Issue tracker: GitHub

Issues and PRDs for this repo live as GitHub issues on **`flotilla-org/flotilla`**. Use the `gh` CLI for all operations.

**Always pass `-R flotilla-org/flotilla` explicitly.** Do not rely on `gh` inferring the repo from `git remote` — clones of this repo may be forks (e.g. `changedirection/flotilla` for Claude Code Web sessions) whose remotes point elsewhere, while issues and PRs always live upstream.

## Conventions

- **Create an issue**: `gh issue create -R flotilla-org/flotilla --title "..." --body "..."`. Use a heredoc for multi-line bodies. Then set the issue type (see below).
- **Read an issue**: `gh issue view <number> -R flotilla-org/flotilla --comments`, filtering comments by `jq` and also fetching labels.
- **List issues**: `gh issue list -R flotilla-org/flotilla --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'` with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> -R flotilla-org/flotilla --body "..."`
- **Apply / remove labels**: `gh issue edit <number> -R flotilla-org/flotilla --add-label "..."` / `--remove-label "..."`
- **Close**: `gh issue close <number> -R flotilla-org/flotilla --comment "..."`

## Issue types

Every issue gets a **type** (lifecycle stage), distinct from labels: `Task`, `Bug`, `Feature`, or `Brainstorm`. `gh issue create` does not support `--type`, so set it after creation via the API:

```bash
gh api -X PATCH repos/flotilla-org/flotilla/issues/<number> -f type="TypeName"
```

| Type | Use for |
|------|---------|
| `Task` | A specific piece of work |
| `Bug` | An unexpected problem or behavior |
| `Feature` | A request, idea, or new functionality |
| `Brainstorm` | Needs design thinking before it can become a task or feature |

## Labels

Triage-role labels are mapped in `docs/agents/triage-labels.md`. Topic labels (`bug`, `ui`, `multi-host`, `from-review`, `quick-win`, …) are documented in the "Issue Types and Labels" section of `CLAUDE.md` — combine as appropriate.

## Pull requests as a triage surface

**PRs as a request surface: no.** External PRs go through the normal review flow; `/triage` only reads issues.

## When a skill says "publish to the issue tracker"

Create a GitHub issue on `flotilla-org/flotilla` (and set its type).

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> -R flotilla-org/flotilla --comments`.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single issue with **child** issues as tickets.

- **Map**: a single issue labelled `wayfinder:map`, holding the Notes / Decisions-so-far / Fog body. `gh issue create -R flotilla-org/flotilla --label wayfinder:map`.
- **Child ticket**: an issue linked to the map as a GitHub sub-issue (`gh api` on the sub-issues endpoint). Where sub-issues aren't enabled, add the child to a task list in the map body and put `Part of #<map>` at the top of the child body. Labels: `wayfinder:<type>` (`research`/`prototype`/`grilling`/`task`). Once claimed, the ticket is assigned to the driving dev.
- **Blocking**: GitHub's **native issue dependencies** — the canonical, UI-visible representation. Add an edge with `gh api --method POST repos/flotilla-org/flotilla/issues/<child>/dependencies/blocked_by -F issue_id=<blocker-db-id>`, where `<blocker-db-id>` is the blocker's numeric **database id** (`gh api repos/flotilla-org/flotilla/issues/<n> --jq .id`, _not_ the `#number` or `node_id`). GitHub reports `issue_dependencies_summary.blocked_by` (open blockers only — the live gate). Where dependencies aren't available, fall back to a `Blocked by: #<n>, #<n>` line at the top of the child body. A ticket is unblocked when every blocker is closed.
- **Frontier query**: list the map's open children (`gh issue list -R flotilla-org/flotilla --state open`, scoped to the map's sub-issues / task list), drop any with an open blocker (`issue_dependencies_summary.blocked_by > 0`, or an open issue in the `Blocked by` line) or an assignee; first in map order wins.
- **Claim**: `gh issue edit <n> -R flotilla-org/flotilla --add-assignee @me` — the session's first write.
- **Resolve**: `gh issue comment <n> -R flotilla-org/flotilla --body "<answer>"`, then `gh issue close <n> -R flotilla-org/flotilla`, then append a context pointer (gist + link) to the map's Decisions-so-far.
