# Session Daemon Terminal Pool: Issue #387

Replace shpool as the preferred persistent terminal backend with a Rust session daemon that can preserve detached terminal sessions and grow into higher-fidelity reattach via pluggable VT engines.

## Context

Flotilla currently uses shpool through the `TerminalPool` abstraction. That seam is already good enough to support a backend swap, but shpool has structural limits for modern TUIs:

- it depends on input interception for detach behavior
- it has weak terminal state tracking on reattach
- it concentrates all sessions behind a single daemon

Issue #387 proposes a daemon-per-session design, inspired by tools like zmx, with a richer long-term VT state model. The implementation should start in-tree in this workspace, but the architecture should remain generic enough to move to its own repo later if it proves broadly reusable.

## Goals

- Add a new preferred `TerminalPool` implementation without disturbing the rest of flotilla's executor flow.
- Use one daemon process per session for fault isolation and simpler lifecycle ownership.
- Keep the CLI stable while allowing the daemon socket protocol to evolve internally.
- Introduce a `VtEngine` abstraction from the start, with a phase-1 `passthrough` implementation and richer engines behind feature flags.
- Preserve room for future observer and control channels without forcing symmetric multi-client semantics now.

## Non-goals

- Solving full VT replay and restore in phase 1.
- Exposing a stable public daemon socket protocol in phase 1.
- Designing true peer multi-client input ownership in phase 1.
- Committing now to splitting the daemon into a separate repository.

## Recommended approach

Design the full target architecture now, but implement it in phases.

This gives the daemon, CLI, `TerminalPool`, and `VtEngine` seams the right shape before code hardens around the phase-1 behavior. The key risk is not swapping shpool out; the existing `TerminalPool` trait already covers that. The key risk is choosing boundaries that later fight replay, attach policy, and future non-foreground control paths.

## Architecture overview

The system has two layers:

1. A new in-tree session daemon crate that owns PTY lifecycle, session discovery, client attachment, and VT-engine integration.
2. A new `TerminalPool` adapter in `flotilla-core` that translates flotilla terminal lifecycle into daemon CLI commands and is preferred over shpool when available.

Flotilla talks to the daemon through the CLI, not by binding directly to the daemon socket protocol. The CLI is the stable public interface. The Unix-socket protocol between the CLI and daemon is internal and may evolve while the design settles.

## Why daemon-per-session

Daemon-per-session is the preferred model because it optimizes for correctness and isolation over process count:

- one wedged session affects only one terminal instead of every session
- one daemon owns one PTY, one current size, one replay engine, and one runtime directory
- stale cleanup is local to a single session directory
- future observer/control features fit naturally into an already isolated session boundary

The tradeoff is higher process and memory overhead, especially once richer VT engines maintain scrollback and mode state. That is acceptable here because issue #387 exists primarily to reduce fragility and improve terminal correctness, not to minimize background process count.

## Session identity

Session identity is daemon-owned and generic.

The CLI should support both:

- caller-supplied stable names
- daemon-generated IDs for unnamed sessions

Flotilla must not depend on semantic names. The new `TerminalPool` adapter may provide stable names if convenient, or it may persist daemon-generated IDs in the attachable store and treat them as opaque handles. The daemon itself should not know or care about flotilla-specific naming conventions.

## Session runtime layout

The daemon runtime root contains one directory per session. A session directory is the unit of lifecycle ownership and discovery.

Example shape:

- `$XDG_RUNTIME_DIR/bollards/<session-id>/socket`
- `$XDG_RUNTIME_DIR/bollards/<session-id>/meta.json`
- optional future files such as `pid`, `log`, or engine-specific artifacts

This layout keeps session artifacts together and narrows creation races to the initial session directory allocation. Discovery becomes filesystem-first: enumerate session directories, probe their sockets, ignore broken entries, and reap stale sessions conservatively.

## CLI surface

The stable interface is a CLI with these commands:

- `attach`
- `create`
- `list`
- `kill`

`attach` is the default user and flotilla path. It creates the session if needed, otherwise attaches to the existing one. If a session is created without an explicit command, it starts the user's login shell.

`create` exists for tests, tooling, and future warm-session behavior.

`list` returns structured session metadata suitable for terminal-pool discovery.

`kill` terminates the session cleanly and removes its runtime directory.

Phase 1 does not require an in-band detach key or self-detach command. Detach is achieved by outer client disconnect behavior.

## Daemon-client protocol

The daemon listens on a Unix socket inside each session directory. The client and daemon communicate with a framed internal protocol that carries:

- client hello and capability negotiation
- PTY input bytes from the foreground client
- PTY output bytes from the daemon
- resize requests
- metadata and control operations
- engine-driven replay payloads on reattach when available

The exact wire format is an implementation detail in early phases. The important architectural decision is that the protocol is private while the CLI remains stable.

## Client roles and resize policy

The daemon models one foreground interactive client at a time.

That client owns:

- live keyboard input
- authoritative terminal size while attached

On disconnect, the daemon freezes the last known size.

This intentionally asymmetric model is simpler and more correct than pretending all clients are peers. It also leaves room for future roles:

- observer clients that receive output or replay
- control clients that can inspect or manipulate the session
- future takeover behavior where another client becomes foreground behind an explicit UX boundary

If a second interactive client attempts to attach while one is active, the daemon should reject it clearly until richer takeover semantics are designed.

## VT engine model

The daemon owns a `VtEngine` trait from day one, even though phase 1 uses a minimal implementation.

The trait should cover engine capabilities, not just parsing. It needs to:

- consume PTY output incrementally
- track dimensions
- report whether replay is supported
- produce a restore payload for reattach when supported

Initial engines:

- `passthrough`: phase-1 default, no replay support
- `ghostty` / `libghostty-vt`: intended long-term primary engine behind a feature flag
- optional pure-Rust fallback later behind a separate feature flag

Replay is engine-dependent. On reattach, the daemon asks the current engine whether it can generate a restore payload. If yes, that payload is sent before live PTY output resumes. If not, the client receives only future live output.

This keeps replay behavior out of daemon phase branching and lets all engines share the same behavioral contract.

## Flotilla integration

Flotilla gets a new `TerminalPool` backend that is preferred when the session-daemon CLI is available, with shpool retained as fallback.

The new adapter is responsible for:

- probing daemon CLI availability
- mapping flotilla terminal lifecycle into `create` / `attach` / `list` / `kill`
- persisting whatever stable external handle it needs in the attachable store
- returning the attach command string executed by workspace managers

The rest of the executor flow should remain unchanged. The current `TerminalPool` seam is already sufficient for the backend replacement.

## Failure handling

Failure handling should stay local to each session:

- foreground client disconnect keeps the session alive
- child process exit tears the session down and removes its runtime directory
- stale session directories are reaped conservatively when sockets are dead or unresponsive
- daemon failures affect only the owning session

No central registry should be introduced. Filesystem discovery plus per-session probing keeps shared failure modes small.

## Implementation phases

### Phase 1: session daemon foundation

- add new in-tree daemon crate and stable CLI
- implement daemon-per-session PTY lifecycle
- use per-session runtime directories and Unix sockets
- add `passthrough` `VtEngine`
- support `attach`, `create`, `list`, and `kill`
- add new preferred `TerminalPool` adapter in flotilla-core
- keep detach as outer-client disconnect only

Phase 1 replaces shpool for basic persistence but does not promise replay fidelity.

### Phase 2: replay-capable engine integration

- add feature-gated richer VT engine implementation
- feed PTY output into the engine
- generate restore payloads on reattach when supported
- add engine-focused behavior tests shared across implementations

### Phase 3: hardening and extended control

- improve diagnostics and stale cleanup
- support richer metadata or info queries
- prepare for observer/control-side channels and future takeover policies
- support agent-driven TUI testing and automation workflows that need structured snapshots and targeted input without relying on tmux screen-scraping
- revisit whether the daemon should remain in-tree or split into its own repo

## Testing

Testing should follow behavior seams, not only subprocess realism:

- unit tests for runtime directory allocation, stale cleanup, and CLI argument behavior
- daemon lifecycle tests for create, attach, disconnect, list, and kill
- `VtEngine` contract tests, initially against `passthrough`, then against richer engines
- `TerminalPool` integration tests in `flotilla-core` covering discovery preference, attach command generation, stable handle persistence, and list/kill mapping
- in-process daemon-style tests wherever possible before relying on end-to-end PTY integration for every scenario

### External terminal test suites and reference corpora

The design should plan to reuse existing terminal-emulation test assets where practical rather than inventing a corpus from scratch.

- `vttest` is the long-lived baseline suite for VT100, VT220, VT420, VT520, and xterm-oriented terminal behavior. It is old, but still actively maintained and remains the standard compatibility smoke test for display and keyboard behavior.
- `esctest` is a more automated terminal-emulation test suite from the freedesktop terminal working group. It is a better fit for CI-style sequence validation than `vttest`, which is menu-driven and more interactive.

For sequence selection and expected behavior research, the most useful current references are:

- the xterm control-sequences reference, which remains the de facto compatibility baseline for many terminal features
- Ghostty's VT reference, which is useful when deciding modern sequence coverage and future `libghostty-vt` alignment

Recommended testing strategy:

- use internal contract tests as the primary regression suite for daemon and `VtEngine` behavior
- use `esctest` first at the `VtEngine` seam, where terminal input/output behavior can be validated without the full daemon lifecycle in the way
- add a narrower end-to-end `esctest`-style layer later for daemon attach, detach, and replay-path validation once reattach fidelity exists
- use `vttest` as a supplemental manual or scripted compatibility sweep, especially for baseline VT/xterm behaviors and keyboard handling
- treat xterm and Ghostty documentation as the reference inputs for deciding which controls are in scope for phase 2 and later

## Files and components likely involved

Likely new or changed areas:

- new crate for the session daemon and CLI
- `crates/flotilla-core/src/providers/terminal/` for the new backend adapter
- `crates/flotilla-core/src/providers/discovery/factories/` for preferred-provider discovery
- attachable-store integration for stable daemon session handles
- tests in `flotilla-core` and the new daemon crate for lifecycle and contract behavior

## Open questions intentionally deferred

- the final binary and crate naming
- the exact internal socket wire format
- the first richer VT engine to land after `passthrough`
- whether future control and observer channels share one socket protocol or split into separate endpoints
- when, if ever, the daemon should be extracted into its own repository
