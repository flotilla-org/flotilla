# Workspace Managers

Workspace creation and selection are abstracted behind the
`WorkspaceManager` trait, but the underlying tools do not all model workspaces
the way Flotilla would ideally like them to. This document records both the
current contract and the major mismatches in the adapters.

## Current Contract

The current workspace-manager boundary is intentionally thin:

- list existing workspaces
- create a workspace from a rendered template/config
- select an existing workspace

Template parsing and variable substitution live in core. Provider-specific pane
creation, tab selection, and command dispatch live in the workspace-manager
implementations.

## Templates

Core supports a small YAML workspace template model:

- panes with names
- parent/split relationships
- surfaces with commands
- variable substitution such as `{main_command}`

This gives Flotilla one portable representation for workspace creation even when
the underlying tools use different native formats. Native provider-specific
formats can still be layered on later.

## Current Adapters

Detection is based primarily on the current shell environment so Flotilla
prefers the terminal multiplexer it is actually running inside.

| Flotilla concept | cmux | tmux | zellij |
|---|---|---|---|
| **Workspace** | workspace | window | tab |
| **Pane** | pane (split) | pane (split) | pane (split) |
| **Surface** | surface (tab in pane) | extra split (degraded) | stacked pane |
| **Multi-surface** | native tabs | becomes additional splits (warning logged) | native stacking |
| **Detection** | `CMUX_SOCKET_PATH` | `TMUX` | `ZELLIJ` |
| **Version req** | none | none | >= 0.40 |
| **State file** | none | `~/.config/flotilla/tmux/{session}/state.toml` | `~/.config/flotilla/zellij/{session}/state.toml` |

tmux has no tabbed or stacked pane concept, so multiple surfaces in a single
pane degrade to additional splits. This is a known mismatch, not the desired
behavior.

## Multi-Checkout Gap

A workspace manager (cmux, tmux, zellij) reports its workspaces with a list of directories. Each directory becomes a `CorrelationKey::CheckoutPath`, which the correlation engine uses to merge items into groups.

When a workspace references multiple checkout paths, those paths can pull multiple distinct checkouts into a single correlation group. The resulting `WorkItem` can only represent one checkout (one `CheckoutRef`). Today we pick the first checkout encountered and discard others.

This means: if a user has a cmux workspace with panes open in two different worktrees, only one worktree's data appears on the correlated row.

## Ideal Workspace Manager

A workspace manager that understands its purpose would:

1. **Know which checkout is primary** -- a workspace is "about" one branch/worktree, even if it has panes open elsewhere (e.g. the main repo for reference).
2. **Provide event streams or sync RPCs** -- report workspace state changes in real time rather than requiring polling with fixed sleeps.
3. **Report readiness** -- signal when panes/surfaces are ready to receive input, eliminating the need for fixed-delay sleeps during creation.

## Current Limitations

None of cmux, tmux, or zellij directly model the concept of a "primary working directory" for a workspace. We infer it from the directory list. This is a fundamental mismatch between the workspace manager abstraction and the tools available today.

`tmux` and `zellij` also currently rely on fixed sleeps while building pane
layouts. That is a practical adapter workaround, not the desired contract.

Detached daemon mode creates another pressure point for `cmux`: access control
is tied to process ancestry, so a daemon that outlives the original TUI process
may lose authority to drive the workspace manager.

## Design Pressure

When building new workspace manager integrations or improving existing ones:

- Prefer providers that can distinguish primary vs auxiliary directories.
- Push for event-based or callback-based readiness signals over fixed sleeps.
- Document gaps between the ideal contract and what the provider actually supports, so we have ammunition to request improvements upstream or in our own adapters.
- Treat native template support as additive polish, not a reason to remove the
  portable logical workspace model.
