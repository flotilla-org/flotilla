# Attachable Test Harness Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a reusable attachable test seam with shared store contracts, richer fake provider injection, and an in-process daemon regression test for remote workspace correlation.

**Architecture:** Split attachable-store behavior from file-backed persistence, introduce an in-memory implementation, and push orchestration tests up to the `InProcessDaemon` layer. Behavior-heavy tests should use injected collaborators and shared contracts across implementations, while real-backed persistence remains covered by narrower contract verification.

**Tech Stack:** Rust, `tokio`, existing `InProcessDaemon`, existing fake discovery runtime, shared attachable registry semantics, tempdir only for file-backed contract coverage.

**Spec:** `docs/superpowers/specs/2026-03-17-attachable-test-harness-design.md`

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/flotilla-core/src/attachable/mod.rs` | Modify | Export trait + implementations |
| `crates/flotilla-core/src/attachable/store.rs` | Refactor | Existing behavior moved behind abstraction or file-backed impl |
| `crates/flotilla-core/src/attachable/in_memory.rs` | Add | In-memory attachable store |
| `crates/flotilla-core/src/attachable/file_backed.rs` | Add or move | File-backed attachable store |
| `crates/flotilla-core/src/refresh.rs` | Modify | Use abstraction |
| `crates/flotilla-core/src/executor.rs` | Modify | Use abstraction |
| `crates/flotilla-core/src/providers/terminal/shpool.rs` | Modify | Use abstraction |
| `crates/flotilla-core/src/providers/discovery/test_support.rs` | Modify | Richer fake-provider builder |
| `crates/flotilla-core/tests/in_process_daemon.rs` | Modify | Regression scenario + helper use |
| `AGENTS.md` | Already modified | Testing philosophy note |
| `CLAUDE.md` | Already modified | Testing philosophy note |

## Chunk 1: Introduce the Store Abstraction

### Task 1: Define the behavior-facing attachable store interface

**Files:**
- Modify: `crates/flotilla-core/src/attachable/mod.rs`
- Modify: `crates/flotilla-core/src/attachable/store.rs`
- Add: `crates/flotilla-core/src/attachable/in_memory.rs`
- Add or move: `crates/flotilla-core/src/attachable/file_backed.rs`

- [ ] **Step 1: Write the failing compilation/tests for the new shape**

Add or update targeted tests that require:
- constructing an in-memory store
- constructing a file-backed store
- calling the shared behavior API from both

- [ ] **Step 2: Run targeted tests to verify failure**

Run:

```bash
cargo test -p flotilla-core --locked attachable
```

Expected: FAIL because the new abstraction and implementation split do not exist yet.

- [ ] **Step 3: Introduce the trait/object-safe interface**

Define the attachable-store behavior interface used by higher layers:
- registry view access
- id allocation
- ensure set/member helpers
- binding replacement
- binding lookup

Keep the interface focused on observable behavior, not storage details.

- [ ] **Step 4: Implement the in-memory store**

Add an in-memory implementation with the same semantics as the current store but no filesystem I/O.

- [ ] **Step 5: Move/refactor the existing implementation into file-backed form**

Preserve existing persisted behavior while routing higher layers through the abstraction.

- [ ] **Step 6: Update exported types and aliases**

Make the core code depend on the new abstraction instead of the concrete file-backed type.

- [ ] **Step 7: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core --locked attachable
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-core/src/attachable
git commit -m "refactor: abstract attachable store implementations"
```

## Chunk 2: Add Shared Store Contract Tests

### Task 2: Specify attachable-store behavior once and run it against both implementations

**Files:**
- Modify: `crates/flotilla-core/src/attachable/in_memory.rs`
- Modify: `crates/flotilla-core/src/attachable/file_backed.rs`
- Add or modify shared test helpers under `crates/flotilla-core/src/attachable/`

- [ ] **Step 1: Write shared contract test helpers**

Cover:
- opaque id behavior
- set reuse
- binding replacement
- binding lookup
- remote `HostPath` preservation
- parity between in-memory and file-backed semantics

- [ ] **Step 2: Hook the shared contract into both implementations**

Run the same contract cases against:
- in-memory store
- file-backed store

- [ ] **Step 3: Add file-backed reload-specific checks**

Keep persistence-specific checks small and separate from generic behavior assertions.

- [ ] **Step 4: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core --locked attachable
```

Expected: PASS for both implementations.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/attachable
git commit -m "test: add shared attachable store contracts"
```

## Chunk 3: Update Production Callers to Use the Abstraction

### Task 3: Switch refresh, executor, and terminal integration to the new store seam

**Files:**
- Modify: `crates/flotilla-core/src/refresh.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`

- [ ] **Step 1: Write or update failing integration-style unit tests where signatures change**

Cover:
- refresh projection still resolves workspace and terminal bindings
- executor persistence tests still prove remote set bindings preserve remote host/path
- shpool tests still register attachables correctly

- [ ] **Step 2: Run targeted tests to verify breakage**

Run:

```bash
cargo test -p flotilla-core --locked refresh executor shpool
```

Expected: FAIL until callers are updated to the new abstraction.

- [ ] **Step 3: Update caller plumbing**

Replace direct dependence on the concrete store with the abstract interface/shared handle.

- [ ] **Step 4: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core --locked refresh executor shpool
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/refresh.rs crates/flotilla-core/src/executor.rs crates/flotilla-core/src/providers/terminal/shpool.rs crates/flotilla-core/src/providers/discovery/mod.rs
git commit -m "refactor: route core attachable logic through store interface"
```

## Chunk 4: Expand Fake Discovery and Scenario Seeding

### Task 4: Add richer fake-provider injection for in-process daemon tests

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Write failing tests for the desired fake-discovery ergonomics**

Add tests or helper call sites that require fake discovery injection for:
- `CheckoutManager`
- `WorkspaceManager`
- `TerminalPool`

- [ ] **Step 2: Run targeted tests to verify failure**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon fake_discovery
```

Expected: FAIL because the current fake builder is too narrow.

- [ ] **Step 3: Implement a richer fake-discovery builder**

Prefer a builder or config struct over a growing positional helper signature.

- [ ] **Step 4: Add scenario helper functions in the in-process daemon test module**

Helpers should make it easy to:
- seed peer overlay provider data
- seed attachable-store state
- trigger refresh
- fetch snapshots/work items

- [ ] **Step 5: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon
```

Expected: PASS for existing tests and new harness-level tests.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/test_support.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "test: expand in-process daemon scenario harness"
```

## Chunk 5: Add the Remote Workspace Correlation Regression Test

### Task 5: Reproduce the live remote attachable correlation bug in-process

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Write the failing regression test first**

Scenario:
- local daemon host is `kiwi`
- peer overlay includes checkout `feta:/home/robert/dev/flotilla.terminal-stuff`
- local workspace provider exposes `workspace:9`
- local attachable-store state binds `workspace:9` to `set-remote`
- `set-remote` checkout remains `feta:/home/robert/dev/flotilla.terminal-stuff`

Assertions:
- merged provider data still records `set-remote` checkout on `feta`
- remote checkout gets the expected `attachable_set_id`
- remote checkout gets the expected workspace ref
- no extra kiwi-side attachable-set work item is emitted for the same logical checkout

- [ ] **Step 2: Run the targeted regression test and verify failure**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon remote_workspace_attachable
```

Expected: FAIL with the current host/path rewriting behavior.

- [ ] **Step 3: Fix the production logic with the narrowest correct change**

Likely touchpoints:
- `crates/flotilla-core/src/in_process.rs`
- possibly merge/correlation code if the failure is broader than expected

Keep the fix focused on preserving remote attachable-set ownership through normalization and merge.

- [ ] **Step 4: Re-run the targeted regression**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon remote_workspace_attachable
```

Expected: PASS.

- [ ] **Step 5: Run broader affected coverage**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon
cargo test -p flotilla-core --locked executor
cargo test -p flotilla-core --locked refresh
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "fix: preserve remote attachable ownership in correlation"
```

## Chunk 6: Final Verification

### Task 6: Run repo-level verification for the attachable test seam changes

**Files:**
- No code changes

- [ ] **Step 1: Run focused package verification**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon
cargo test -p flotilla-core --locked
```

Expected: PASS.

- [ ] **Step 2: Run formatting and linting for touched crates**

Run:

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy -p flotilla-core --all-targets --locked -- -D warnings
```

Expected: PASS.

- [ ] **Step 3: Commit any final cleanup**

```bash
git add -A
git commit -m "test: add attachable scenario and contract coverage"
```

