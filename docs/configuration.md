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

## Dispatch-time agent selection

Workflow templates name *capabilities* (`code`, `code-review`), never
harnesses. `flotilla convoy start` can bind a capability to a specific agent
harness for that one dispatch:

```sh
flotilla convoy start --project flotilla --issue 1234 \
    --agent claude-code:opus --no-attach

flotilla convoy start --project flotilla --issue 1234 \
    --agent claude-code:sonnet --agent review=codex --no-attach
```

The flag is `--agent [capability=]adapter[:model]` and is repeatable, once per
capability. The bare form applies to the `code` capability — the one every
stock coding workflow's crew selects on — so `--agent claude-code:opus` is
shorthand for `--agent code=claude-code:opus`. Without an override, a
capability resolves through the seeded table: `code` and `coding` to `codex`,
`review` and `code-review` to `claude-code` on `opus`.

Semantics worth knowing:

- The override is written into the convoy's workflow snapshot at admission, not
  carried alongside it. Placement validation, the vessel reconciler, and
  terminal launch all read the same effective requirement from the snapshot's
  selector; the template itself stays capability-only.
- An adapter override does not inherit the seeded capability table's model. The
  seeded `review` capability pairs `claude-code` with `opus`, but
  `--agent review=codex` launches codex with no model, because a model name
  only means something against the harness it was chosen for. Name the model
  explicitly when you want one.
- Placement admission checks the *effective* adapter. Dispatching to a host
  that does not have it is refused, and the refusal names the adapter:
  "workflow requires agent adapter `claude-code`, which is not available in
  placement `lab-feta`".
- Overriding a capability that no agent crew in the workflow selects is
  refused, and the refusal lists the capabilities the workflow does carry.
- Adapter and model tokens are restricted to alphanumerics, `.`, `_`, and `-`.

Reach for it when coding work should run on a different harness than the
default — most often when one subscription's weekly budget is spent and the
work should move to another (the budget direction in
[#1394](https://github.com/flotilla-org/flotilla/issues/1394)). Harness and
model are separate axes: `--agent claude-code:opus` and
`--agent claude-code:sonnet` pick the same harness at different cost.

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

## Credential health

Each daemon's heartbeat probes expiry metadata for held credential material
(the ambient claude login today; declared credentials as adapters learn to
express expiry) and publishes it on the Host resource — timestamps and scope
names only, never material. Expired material refuses dependent dispatch at
admission; expired *and* near-expiry material surfaces in `flotilla host list`
and the TUI fleet health pane. Override the near-expiry warning window
(default 7 days) per host in that host's `~/.config/flotilla/daemon.toml`:

```toml
[credentials]
warning_window_days = 14
```

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
