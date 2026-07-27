# Configuration

## Repo tracking

Stored in `~/.config/flotilla/`:

- `repos/*.toml` — one file per tracked repo, containing `path = "..."`
- `open-views.toml` — the ordered set of Views opened by the TUI

Repos are added interactively from within flotilla using the `a` key.

### Fork provenance

Repositories that are maintained as forks declare their upstream provenance in
the tracked repo's TOML file:

```toml
path = "/home/alice/dev/zellij"

[upstream]
url = "https://github.com/zellij-org/zellij"
relation = "fork"

[issue_tracker.forgejo]
scope = "fork-issues/zellij"
```

Fork-stance provisioning clones only the repository's own URL as `origin`; it
does not add the upstream as a remote. Convoy admission requires a workflow
with an in-crew reviewer, such as `implement-review`. A deliberate per-repo
override can admit review-less workflows:

```toml
[workflow]
allow_reviewless = true
```

## Convoy start attachment

By default, `flotilla convoy start` attaches to the new convoy only when the
daemon has no presentation-manager connector (`flotilla pm connect`) connected.
`--attach` and `--no-attach` always override that heuristic.

To override the default for every convoy start that omits those flags, set
`auto_attach` in `~/.config/flotilla/config.toml`:

```toml
[convoy]
auto_attach = false
```

## Convoy placement admission

Each host refuses new convoy placement when the volume containing its Flotilla
state directory is below a free-space floor. The default is 20 GiB. Override it
per host in that host's `~/.config/flotilla/daemon.toml`:

```toml
[admission]
free_space_floor_gib = 50
```

Set the floor to `0` only when an external system provides an equivalent
capacity guard.

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
