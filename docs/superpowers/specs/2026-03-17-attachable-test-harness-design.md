# Attachable Test Harness Design

## Summary

Flotilla's current attachable and multi-host tests prove several isolated pieces:

- executor tests prove remote `attachable_set_id` values can be persisted into local workspace bindings
- refresh tests prove projected attachable data can enrich provider data
- in-process daemon tests prove peer overlays and snapshot rebuilding work in general

What is missing is a reusable seam that can express the full logical scenario behind recent regressions:

- remote checkout exists on peer host
- local workspace binds to the peer's attachable set
- snapshot rebuild merges local bindings with peer-owned checkout data
- correlation should join them into one work item

Today that scenario is awkward to express because:

- `AttachableStore` is file-backed and concrete
- fake discovery helpers do not cover enough provider categories for richer in-process scenarios
- behavior tests end up reasoning about filesystem setup and incidental persistence details while trying to validate correlation logic

This design introduces a broader test seam for attachable and multi-host behavior:

1. an `AttachableStore` abstraction with both in-memory and file-backed implementations
2. shared contract tests that define the attachable-store behavior once and run it against both implementations
3. expanded fake-provider/discovery support so `InProcessDaemon` tests can model remote checkout/workspace/terminal scenarios without real subprocess or transport setup

The immediate target is to make the live remote-workspace correlation bug reproducible as a deterministic in-process test that asserts both raw provider data and final work-item behavior.

## Problem

The live investigation showed the following sequence:

1. direct prepare on `kiwi -> feta` returns a non-null `attachable_set_id`
2. `feta` persists that exact set id in its attachable registry
3. local workspace creation on `kiwi` persists a workspace-manager binding for that same set id
4. the rebuilt snapshot still shows:
   - a remote checkout item on `feta` with `attachable_set_id = null`
   - a separate local `AttachableSet` item on `kiwi` for the same remote checkout path

This is a logical correlation/materialization failure, not a transport or persistence failure.

The likely culprit is that local snapshot normalization rewrites attachable-set host/path ownership too aggressively before peer merge and correlation. That is exactly the kind of bug that should be cheap to express in an in-process scenario test, but the current test infrastructure makes that harder than it should be.

## Goals

- Make attachable behavior tests independent of filesystem mechanics by default
- Define attachable-store behavior once and verify all implementations against the same contract
- Expand in-process daemon test infrastructure so multi-step remote/local correlation scenarios are easy to set up
- Add a regression test that covers the full logical path for remote prepared-terminal workspace correlation
- Assert on both raw snapshot/provider data and final correlated work items for the regression

## Non-goals

- Do not remove real file-backed attachable-store tests
- Do not replace all provider tests with fakes
- Do not require real SSH, daemon sockets, or subprocess orchestration for logic-only scenarios
- Do not redesign the production multi-host architecture in this slice

## Design Principles

### Behavior specification first

Behavior should be defined independently of persistence or transport choice.

Examples:

- "binding a workspace ref to a remote attachable set preserves the remote host/path"
- "a peer checkout and a local workspace bound to the same attachable set correlate into one work item"

Those are semantic rules. They should not depend on whether the backing store is in-memory or file-backed.

### Real implementations still matter

This design does not argue against testing the real persistence layer. It argues for:

- behavior/contract tests that run against both implementations
- scenario tests that use the in-memory implementation by default
- narrower persistence tests that focus on reload and on-disk compatibility

### In-process orchestration is the default seam

When a bug is about local refresh, peer overlays, correlation, or snapshot rebuilding, `InProcessDaemon` is the correct test surface unless the bug explicitly depends on process boundaries or transport behavior.

## Proposed Architecture

### 1. Introduce an attachable-store abstraction

Add a behavior-facing store interface that encapsulates:

- reading the current registry view
- allocating ids
- ensuring terminal sets and members
- inserting/replacing bindings
- looking up bindings
- persisting or snapshotting as needed by the implementation

The production code should depend on this interface rather than on a concrete file-backed type.

The current file-backed implementation becomes one implementation of that interface rather than the interface itself.

### 2. Add an in-memory implementation

Add an in-memory attachable store with the same observable behavior as the file-backed implementation but without filesystem I/O.

It should support the full logical API used by:

- executor
- refresh projection
- terminal providers
- workspace binding persistence

This becomes the default choice for behavior-heavy tests and scenario harnesses.

### 3. Define shared store contract tests

Create a shared contract test suite that runs against both:

- in-memory attachable store
- file-backed attachable store

The contract should cover at least:

- id allocation shape and opacity
- set creation and reuse semantics
- binding replacement semantics
- lookup behavior
- preservation of remote `HostPath` ownership
- persistence/reload parity where applicable

This separates behavioral correctness from storage medium.

### 4. Expand fake discovery/provider injection

The current fake discovery helper is too narrow for multi-step orchestration scenarios because it only injects a subset of provider types.

Replace or extend it with a builder that can inject:

- `CheckoutManager`
- `WorkspaceManager`
- `TerminalPool`
- `ChangeRequestTracker`
- `IssueTracker`
- later, other providers as needed

This lets in-process daemon tests assemble realistic logical scenarios without real subprocesses.

### 5. Add an in-process scenario harness

Add reusable helpers for in-process daemon tests that can:

- create a daemon with fake providers
- seed peer overlay `ProviderData`
- seed attachable-store state directly
- trigger refresh/snapshot rebuild
- fetch snapshots/work items for assertions

The key point is that test setup should read like a scenario specification, not like incidental storage plumbing.

## First Regression Test

The first failing regression test should model the live bug directly.

### Scenario

- local host: `kiwi`
- local repo root: tracked repo on `kiwi`
- peer overlay includes checkout:
  - host: `feta`
  - path: `/home/robert/dev/flotilla.terminal-stuff`
  - branch: `attachable-correlation`
- local attachable store contains:
  - set id `set-remote`
  - set checkout still owned by `feta:/home/robert/dev/flotilla.terminal-stuff`
  - workspace-manager binding `workspace:9 -> set-remote`
- local workspace provider exposes `workspace:9`

### Assertions on provider data

- the merged snapshot still contains `attachable_sets["set-remote"].checkout.host == "feta"`
- the merged snapshot still contains `attachable_sets["set-remote"].checkout.path == "/home/robert/dev/flotilla.terminal-stuff"`
- the remote checkout entry remains owned by `feta`, not rewritten to local host

### Assertions on work items

- the remote checkout work item carries `attachable_set_id == "set-remote"`
- the remote checkout work item includes the bound local workspace ref
- there is not a second separate local `AttachableSet` work item representing the same logical checkout path

This test should fail against the current logic if local normalization rewrites the set ownership before peer merge/correlation.

## File and Responsibility Map

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/flotilla-core/src/attachable/store.rs` | Refactor | Split behavior from file-backed persistence |
| `crates/flotilla-core/src/attachable/mod.rs` | Modify | Export abstraction and implementations |
| `crates/flotilla-core/src/attachable/in_memory.rs` | Add | In-memory attachable-store implementation |
| `crates/flotilla-core/src/attachable/file_backed.rs` | Add or refactor | File-backed attachable-store implementation |
| `crates/flotilla-core/src/refresh.rs` | Modify | Depend on abstraction, not concrete file-backed store |
| `crates/flotilla-core/src/executor.rs` | Modify | Depend on abstraction, not concrete file-backed store |
| `crates/flotilla-core/src/providers/terminal/shpool.rs` | Modify | Depend on abstraction, not concrete file-backed store |
| `crates/flotilla-core/src/providers/discovery/test_support.rs` | Modify | Expand fake provider injection/builder support |
| `crates/flotilla-core/tests/in_process_daemon.rs` | Modify | Add end-to-end logical regression scenario |
| `crates/flotilla-core/src/attachable/*tests*` | Add/modify | Shared contract tests for store implementations |

## Testing Strategy

### Contract tests

Run shared attachable-store behavior tests against both implementations.

### Scenario tests

Use `InProcessDaemon` plus fake providers and the in-memory store for multi-step logical scenarios.

### Real-backed verification

Keep a smaller file-backed suite that proves:

- on-disk roundtrip
- reload parity
- contract conformance

## Trade-offs

### Why not keep using tempdirs everywhere?

Because the cost is paid on every new scenario:

- more incidental setup
- harder failure diagnosis
- less reuse
- less clarity about whether a test is validating logic or persistence

### Why not only test at the lowest level?

Because this regression is not just a store bug. It is about:

- projection
- normalization
- merge
- correlation

Only an in-process scenario test sees the full failure shape.

## Recommendation

Implement the broader harness now:

1. add the attachable-store abstraction and in-memory implementation
2. add shared store contract tests
3. expand fake discovery/provider injection
4. add the in-process regression scenario that asserts on both provider data and work items

This is the right amount of infrastructure for the problem space and should make future multi-host logical regressions substantially cheaper to catch.
