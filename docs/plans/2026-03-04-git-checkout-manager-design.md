# Git Checkout Manager Design

## Summary

A `GitCheckoutManager` implementing the `CheckoutManager` trait using plain `git worktree` commands. Serves as a fallback when the `wt` CLI is not installed, or can be forced via config.

## Path Template

Worktrunk-compatible Jinja syntax via `minijinja`. Default:

```
{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}
```

**Variables:**
- `repo_path` — absolute path to repo root
- `repo` — repo directory name
- `branch` — raw branch name

**Filters:**
- `sanitize` — replaces `/` and `\` with `-`

## Configuration

Global config in `~/.config/flotilla/config.toml`:

```toml
[vcs.git.checkouts]
path = "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}"
provider = "auto"  # "auto" | "git" | "wt"
```

Per-repo override in `~/.config/flotilla/repos/<slug>.toml`:

```toml
path = "/Users/robert/dev/some-repo"

[vcs.git.checkouts]
path = ".worktrees/{{ branch | sanitize }}"
provider = "git"
```

`provider` values:
- `auto` (default) — try `wt` first, fall back to plain `git`
- `git` — force plain git worktree commands
- `wt` — force `wt` CLI

## Git Commands

| Operation | Command |
|-----------|---------|
| List | `git worktree list --porcelain` |
| Create | `git worktree add <path> <branch>` or `git worktree add -b <branch> <path>` |
| Remove | `git worktree remove <path>` then `git branch -D <branch>` |

## Listing Details

Parse `git worktree list --porcelain`, then for each worktree:
- `git rev-list --left-right --count HEAD...origin/<branch>` — remote ahead/behind
- `git rev-list --left-right --count HEAD...<default_branch>` — trunk ahead/behind
- `git status --porcelain` — working tree status
- `git log -1 --format=...` — last commit info

## Provider Discovery

In `discovery.rs`: check config for `provider` setting. If `auto`, try `wt` first, register `GitCheckoutManager` if not found. If `git`, skip `wt` check.

## New Dependencies

- `minijinja` — template rendering for path patterns

## Files Changed

| File | Change |
|------|--------|
| `src/providers/vcs/git_worktree.rs` | New — `GitCheckoutManager` implementation |
| `src/providers/vcs/mod.rs` | Re-export new module |
| `src/providers/discovery.rs` | Config-aware provider selection |
| `src/config.rs` | Load `config.toml`, parse worktree + provider settings |
| `Cargo.toml` | Add `minijinja` |
