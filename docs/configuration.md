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

The state-directory volume is the admission proxy for host capacity; checkout
paths can be configured on other volumes and are not measured by this check.
Hosts that separate those volumes should provide an equivalent capacity guard
for each checkout volume.

If Flotilla cannot identify the state-directory volume or measure its available
space, placement is refused. This fail-closed behavior prevents an unavailable
measurement from silently disabling admission.

Set the floor to `0` only when an external system provides an equivalent
capacity guard.

## Daemon logging

Each daemon writes structured JSON-lines to
`~/.local/state/flotilla/log/flotillad.jsonl`. The file rotates by size and
remains host-local. Configure the filter and rotation bounds in that host's
`~/.config/flotilla/daemon.toml`:

```toml
[logging]
filter = "info,flotilla_daemon::peer=debug"
max_bytes = 10485760
generations = 4
```

The filter uses `RUST_LOG` directive syntax. When it is omitted, the daemon
uses `RUST_LOG` and then its built-in defaults. Restart the daemon after
changing logging settings; the writer and its rotation bounds are configured
at startup.

Read local or peer logs on demand without SSH:

```bash
flotilla logs --host feta --since 2h --level warn --target flotilla_daemon::peer
```

Output remains JSONL so it can be piped directly to `jq`.

## Resource manifests

A daemon can continuously apply a directory of JSON and YAML resource
documents as additive desired state:

```toml
[manifests]
dir = "/home/alice/dev/project-map/flotilla"
```

Each file contains one or more full resource envelopes (`apiVersion`, `kind`,
`metadata`, and `spec`). The daemon labels created objects as managed by the
manifest reconciler and records the relative source path and last-applied spec
digest. It fast-forwards a changed manifest only while the live spec still
matches that digest; live drift and collisions with unmanaged objects are
reported and left untouched.

This first manifest-reconciliation slice is deliberately additive: removing a
file does not delete its object, and existing unmanaged objects are never
adopted. Omit `[manifests]` to disable the loop.

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
