# Extension Points

This document records the main seams the current architecture is intentionally
leaving open, along with the backlog pressure acting on them. It is not a
feature wish-list; it is a guide to where future changes are expected to land.

## Provider Expansion

The provider registry already anticipates more implementations than are shipped
today.

Near-term extension surfaces include:

- VCS and checkout backends such as `jj`, libgit2-backed git, and local clones
- remote platforms such as GitLab, Jira, and Linear
- coding agents and AI utilities such as Codex/OpenAI and generic LLM APIs
- deeper zellij workspace support

Relevant issues: `#43`, `#44`, `#45`, `#49`, `#50`, `#51`, `#52`, `#53`,
`#54`, `#55`, `#56`.

## Frontend Expansion

The daemon boundary is the main enabler for more than one client.

Expected pressure here:

- a web frontend consuming the daemon model
- more robust record/replay testing
- keeping daemon tests decoupled from TUI implementation details

Relevant issues: `#36`, `#103`, `#120`.

## Shared State Versus Client State

Some features are currently implemented in the fastest place rather than the
cleanest boundary. The main example is search:

- shared snapshots currently carry issue search results
- semantically, search belongs to a requesting client unless it becomes a fully
  shared daemon concept

When adding new features, decide this boundary explicitly instead of letting
per-client overlays leak into shared snapshot state.

Relevant issue: `#114`.

## Command Orchestration

Async command execution is daemon-owned now, but orchestration is still fairly
flat.

Expected next steps:

- richer per-row pending-state feedback
- cancellation for long-running commands
- DAG-style intent execution with partial failure and resume

Relevant issues: `#58`, `#146`, `#150`.

## Workspace Ownership And Portability

Today a workspace-manager adapter is still mostly a bridge to an external
terminal multiplexer. Longer-term pressure is toward Flotilla owning more of the
logical workspace lifecycle:

- better readiness and event feedback from workspace managers
- persistent or portable sessions
- multi-host visibility and eventual handoff
- agent-facing control surfaces

Relevant issues: `#24`, `#32`, `#33`, `#35`.
