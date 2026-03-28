# High-Fidelity TUI Integration Testing

**Date**: 2026-03-27
**Issue**: #541
**Status**: Approved

## Summary

Flotilla currently has strong widget tests, snapshot tests, and daemon integration tests, but it lacks a high-fidelity harness for TUI workflows. That gap means agents can break real user flows while still preserving line coverage and passing narrowly scoped tests.

This design adds a single-process TUI integration harness that uses the real `App`, the real client/server daemon comms path, and a realistic two-daemon topology, while keeping providers and transports deterministic below the external-service boundary. The goal is to catch regressions in command dispatch, remote routing, progress propagation, and visible TUI feedback without paying the cost of full process or Docker-based end-to-end tests.

## Goals

- Exercise real TUI workflows through the same command and event paths used in production socket mode.
- Use a realistic topology: one TUI client, one leader daemon, and one follower daemon.
- Keep provider behavior deterministic and harness-controlled.
- Catch regressions in remote command routing and progress visibility.
- Prefer a narrow initial functionality slice over broad feature coverage.

## Non-Goals

- Full binary end-to-end tests with subprocess daemons, sockets, SSH, or a real terminal.
- Replacing existing widget, snapshot, or provider contract tests.
- Covering every TUI feature in the first version of the harness.
- Making full-render assertions the primary testing strategy.
- Solving embedded-mode remote routing as part of this harness work.

## Design

### Test Layer

This harness sits between widget tests and full end-to-end tests.

It is more realistic than current TUI tests because it uses:

- a real `App`
- a real client-side `DaemonHandle`
- real server-side request dispatch and remote command routing
- real peer routing between daemons
- real daemon event subscription and application

It is lighter than full end-to-end tests because it still uses:

- fake discovery
- fake providers
- an in-memory transport in place of Unix sockets
- direct control of async provider behavior from the test harness

The realism axis is topology and message flow. The narrowness axis is the user functionality under test.

### Transport Boundary

The key correction is that the TUI harness must not bind the app directly to `InProcessDaemon`.

Production TUI socket mode currently works through:

- `App`
- client-side `DaemonHandle`
- framed `Message` traffic
- server-side request dispatch and `RemoteCommandRouter`
- `InProcessDaemon`
- peer runtime between daemon instances

The harness should preserve that shape exactly, except that the client/server connection uses an in-memory transport rather than a real Unix socket.

This requires extracting a reusable transport/session layer so both production sockets and tests can supply the same client/server message session semantics. The harness target is therefore:

- real `App`
- real client/server comms channel
- fake transport for that channel
- real server-side request dispatch and remote routing
- real in-memory peer transport between daemons
- fake stepped providers underneath

### Architecture

The harness should build the following components inside one test process:

- one leader `InProcessDaemon`
- one follower `InProcessDaemon`
- one real client-side daemon handle connected to the leader through an in-memory `Message` session
- one real TUI `App` connected to that daemon handle through `Arc<dyn DaemonHandle>`
- a real in-memory peer connection between the two daemons using the existing channel transport and peer runtime

The TUI must not receive synthetic daemon events. It subscribes through the real client-side handle and updates through the same app logic used in production.

The fake layer belongs below the daemon boundary. Discovery and providers are seeded with deterministic test fixtures so the daemons behave like a real deployment without depending on external tools or services.

### Transport Extraction

The reusable extraction should happen in two layers.

First, add a generic in-memory bidirectional session primitive parameterized over message type. This captures the reusable subset already present in the peer test transport pattern:

- paired endpoints
- send/receive channels
- disconnect/close semantics
- session lifecycle suitable for test transports

Second, add a `Message`-typed session abstraction on top of that primitive for client/server comms. Production code can adapt Unix sockets into this session type, and tests can adapt in-memory channel endpoints into the same session type.

This keeps the real business logic above the transport seam and gives future record/replay work a semantically meaningful place to observe or replay traffic.

### Placement And Dependency Injection

This transport/session code should live in a small shared crate, `flotilla-transport`.

Its responsibility should stay narrow:

- generic in-memory bidirectional session primitive
- `Message`-typed session API
- socket-backed `Message` session adapters where needed

It should not own:

- `DaemonHandle`
- `InProcessDaemon`
- request dispatch logic
- peer manager logic

`flotilla-client` should depend on it for client construction, and `flotilla-daemon` should depend on it for server request handling. Dependency injection should happen at the message-session boundary so production and test code paths remain architecturally similar rather than diverging into test-only mocks.

### Harness Responsibilities

The shared harness should:

- construct the two-daemon topology
- seed repo and work-item state on the appropriate host
- connect the daemons through the in-memory peer transport
- stand up a real server-side request session for the leader daemon
- construct a real client-side daemon handle bound to that session
- construct a real `App` bound to that daemon handle
- expose helpers for driving TUI actions through normal app input paths
- expose helpers for draining daemon events into the app
- expose checkpoints for stepped provider behavior
- capture ordered event logs for failure diagnostics

The harness should provide builder-style setup for common scenarios, but v1 only needs enough API to support the first remote-progress workflow.

### Provider Stepping Model

Long-running fake providers should be explicitly stepped by the harness.

Each stepped action exposes checkpoints such as:

- command started
- provider entered long-running phase
- progress became observable
- completion released

This allows tests to pause execution at meaningful intermediate states and assert on what the user would see while the remote action is still running. The harness controls provider progression; daemon events remain a consequence of normal command execution.

### First Scenario

The first implemented workflow is a remote action with visible progress in the TUI.

The scenario is intentionally narrow in functionality but realistic in topology:

1. The follower daemon owns the actionable remote state.
2. The leader TUI selects the relevant item and triggers the action through normal app behavior.
3. The client-side daemon handle sends a real execute request over the in-memory client/server transport.
4. The leader server dispatches through the real `RequestDispatcher` and `RemoteCommandRouter`.
5. The leader daemon routes the command across the in-memory peer channel.
6. The follower executes the action against a stepped fake provider.
7. Real progress and completion events flow back through the leader daemon and the client-side session.
8. The TUI updates through its real event-handling path.

The primary regression signal is visible progress or status surfacing in the TUI while the command is still running. Row-level pending state is also useful, but secondary.

## Data Flow

The expected control flow is:

1. Seed remote repo state and any required work items through fake discovery and providers.
2. Start the leader and follower daemons and connect them via the in-memory peer transport.
3. Start an in-process server-side request session for the leader daemon.
4. Create a client-side daemon handle against an in-memory `Message` session connected to that server session.
5. Create the TUI `App` against that daemon handle.
6. Drive the TUI to trigger the remote command using normal app-level input handling.
7. Let the leader server dispatch and forward the command to the follower through real peer messages.
8. Pause the follower-side stepped provider at an in-progress checkpoint.
9. Drain real daemon events through the client session into the app until progress becomes visible.
10. Assert on user-visible TUI status and, where cheap, row-level pending state.
11. Release the provider to completion.
12. Drain the remaining events and assert on final UI state.

No test should inject fake daemon events into the TUI. If a state transition matters, it should be produced by real client/server and daemon execution.

## Error Handling

The harness must make async failures explicit instead of hanging.

Every stepped checkpoint should use bounded waits and produce clear failure messages, for example:

- progress checkpoint was never reached
- remote command never finished
- expected TUI status text never became visible
- client/server message session disconnected unexpectedly

The harness should record leader and follower event streams for debugging, but those logs are secondary diagnostics, not the main assertion surface.

When an assertion fails, the test output should make it easy to determine whether the breakage is in:

- TUI input dispatch
- client/server request transport
- server request dispatch
- peer routing
- provider execution
- progress propagation
- TUI event application

## Testing Strategy

### Primary Assertions

The first tests should assert on:

- user-visible TUI progress or status while a remote command is in flight
- successful completion after the stepped provider is released
- any small final-state mutation needed to prove the workflow completed end to end

### Secondary Assertions

Where low-cost and stable, tests may also assert on:

- row-level pending or in-flight indicators
- ordered command lifecycle events captured by the harness
- final repo or work-item state after completion

### Boundaries

This harness should not try to absorb responsibilities already covered elsewhere:

- widget behavior stays in widget tests
- static rendering and layout coverage stays in snapshot tests
- provider contract behavior stays in provider tests
- process, socket, SSH, and container deployment coverage stays in full end-to-end tests
- embedded-mode parity stays a separate product/design question

## Initial Deliverable

The first milestone is one working high-fidelity regression test that proves:

- one TUI can drive a remote action through a leader-server plus follower-daemon topology
- the action routes over the real in-memory peer path
- progress is surfaced visibly in the TUI before completion
- the workflow resolves cleanly after the provider is released

Before that harness is built, the transport refactor should prove:

- client parity across request/response, pushed events, and replay handling
- server parity across request dispatch and event streaming
- in-memory message-session viability for a real client/server request path

Once that path is stable, the same harness can be extended to additional remote and local workflows that agents frequently break.
