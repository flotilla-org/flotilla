# Repo Identity And Command Affinity Design

## Summary

This design updates the open remote checkout / terminal prep branch so it is correct across hosts with different local repo roots, and so command routing stays aligned with actual provider ownership.

The previous branch work proved the basic remote-command path, but it still assumes repo paths are globally meaningful. That is false in a multi-host setup. The correct boundary is:

- repo identity is the stable cross-host key
- host-local filesystem paths remain metadata owned by each host
- command routing uses execution-host semantics only where the target daemon can actually service the command

This design folds issue `#298` into PR `#334` and fixes the two review regressions discovered afterward.

## Problems To Fix

### 1. Remote commands still use host-local paths as cross-host selectors

The TUI currently forwards repo references using the active local repo path. The remote daemon still resolves commands by its own tracked filesystem paths. That means remote checkout creation, branch-name generation, and terminal preparation only work when both hosts happen to use the same absolute repo root.

This is the core architectural bug behind review item 1. The robust fix is to move routed command selection onto `RepoIdentity`.

### 2. Some item actions are routed to the wrong host

The current branch began routing several item-backed actions to `item.host`. That host is derived from checkout correlation, not from the host that owns the provider implementation for browser/API actions like open PR, open issue, link issues, close PR, or archive session.

On follower hosts those providers are intentionally absent. So those actions must not be routed to follower daemons just because the selected work item is anchored to a follower checkout.

### 3. Terminal prep follow-up loses originating repo affinity

`TerminalPrepared` currently triggers a follow-up workspace command using the TUI's current active repo when the async result arrives. If the user changes repos while the command is in flight, the workspace can be created against the wrong repo.

The fix is to preserve originating repo identity through the result path and queue the follow-up against that explicit repo, not the current tab.

## Scope

### In scope

- Add repo-identity-based command selection for cross-host routing
- Re-key daemon and client state where repo identity must be the stable key
- Preserve host-local paths as metadata for local actions and display
- Fix item-action routing so only true execution-host actions are remote-routed
- Preserve repo affinity through terminal-prep follow-up
- Repair tests so the routed multi-host cases cover different local and remote repo roots

### Out of scope

- Session handoff / migration (`#275`)
- New host-picker UI beyond the already-added target host status selector
- Full provider-ownership modeling for every work-item type
- Removal of path-based local-only APIs that are not involved in routed execution yet

## Design Principles

### Repo identity is the cross-host key

`RepoIdentity` is the only selector that is stable across hosts. Paths are host-local implementation details. Protocol objects and routed commands that need to refer to "the same repo" across machines must carry identity.

### Paths remain first-class local metadata

The daemon, TUI, and workspace managers still need host-local paths for:

- display
- local file actions
- workspace creation
- provider execution on the owning host

So the fix is not "delete paths." It is "stop using paths as the cross-host identity key."

### Execution host and presentation host are different concerns

The selected target host continues to determine where execution-host actions run. But browser/API actions remain presentation-host concerns unless there is explicit evidence that the provider is owned remotely.

### Preserve affinity through async boundaries

Any async result that triggers follow-up commands must carry enough identity to reconstruct the exact originating repo and host context without consulting mutable UI state.

## Approach

Use `RepoIdentity` as the canonical repo key through protocol, daemon state, and TUI state, while keeping each repo's current host-local `PathBuf` in repo metadata.

Concretely:

- add `RepoSelector::Identity(RepoIdentity)`
- add repo identity to wire snapshot and repo event types that currently only carry `PathBuf`
- re-key daemon tracked repos, replay state, and TUI repo/tab/UI maps by `RepoIdentity`
- keep current path in `RepoInfo`, `Snapshot`, repo model state, and command results where local follow-up still needs it
- add originating repo identity to `CommandResult::TerminalPrepared`

This is the broadest of the three options considered during review, but it is the only one that fixes routed execution honestly rather than papering over one command path at a time.

## Protocol Changes

### Repo selectors

`RepoSelector` gains:

- `Identity(RepoIdentity)`

Existing path and query selectors remain for local-only flows and backwards-compatible command construction inside the same process. Routed commands should prefer identity.

### Repo-bearing daemon events

The following protocol types should carry repo identity as the stable key:

- `RepoInfo`
- `Snapshot`
- `SnapshotDelta`
- `DaemonEvent::RepoRemoved`
- `DaemonEvent::CommandStarted`
- `DaemonEvent::CommandFinished`
- `DaemonEvent::CommandStepUpdate`

Each should keep the host-local path as metadata where consumers still need it. The rule is:

- identity = key
- path = local detail

### Terminal prep result

`CommandResult::TerminalPrepared` should include the originating `RepoIdentity`.

That makes the follow-up workspace command deterministic even if the user changes tabs before the result arrives.

## Core Daemon Design

### InProcessDaemon repo state

`InProcessDaemon` already has an identity map bridge. This change makes identity the primary key instead of an auxiliary lookup.

Re-key the relevant daemon maps by `RepoIdentity`, including:

- tracked repos
- repo order
- replay sequence tracking
- command lifecycle repo association

Each repo state still stores:

- current host-local path
- labels
- provider health
- provider data and snapshots

Local APIs that still accept paths can resolve them through identity/path lookup helpers internally.

### Command resolution

For routed commands:

- resolve `RepoSelector::Identity` directly
- reject routed path selectors where no matching local path exists on the target host

For locally initiated commands:

- existing path/query selectors can continue to resolve as before
- identity selectors should be preferred when available

## TUI / Client Design

### Repo model keys

The TUI currently keys repos, UI tab state, and replay bookkeeping by `PathBuf`. That breaks as soon as a repo must be identified consistently across hosts while its local path differs.

Re-key these structures by `RepoIdentity`, including:

- `TuiModel.repos`
- `repo_order`
- `UiState.repo_ui`
- daemon replay sequence tracking
- in-flight command repo association

Each `TuiRepoModel` continues to store the local path for display and local filesystem actions.

### Command construction

Repo-scoped commands should use the active repo's identity, not its path, when the command may be routed or when the result must be matched back to a repo across async boundaries.

That includes:

- remote checkout creation
- branch-name generation
- terminal preparation
- follow-up workspace creation after remote terminal prep

### Item action routing

Split item-backed actions into two categories:

1. Execution-host actions
   - checkout or terminal actions that must run on the host owning the filesystem/process
   - these may route to a remote target host or item-inherent host

2. Presentation/provider actions
   - open PR
   - close PR
   - open issue
   - link issues
   - archive session
   - similar browser/API actions backed by local providers
   - these stay on the presentation host for now

This removes the regression where follower checkout items hijack provider-backed actions to a daemon that cannot service them.

### Terminal prep follow-up

When `TerminalPrepared` arrives:

- read the repo identity from the result payload
- resolve the corresponding repo model directly
- queue `CreateWorkspaceFromPreparedTerminal` against that explicit repo identity/path

Do not rebuild the next command from the currently active repo tab.

## Testing Strategy

### Protocol and daemon

- serialization roundtrips for `RepoSelector::Identity`
- daemon-event roundtrips for repo-identity-bearing events
- daemon/unit tests for resolving identity selectors and preserving path metadata

### Multi-host integration

Add or update multi-host tests so leader and follower use different absolute repo roots. The routed test must prove:

- remote checkout creation works despite differing local/remote roots
- remote branch-name generation works with identity-based repo selection
- remote terminal preparation works with identity-based repo selection

### TUI

- repo/tab state remains stable when snapshots use differing host-local paths
- provider-backed item actions stay local
- execution-host item actions still route correctly
- terminal-prep follow-up uses originating repo identity, not active-tab state

## Migration / Compatibility Notes

This is an internal protocol used by the same workspace's daemon/client code, so there is no long-term compatibility requirement with older builds in this branch. The change can therefore be a clean same-branch migration rather than a backwards-compat shim.

## Outcome

After this change:

- remote repo-executed commands no longer depend on path coincidence across hosts
- provider-backed browser/API actions stop routing to follower daemons that cannot service them
- terminal prep follow-up is stable under repo switching
- PR `#334` becomes honest about cross-host execution semantics instead of relying on same-path assumptions
