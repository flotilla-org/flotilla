# Daemon And Clients

Flotilla is structured around a daemon boundary even when everything runs in one
process. That boundary is why the TUI, socket mode, and future clients can
share the same core model.

## Daemon Contract

`DaemonHandle` is the client-facing contract. It exposes:

- repo enumeration
- full current state retrieval
- command submission
- manual refresh
- repo add/remove
- event subscription
- replay since known sequence numbers

Both `InProcessDaemon` and `SocketDaemon` satisfy this contract.

## Deployment Modes

### Embedded mode

The TUI talks directly to `InProcessDaemon`. This is the simplest path and
avoids socket transport overhead.

### Socket mode

`flotilla-daemon` wraps the same daemon state with a Unix socket server, and
`flotilla-tui` talks to it through `SocketDaemon`. The transport is newline-
delimited JSON over a Unix domain socket using shared protocol types.

The intended rule is that the socket layer remains thin. Business logic belongs
in core, not in the transport adapters.

## Snapshots, Deltas, And Replay

The daemon publishes repo state as either:

- `SnapshotFull`
- `SnapshotDelta`

The daemon keeps a bounded per-repo delta log. On reconnect or sequence-gap
detection, clients can call `replay_since`:

- if missing events are still in the log, replay deltas
- otherwise fall back to a full snapshot

This keeps reconnect behavior correct without forcing full snapshots on every
update.

## Where Shared State Is Built

The authoritative snapshot assembly path is in the daemon:

1. read the latest background refresh result
2. inject cached or searched issues
3. re-run correlation against the injected provider data
4. convert to protocol `WorkItem`s
5. compare against last broadcast state to choose full snapshot vs delta

Snapshot publication is therefore more than just forwarding refresh output. The
daemon owns the final shared state seen by all clients.

## Async Command Lifecycle

Long-running commands no longer block the UI event loop end-to-end.

Current behavior:

- client submits `Command`
- daemon assigns a `u64` command ID
- daemon broadcasts `CommandStarted`
- work runs in the daemon
- daemon broadcasts `CommandFinished`
- the client updates local in-flight UI state and handles the result

This is the default shape for slow operations such as checkout creation,
branch-name generation, or workspace setup.

## What Stays Local To Clients

Clients should keep only presentation state that is not part of the shared
system model, for example:

- focus and selection (stabilized via `WorkItemIdentity` across rebuilds)
- per-client input modes and intents (intents are a UI concept; the daemon sees
  only `Command` values)
- unseen-change badges
- status text
- in-flight command rendering

One current mismatch is search: issue search results still ride in shared
snapshot state, even though search is semantically client-local. That is a
known cleanup target, not a pattern to copy.

## Design Decisions

- **In-process mode never goes away.** Embedded mode is the simplest deployment
  and avoids socket overhead. Socket mode adds multi-client support but does not
  replace embedded mode.
- **No automatic reconnection.** If the daemon dies, the TUI exits with an
  error and the user restarts. This is a deliberate simplicity choice.
- **Clients receive work items, not raw provider data.** The daemon performs
  correlation so clients stay thin. This was a deliberate choice over pushing
  raw provider data to clients and correlating per-frontend.
- **The architecture reached its current shape via a Strangler Fig migration**:
  (1) define the daemon boundary in-process, (2) add a socket server,
  (3) add delta snapshots, (4) multi-host (future). Intermediate structure from
  earlier steps may still be visible in the code.

## Current Pressure

The TUI still re-applies deltas, re-correlates provider data, and rebuilds
table state client-side. That keeps the client simple, but it is also where
current performance pressure is accumulating. If repo scale or additional
frontends make that too expensive, the next step is to move more materialized
state ownership to the daemon.
