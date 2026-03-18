# Session Daemon Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the first usable Rust session-daemon terminal backend and make flotilla prefer it over shpool for basic persistent terminal sessions.

**Architecture:** Add a new sibling workspace crate, provisionally named `bollard`, that owns session directories, CLI commands, a private daemon socket protocol, PTY lifecycle, and a `passthrough` `VtEngine`. Integrate it into `flotilla-core` through a new `TerminalPool` adapter and provider-discovery factory, keeping the existing executor flow unchanged and leaving replay-capable VT engines for later plans.

**Tech Stack:** Rust, Tokio, Unix sockets/PTTY support, `flotilla-core` provider discovery, attachable-store persistence, targeted crate tests, sandbox-safe cargo test commands

---

## Scope split

The approved spec covers multiple stages. This plan intentionally targets only phase 1:

- new sibling session-daemon crate
- `passthrough` `VtEngine`
- create / attach / list / kill lifecycle
- preferred `TerminalPool` integration in `flotilla-core`

Follow-on plans should cover:

- replay-capable VT engine integration
- external `esctest` / `vttest` adoption
- observer/control channels and agent-driven TUI automation

## File map

### New crate

- Create: `crates/bollard/Cargo.toml`
- Create: `crates/bollard/src/lib.rs`
- Create: `crates/bollard/src/main.rs`
- Create: `crates/bollard/src/cli.rs`
- Create: `crates/bollard/src/runtime.rs`
- Create: `crates/bollard/src/session.rs`
- Create: `crates/bollard/src/server.rs`
- Create: `crates/bollard/src/protocol.rs`
- Create: `crates/bollard/src/vt/mod.rs`
- Create: `crates/bollard/src/vt/passthrough.rs`
- Create: `crates/bollard/tests/cli.rs`
- Create: `crates/bollard/tests/runtime.rs`
- Create: `crates/bollard/tests/lifecycle.rs`

### Existing workspace / discovery

- Modify: `Cargo.toml`
- Modify: `crates/flotilla-core/Cargo.toml`
- Modify: `crates/flotilla-core/src/providers/terminal/mod.rs`
- Create: `crates/flotilla-core/src/providers/terminal/session.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/detectors/mod.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/mod.rs`
- Create: `crates/flotilla-core/src/providers/discovery/factories/session.rs`
- Possibly modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

### Existing executor / attachable persistence

- Modify: `crates/flotilla-core/src/executor.rs`
- Possibly modify: `crates/flotilla-core/src/attachable/mod.rs`
- Possibly modify: `crates/flotilla-core/src/attachable/store.rs`

### Design docs

- Modify: `docs/superpowers/specs/2026-03-18-session-daemon-design.md`
- Create: `docs/superpowers/plans/2026-03-18-session-daemon-phase1.md`

## Chunk 1: Workspace and crate bootstrap

### Task 1: Add the new sibling crate without touching the existing multi-host daemon crate

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/bollard/Cargo.toml`
- Create: `crates/bollard/src/lib.rs`
- Create: `crates/bollard/src/main.rs`
- Create: `crates/bollard/src/cli.rs`
- Test: `crates/bollard/tests/cli.rs`

- [ ] **Step 1: Write the failing CLI smoke tests**
  - Add tests that exercise `bollard --help`, `bollard create --help`, and `bollard list --help`.
  - Assert the crate exposes the expected top-level commands: `attach`, `create`, `list`, `kill`.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p bollard --locked cli`
Expected: FAIL because the crate does not exist yet.

- [ ] **Step 3: Add the workspace member and minimal binary crate**
  - Add `crates/bollard` to the workspace members in `Cargo.toml`.
  - Create a standalone package with its own binary entrypoint.
  - Keep it fully separate from `crates/flotilla-daemon`; do not extend the existing multi-host daemon crate.
  - Add clap-based command parsing for `attach`, `create`, `list`, and `kill`.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p bollard --locked cli`
Expected: PASS

## Chunk 2: Runtime root and session identity

### Task 2: Implement session directory allocation and opaque session IDs

**Files:**
- Create: `crates/bollard/src/runtime.rs`
- Possibly create: `crates/bollard/src/session.rs`
- Test: `crates/bollard/tests/runtime.rs`

- [ ] **Step 1: Write the failing runtime tests**
  - Add tests for runtime-root selection and fallback behavior.
  - Add tests showing each session gets its own directory.
  - Add tests for caller-supplied names versus daemon-generated opaque IDs.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p bollard --locked runtime`
Expected: FAIL because runtime selection and ID allocation are not implemented.

- [ ] **Step 3: Implement the minimal runtime layer**
  - Resolve the runtime root using `XDG_RUNTIME_DIR`, then fallback paths.
  - Allocate one directory per session.
  - Support both `create --name <name>` and `create` with generated IDs.
  - Store enough metadata to support later list/probe behavior without requiring flotilla-specific naming.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p bollard --locked runtime`
Expected: PASS

## Chunk 3: VT engine contract with passthrough phase-1 implementation

### Task 3: Introduce the phase-1 `VtEngine` seam

**Files:**
- Create: `crates/bollard/src/vt/mod.rs`
- Create: `crates/bollard/src/vt/passthrough.rs`
- Possibly modify: `crates/bollard/src/session.rs`
- Test: `crates/bollard/tests/runtime.rs`

- [ ] **Step 1: Write the failing engine contract tests**
  - Add tests that `PassthroughVtEngine` accepts output bytes and reports replay as unsupported.
  - Add tests that resizing updates engine state without producing a replay payload.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p bollard --locked passthrough`
Expected: FAIL because the trait and implementation do not exist.

- [ ] **Step 3: Implement the trait and passthrough engine**
  - Define the `VtEngine` trait around capability queries and restore-payload generation.
  - Add a no-op `PassthroughVtEngine` for phase 1.
  - Thread the engine into session state so the daemon always owns an engine instance even before replay exists.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p bollard --locked passthrough`
Expected: PASS

## Chunk 4: Session lifecycle daemon and CLI behavior

### Task 4: Implement create/list/kill around per-session directories

**Files:**
- Create: `crates/bollard/src/server.rs`
- Create: `crates/bollard/src/protocol.rs`
- Modify: `crates/bollard/src/cli.rs`
- Modify: `crates/bollard/src/main.rs`
- Test: `crates/bollard/tests/lifecycle.rs`

- [ ] **Step 1: Write the failing lifecycle tests**
  - Add tests for:
    - `create` creating a session directory and metadata
    - `list` reporting existing sessions
    - `kill` removing the session directory
  - Keep these tests at the CLI/runtime boundary first; do not block this task on PTY attach yet.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p bollard --locked lifecycle -- create`
Expected: FAIL because the lifecycle commands are still stubs.

- [ ] **Step 3: Implement the lifecycle primitives**
  - Add private protocol and server-side helpers only as needed to back the CLI.
  - Make `list` use directory enumeration plus conservative probing.
  - Make `kill` target one session and fully clean up its runtime directory.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p bollard --locked lifecycle -- create`
Expected: PASS

### Task 5: Implement PTY-backed attach with disconnect-preserves-session semantics

**Files:**
- Modify: `crates/bollard/src/session.rs`
- Modify: `crates/bollard/src/server.rs`
- Modify: `crates/bollard/src/protocol.rs`
- Modify: `crates/bollard/src/cli.rs`
- Test: `crates/bollard/tests/lifecycle.rs`

- [ ] **Step 1: Write the failing attach tests**
  - Add tests showing:
    - `attach` creates the session lazily if missing
    - re-attaching to an existing session reuses it
    - foreground client disconnect leaves the session alive
    - a second simultaneous interactive attach is rejected

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p bollard --locked lifecycle -- attach`
Expected: FAIL because attach/reuse/disconnect semantics are not implemented.

- [ ] **Step 3: Implement the minimal PTY lifecycle**
  - Spawn one session process tree per session.
  - Support `attach` with optional `--cwd` and `--cmd`, defaulting to the login shell when creating a new unnamed command.
  - Preserve the session after client disconnect.
  - Keep a single foreground interactive client policy in phase 1.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p bollard --locked lifecycle -- attach`
Expected: PASS

## Chunk 5: Discovery and preferred provider integration

### Task 6: Add binary detection and provider factory wiring for the new backend

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/detectors/mod.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/mod.rs`
- Create: `crates/flotilla-core/src/providers/discovery/factories/session.rs`
- Possibly modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

- [ ] **Step 1: Write the failing discovery tests**
  - Add a detector table entry for the new binary.
  - Add a factory test showing the session backend is preferred when the binary is present.
  - Add a fallback test showing passthrough/shpool behavior is unchanged when it is absent.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p flotilla-core --locked session_factory`
Expected: FAIL because no detector or session factory exists.

- [ ] **Step 3: Implement discovery and priority**
  - Detect the `bollard` binary in host detection.
  - Add a new terminal-pool factory and register it before shpool in terminal-pool priority order.
  - Keep shpool and passthrough as valid fallback options.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p flotilla-core --locked session_factory`
Expected: PASS

## Chunk 6: TerminalPool adapter and opaque handle persistence

### Task 7: Add the new `TerminalPool` adapter in `flotilla-core`

**Files:**
- Modify: `crates/flotilla-core/Cargo.toml`
- Modify: `crates/flotilla-core/src/providers/terminal/mod.rs`
- Create: `crates/flotilla-core/src/providers/terminal/session.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Possibly modify: `crates/flotilla-core/src/attachable/mod.rs`
- Possibly modify: `crates/flotilla-core/src/attachable/store.rs`
- Test: `crates/flotilla-core/src/providers/terminal/session.rs`

- [ ] **Step 1: Write the failing provider tests**
  - Add tests for:
    - `attach_command()` returning a `bollard attach ...` command
    - `list_terminals()` mapping CLI output into `ManagedTerminal`
    - `kill_terminal()` calling the CLI kill path
    - persisted attachable bindings using an opaque daemon session handle rather than assuming shpool-style semantic names

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p flotilla-core --locked session_terminal_pool`
Expected: FAIL because the provider does not exist.

- [ ] **Step 3: Implement the adapter**
  - Add a `SessionTerminalPool` implementation parallel to `ShpoolTerminalPool`.
  - Keep the executor contract unchanged: `ensure_running`, `attach_command`, `list_terminals`, `kill_terminal`.
  - Use the attachable store to persist whichever stable handle the session daemon returns.
  - Avoid baking flotilla-specific naming into the daemon contract.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p flotilla-core --locked session_terminal_pool`
Expected: PASS

## Chunk 7: Integration verification

### Task 8: Prove phase-1 behavior through crate-local and repo-level verification

**Files:**
- Test: `crates/bollard/tests/lifecycle.rs`
- Test: `crates/flotilla-core/src/providers/terminal/session.rs`
- Possibly modify: `docs/superpowers/specs/2026-03-18-session-daemon-design.md`

- [ ] **Step 1: Run focused session-daemon verification**

Run: `cargo test -p bollard --locked`
Expected: PASS

- [ ] **Step 2: Run focused flotilla-core verification**

Run: `cargo test -p flotilla-core --locked session_`
Expected: PASS

- [ ] **Step 3: Run sandbox-safe repo verification**

Run: `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`
Expected: PASS, or explicit surfacing of unrelated pre-existing failures

- [ ] **Step 4: Commit the phase-1 implementation checkpoint**

```bash
git add Cargo.toml crates/bollard crates/flotilla-core docs/superpowers/specs/2026-03-18-session-daemon-design.md docs/superpowers/plans/2026-03-18-session-daemon-phase1.md
git commit -m "Add phase 1 session daemon terminal backend"
```

## Deferred work after this plan

- feature-gated replay-capable VT engine implementation
- `esctest` integration at the `VtEngine` seam
- `vttest` compatibility sweeps
- observer/control channels
- agent-driven TUI testing and automation surfaces
