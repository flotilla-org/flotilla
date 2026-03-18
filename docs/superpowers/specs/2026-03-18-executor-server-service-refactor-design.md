# Executor And Server Service Refactor

## Context

`crates/flotilla-core/src/executor.rs` and
`crates/flotilla-daemon/src/server.rs` have both grown into large mixed-purpose
files.

Today each file combines public entrypoints with several distinct internal
concerns:

- `executor.rs` mixes step-plan building, immediate command execution,
  checkout lifecycle rules, workspace creation, terminal preparation, remote
  SSH attach wrapping, and attachable persistence.
- `server.rs` mixes daemon socket lifecycle, client session handling, peer
  session handling, peer replication runtime, forwarded remote command
  routing, request dispatch, and agent hook persistence.

Tests account for part of the size, but they are not the main issue. The
production code still carries too many responsibilities in one place, which
makes future protocol and topology work harder to sequence safely.

This design intentionally targets internal service boundaries first and defers
protocol cleanup until after issue `#287`, which is expected to reshape the
network/protocol model.

## Goals

- Split `executor.rs` and `server.rs` into smaller service-owned units with
  clear responsibilities.
- Keep the current user-visible functionality intact.
- Prefer behavior-preserving internal API changes over preserving accidental
  helper boundaries.
- Reduce duplicated orchestration logic so future command or protocol changes
  have one obvious implementation path.
- Make the code safer to evolve ahead of and after `#287`.

## Non-Goals

- No protocol redesign as the primary goal of this refactor.
- No functionality removal.
- No broad cleanup outside the responsibility clusters currently owned by
  `executor.rs` and `server.rs`.
- No attempt to solve all protocol cleanliness concerns before `#287`.

## Chosen Approach

Use a service-oriented refactor with stable facades.

The public entrypoint files remain, but become shallow coordinators:

- `executor.rs` keeps `build_plan`, `execute`, and `ExecutorStepResolver`
- `server.rs` keeps `DaemonServer` and top-level runtime wiring

The real behavior moves behind explicit owning services. This keeps the current
external shape understandable while making internals substantially cleaner.

This is preferred over a pure module split because the current pain is not just
file size. The larger issue is that orchestration rules, persistence rules, and
transport logic are spread across unrelated branches and duplicated in both
step-plan and immediate execution paths.

## Design

### Executor Boundaries

Refactor `crates/flotilla-core/src/executor.rs` into service-owned internal
modules under the current executor facade.

Target responsibilities:

- `CheckoutService`
  - validate checkout targets
  - create and remove checkouts
  - resolve checkout selectors
  - write branch-to-issue links
- `WorkspaceOrchestrator`
  - create or select workspaces
  - persist workspace-to-attachable bindings
  - own the reuse-vs-force-create decision
- `TerminalPreparationService`
  - parse and render workspace templates
  - resolve terminal pool sessions
  - build terminal env vars
  - produce prepared terminal commands
- `SessionActionService`
  - resolve coding-agent attach commands
  - archive sessions
  - generate branch names
- `ExecutorFacade`
  - preserve the current entrypoints
  - translate `CommandAction` into service calls or step-plan assembly

The important design rule is that plan-building and immediate execution should
share the same service operations. The current duplication around checkout
creation/removal and teleport-style workspace setup should be eliminated rather
than moved into different files.

### Server Boundaries

Refactor `crates/flotilla-daemon/src/server.rs` into runtime services with
clear ownership of state and invariants.

Target responsibilities:

- `RequestDispatcher`
  - map protocol requests to daemon operations or remote routing operations
  - stay thin and transport-agnostic
- `RemoteCommandRouter`
  - route remote execute/cancel requests
  - track forwarded and pending remote commands
  - proxy remote lifecycle events and completions
- `PeerRuntime`
  - own peer connection lifecycle, reconnect loops, resync handling, snapshot
    replication, and overlay rebuilds
- `ClientConnection`
  - own normal client request/response sessions
  - own event subscription forwarding to connected clients
- `PeerConnection`
  - own peer hello negotiation and peer wire message sessions
- `DaemonServer`
  - keep listener setup, idle timeout handling, signal handling, and task
    spawning

The key change is that `handle_client` should stop being a two-protocol
function, and `dispatch_request` should stop carrying remote routing and agent
state mutation details directly.

### Sequencing Relative To `#287`

This refactor is explicitly pre-`#287`.

The sequence is:

1. extract stable internal service boundaries now
2. preserve current wire/request behavior wherever possible
3. use the cleaner internal boundaries as the base for a later protocol-focused
   pass after `#287`

If a protocol change is needed to unblock decomposition, it should be minimal,
isolated, and justified by the boundary work. Protocol cleanliness is not the
success criterion for this refactor.

## Data Flow And Ownership

### Executor Data Flow

- `build_plan` becomes a composition layer over service methods that produce
  concrete operations or step-plan fragments.
- `execute` becomes a thin action dispatcher over the same services.
- Shared state such as attachable persistence should be owned by the service
  that maintains its invariants instead of being manipulated ad hoc by free
  helpers.
- Template parsing and terminal resolution should flow through one preparation
  path so that template fallback, env-var injection, and terminal-pool behavior
  are consistent across local and remote cases.

### Server Data Flow

- `DaemonServer` accepts connections and routes them into `ClientConnection`
  or `PeerConnection`.
- `ClientConnection` delegates request handling to `RequestDispatcher`.
- `RequestDispatcher` performs local daemon calls directly and delegates remote
  execute/cancel flows to `RemoteCommandRouter`.
- `PeerConnection` forwards peer wire messages into `PeerRuntime`.
- `PeerRuntime` owns replication, reconnect, resync, and overlay rebuild rules.
- `RemoteCommandRouter` owns pending-command maps and the translation between
  routed peer messages and local `DaemonEvent` streams.

This keeps transport concerns separate from orchestration concerns.

## Error Handling

- Preserve current command and request semantics unless a boundary change
  requires a narrowly-scoped internal API adjustment.
- Move repeated stringly error paths behind services where possible so the
  public facade branches become simpler.
- Prefer one owner for best-effort behavior such as terminal cleanup,
  workspace binding persistence, and peer replication retries.
- Do not broaden the surface area of fallback behavior during the refactor.
  If existing fallback logic is preserved, it should move intact into the new
  owning service first and only then be reconsidered in later cleanup work.

## Testing Strategy

- Move large inline test modules into submodule files for both targets to
  improve navigation immediately.
- Preserve behavior tests around public entrypoints while adding focused tests
  around the new service boundaries.
- For executor work:
  - add service-level tests for checkout/workspace/terminal flows
  - keep step-plan behavior tests at the facade boundary
- For server work:
  - add focused tests for request dispatch, remote command routing, peer
    replication, and connection session handling
  - keep integration-style daemon tests for end-to-end routing and peer flows

The refactor should produce smaller test ownership units that line up with the
new service boundaries rather than one giant test module mirroring one giant
production file.

## Risks

- Internal API extraction can accidentally preserve the current coupling under
  new names. Explicit ownership boundaries are required to avoid a cosmetic
  split.
- Service extraction may introduce temporary churn in constructor wiring and
  shared state access.
- The server refactor touches concurrency-heavy code; staging and preserving
  behavior checkpoints are important.
- Over-optimizing for final protocol design before `#287` would create churn
  with low near-term payoff.

## Success Criteria

- `executor.rs` and `server.rs` become shallow facades instead of primary logic
  containers.
- Duplicated orchestration logic in executor flows is consolidated behind shared
  services.
- `handle_client` and `dispatch_request` no longer act as mixed transport and
  orchestration hubs.
- The refactor lands in independently shippable checkpoints with tests passing
  at each stage.
- A later `#287` protocol pass can work against explicit runtime/service
  boundaries instead of monolithic files.
