# Configuration

## Repo tracking

Stored in `~/.config/flotilla/`:

- `repos/*.toml` — one file per tracked repo, containing `path = "..."`
- `tab-order.json` — array of repo paths controlling tab order

Repos are added interactively from within flotilla using the `a` key.

## Dependencies

Flotilla auto-detects available tools. Nothing is strictly required beyond git, but more tools unlock more features.

| Tool | Purpose | Required |
|------|---------|----------|
| [git](https://git-scm.com/) | Repo detection, branches, worktrees | Yes |
| [gh](https://cli.github.com/) | GitHub PRs and issues | No |
| [claude](https://docs.anthropic.com/en/docs/claude-code) | Agent sessions, branch name generation | No |
| [cmux](https://cmux.dev) | Terminal workspace manager | No |
| [wt](https://github.com/max-sixty/worktrunk) | Git worktree manager (alternative to plain git worktrees) | No |

## Checkout manager

The checkout manager provider can be configured per-repo in `~/.config/flotilla/repos/<slug>.toml`:

```toml
[checkouts]
provider = "wt"    # "wt", "git", or "auto" (default)
```

- `auto`: uses `wt` if available, falls back to plain git worktrees
- `wt`: requires the `wt` CLI
- `git`: uses `git worktree` commands directly
