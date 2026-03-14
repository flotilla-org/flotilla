# Remote Repo Identity And Command Affinity Design

## Summary

PR `#334` exposed three linked problems in the first remote checkout / terminal-prep implementation:

1. routed commands still identify repos by host-local absolute path
2. some provider-backed item actions are incorrectly routed to `item.host`
3. `TerminalPrepared` follow-up workspace creation loses repo affinity if the user switches tabs mid-flight

These are not three independent bugs. They come from the same boundary problem: the daemon, protocol, client, and TUI still treat `PathBuf` as the stable repo identifier, even though multi-host routing already uses `RepoIdentity` as the cross-host concept.

This design fixes that boundary honestly by implementing the essence of `#298` now: use `RepoIdentity` as the stable repo key across daemon state, replay bookkeeping, command routing, and TUI tab state, while retaining per-host paths as daemon-local metadata for display, execution, and multi-root discovery.

## Scope

### In scope

- Add `RepoIdentity` to the wire model anywhere repos need stable cross-host identity
- Re-key `InProcessDaemon` repo state, peer overlays, and replay bookkeeping by `RepoIdentity`
- Re-key TUI repo state, tab ordering, UI state, and client replay bookkeeping by `RepoIdentity`
- Add `RepoSelector::Identity` and use it for remote-targeted command routing
- Fix provider-backed item actions so they execute on the presentation host unless the command is truly target-host-owned
- Preserve repo affinity across async `TerminalPrepared` result handling
- Add tests that cover different local/remote repo roots and tab switches during terminal preparation

### Out of scope

- Session handoff / migration (`#275`)
- Host/provider ownership modeling beyond the immediate routing fix
- Full remote attach over the multiplexed peer socket
- Any further terminal pool work beyond preserving the current passthrough design

## Core Decision

### Stable identity is `RepoIdentity`, not path

Local paths are host-relative facts. The same repo can exist at:

- `/Users/robert/dev/flotilla` on the presentation host
- `/srv/dev/flotilla` on a remote Linux host
- a synthetic path for remote-only tabs

Those are not safe cross-host selectors. `RepoIdentity` already is.

So the system should work like this:

- `RepoIdentity` identifies a repo across hosts
- each daemon persists and refreshes all tracked local roots for an identity, plus any synthetic remote-only representation it needs for UI continuity
- commands that need local filesystem access resolve from identity to an explicit local instance inside the executing daemon instead of relying on path-keyed last-writer-wins behavior
- the TUI tracks tabs and in-flight commands by identity, not by current path

## Protocol Changes

### Repo-bearing protocol types carry identity

Add `identity: RepoIdentity` to:

- `RepoInfo`
- `Snapshot`
- `SnapshotDelta`

Change repo-bearing daemon events to use identity as the stable key:

- `RepoRemoved` should identify the removed repo by `RepoIdentity`
- `CommandStarted`
- `CommandFinished`
- `CommandStepUpdate`

Paths can still remain present where useful for human-facing output, but identity must be the field that consumers use for indexing and correlation.

### Routed commands use `RepoSelector::Identity`

Extend `RepoSelector` with:

- `Identity(RepoIdentity)`

Remote-targeted commands should use this selector instead of sending a presentation-host path to the remote daemon.

That applies to:

- remote checkout creation
- remote branch-name generation
- remote terminal preparation

Local-only commands may continue using path selectors where appropriate.

### Terminal preparation result carries originating repo identity

`CommandResult::TerminalPrepared` needs the originating repo identity, not just branch / checkout path.

That lets the TUI queue the follow-up workspace command against the initiating repo even if the active tab changes before the async result arrives.

## Daemon Design

### InProcessDaemon re-keying

`InProcessDaemon` should move from:

- `repos: HashMap<PathBuf, RepoState>`
- `repo_order: Vec<PathBuf>`
- `peer_providers: HashMap<PathBuf, ...>`

to identity-keyed structures:

- `repos: HashMap<RepoIdentity, RepoState>`
- `repo_order: Vec<RepoIdentity>`
- `peer_providers: HashMap<RepoIdentity, ...>`

`RepoState` should store the daemon-local instances for that identity:

- all tracked local repo roots that share the identity
- whichever per-identity local instance is currently preferred when one concrete path is needed for execution-oriented behavior such as creating a new worktree
- synthetic path data for remote-only tabs where no local root exists

This removes the current identity-to-path bridge awkwardness, preserves multi-clone local discovery, and makes routed command resolution match peer replication semantics.

### Repo resolution rules

When a command executes on a daemon:

- `RepoSelector::Identity` resolves directly to repo state by identity
- `RepoSelector::Path` remains valid for local commands and compatibility paths
- local filesystem execution uses an explicit preferred local instance from that daemon’s `RepoState`
- adding multiple local clones with the same identity must remain stable and deterministic rather than collapsing into whichever path was inserted last

This is what fixes the “same repo must exist at the same absolute path on both hosts” bug.

### Replay and event affinity

Replay tracking should also key by identity.

That means:

- `replay_since()` takes last-seen seqs keyed by `RepoIdentity`
- snapshot replay and delta replay use repo identity as the stable index
- add/remove and command lifecycle events identify repos by identity

The client and TUI can then survive path changes or differing roots without losing continuity.

## TUI And Client Design

### TUI repo state is identity-keyed

The TUI should move from path-keyed repo maps and tab ordering to identity-keyed structures:

- `repos: HashMap<RepoIdentity, TuiRepoModel>`
- `repo_order: Vec<RepoIdentity>`
- `UiState.repo_ui: HashMap<RepoIdentity, RepoUiState>`
- provider status maps keyed by repo identity
- in-flight command tracking keyed by repo identity

`TuiRepoModel` should store both:

- stable `RepoIdentity`
- enough local instance metadata to render the current display path and preserve deterministic tab behavior when multiple local roots share one identity

This preserves the existing tab/UI behavior while making async and multi-host logic stable.

### Client replay bookkeeping is identity-keyed

`SocketDaemon` currently tracks seqs by `PathBuf`. That must switch to `RepoIdentity`.

Otherwise replay recovery will remain tied to host-local paths and can drift when the daemon becomes identity-keyed.

### Fixing provider-backed item actions

The current `item.host` routing is wrong for actions whose implementation lives on the presentation host, not on the checkout anchor host.

Immediate routing rule:

- execution-host actions stay target-host or item-host routed
  - checkout creation
  - terminal preparation
- provider-backed browser/API actions stay on the presentation host by default
  - open/close PR
  - open issue
  - link issues
  - archive session

This is intentionally conservative. It matches the current provider registration model, where follower daemons omit those providers.

If we later want true per-item provider ownership routing, that should be modeled explicitly instead of inferred from checkout host.

### Fixing async terminal-prep follow-up

When `TerminalPrepared` arrives:

- the TUI must preserve the originating repo identity from the command result or in-flight record
- it must not rebuild the next command from the active tab

The follow-up local workspace creation command should therefore be queued against the repo identity that initiated preparation, even if the user has switched to another tab.

## Testing Strategy

### Protocol

- roundtrip tests for `RepoSelector::Identity`
- roundtrip tests for repo-bearing structs/events with identity fields
- roundtrip tests for `TerminalPrepared` preserving repo identity

### Core daemon

- tests that `RepoSelector::Identity` resolves correctly
- replay tests keyed by identity instead of path
- `InProcessDaemon` tests for add/remove/get-state/list/replay after the re-key
- multi-root tests showing two tracked local clones with the same identity both remain discoverable and deterministic

### Multi-host / server

- routed remote checkout test with different local and remote repo roots
- routed branch-name / terminal-prep repo resolution tests using identity selectors
- remote terminal-prep response test preserving originating repo identity

### TUI / client

- target-host checkout intent stamps identity-based repo selector for remote execution
- provider-backed item actions remain local despite remote checkout anchors
- terminal-prep result queues follow-up workspace creation for the initiating repo identity even after tab switch
- snapshot/golden updates for any status-bar or tab-label differences that remain intentional

## Migration Notes

This is a refactor of the current branch, not a follow-up feature branch.

So the implementation should:

- update existing remote checkout / terminal-prep code to the new identity model
- preserve all configured local roots for discovery and refresh, even when they share a `RepoIdentity`
- keep public behavior aligned with the current feature intent
- preserve deterministic test-support behavior introduced earlier

The PR should not be merged until:

- the remote routed commands work with different repo roots across hosts
- provider-backed item actions are no longer misrouted to follower-only hosts
- async terminal-prep follow-up is identity-stable across tab switches
