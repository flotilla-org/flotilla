# Test Code Consolidation

## Problem

The test suite has grown to ~36,000 lines (49% of the codebase). Duplicate helper functions, identical mock implementations, and large inline test modules have accumulated across crate boundaries. This duplication slows navigation, invites drift between copies, and makes adding new tests harder than it should be.

## Goals

1. Eliminate duplicate test helpers and mock implementations
2. Extract large inline test modules to separate files
3. Parameterize repetitive accessor tests in `data.rs`
4. Establish shared infrastructure that prevents re-duplication

## Non-Goals

- Changing test coverage or removing tests
- Merging complementary mock implementations that serve distinct testing layers
- Refactoring test logic beyond mechanical consolidation

---

## Phase 1: Shared Builders in `flotilla-protocol`

### Motivation

The most prolific duplicates are builders for protocol types. `hp()` appears in 10 files. `make_checkout()` appears in 6. These construct types owned by `flotilla-protocol`, so that crate is the natural home.

### Changes

Add a `test-support` feature to `flotilla-protocol/Cargo.toml` and create `src/test_support.rs`, gated by `#[cfg(any(test, feature = "test-support"))]`.

#### `hp()` — HostPath factory

```rust
pub fn hp(path: &str) -> HostPath {
    HostPath::new(HostName::new("test-host"), PathBuf::from(path))
}
```

Replaces 10 copies across `flotilla-protocol` (lib.rs, delta.rs, snapshot.rs, provider_data.rs), `flotilla-core` (data.rs, delta.rs, convert.rs, correlation.rs, executor/tests.rs), and `flotilla-tui` (key_handlers.rs). Eight of the ten use `HostName::new("test-host")`; two use `HostName::local()`. The shared version uses the deterministic `"test-host"` to keep tests reproducible. The two `HostName::local()` call sites (`executor/tests.rs` and `key_handlers.rs`) should be checked — if they need the real hostname, they keep a local wrapper.

#### `TestCheckout` — fluent builder

```rust
pub struct TestCheckout { /* fields with defaults */ }

impl TestCheckout {
    pub fn new(branch: &str) -> Self;
    pub fn at(self, path: &str) -> Self;       // sets checkout path + CheckoutPath correlation key
    pub fn is_main(self, val: bool) -> Self;
    pub fn with_branch_key(self) -> Self;       // adds CorrelationKey::Branch
    pub fn build(self) -> Checkout;
}
```

Default behavior: no correlation keys. `at()` adds `CorrelationKey::CheckoutPath`; `with_branch_key()` adds `CorrelationKey::Branch`. Callers opt in to the keys they need.

Replaces 6 `make_checkout()` variants:

| Current location | Current signature | Builder equivalent |
|---|---|---|
| `core/data.rs` | `(branch, path, is_main)` | `TestCheckout::new(b).at(p).is_main(m).with_branch_key().build()` |
| `tui/tests/support` | `(branch, path, is_main) -> (HostPath, Checkout)` | Thin wrapper calling builder |
| `core/refresh.rs` | `(branch)` | `TestCheckout::new(b).with_branch_key().build()` |
| `core/executor/tests.rs` | `(branch, _path)` | `TestCheckout::new(b).build()` |
| `daemon/peer/merge.rs` | `(branch)` | `TestCheckout::new(b).build()` |
| `daemon/tests/multi_host.rs` | `(branch)` | `TestCheckout::new(b).build()` |

#### `TestChangeRequest` — fluent builder

```rust
pub struct TestChangeRequest { /* fields with defaults */ }

impl TestChangeRequest {
    pub fn new(title: &str, branch: &str) -> Self;
    pub fn with_branch_key(self) -> Self;
    pub fn build(self) -> ChangeRequest;
}
```

Replaces 3 copies across `core/data.rs`, `core/refresh.rs`, and `tui/tests/support`.

#### `TestSession` — fluent builder

```rust
pub struct TestSession { /* fields with defaults */ }

impl TestSession {
    pub fn new(title: &str) -> Self;
    pub fn with_status(self, status: SessionStatus) -> Self;
    pub fn with_session_ref(self, provider: &str, id: &str) -> Self;
    pub fn with_branch_key(self, branch: &str) -> Self;
    pub fn build(self) -> CloudAgentSession;
}
```

Replaces 3 copies across `core/data.rs`, `core/refresh.rs`, and `tui/tests/support`.

#### `TestIssue` — fluent builder

```rust
pub struct TestIssue { /* fields with defaults */ }

impl TestIssue {
    pub fn new(title: &str) -> Self;
    pub fn with_labels(self, labels: Vec<String>) -> Self;
    pub fn build(self) -> Issue;
}
```

Replaces 4 copies across `core/data.rs`, `core/executor/tests.rs`, `core/tests/in_process_daemon.rs`, and `tui/tests/support`.

### Dependency updates

Each consuming crate adds `flotilla-protocol` with the `test-support` feature to `[dev-dependencies]`. Crates that already list `flotilla-protocol` in `[dependencies]` (without the feature) add a second entry in `[dev-dependencies]` to enable the feature for tests only — this is the standard Cargo idiom:

```toml
[dev-dependencies]
flotilla-protocol = { path = "../flotilla-protocol", features = ["test-support"] }
```

For `flotilla-core` and `flotilla-daemon` (which already have their own `test-support` features that downstream crates enable), their `test-support` feature should forward to protocol:

```toml
[features]
test-support = ["flotilla-protocol/test-support"]
```

---

## Phase 2: Consolidate Identical Mocks

### Peer networking mocks → `flotilla-daemon/src/peer/test_support.rs`

Three identical `MockPeerSender` implementations exist in `server/tests.rs` (as `CapturePeerSender`), `peer/manager.rs`, and `tests/multi_host.rs`. All append sent messages to an `Arc<Mutex<Vec<PeerWireMessage>>>`.

Two identical `MockTransport` implementations exist in `peer/manager.rs` and `tests/multi_host.rs`.

`BlockingPeerSender` in `server/tests.rs` extends the capture pattern with `Notify`-based synchronization.

Move all three to `peer/test_support.rs` (which already exists with `TestPeer` and `TestNetwork`), gated by `#[cfg(any(test, feature = "test-support"))]`. Delete the 5 duplicate copies. Use the name `MockPeerSender` (2:1 majority). The consolidated `MockTransport` should include both `new()` (from manager.rs) and `with_sender()` constructors.

### `wait_for_command_result()` → daemon test support

Two identical copies exist in `tests/multi_host.rs` and `src/server/tests.rs`, differing only in timeout (10s vs 5s). Extract to a shared location with a timeout parameter:

```rust
pub async fn wait_for_command_result(
    rx: &mut broadcast::Receiver<DaemonEvent>,
    command_id: u64,
    timeout: Duration,
) -> CommandValue;
```

### `CommandRunner` FIFO mocks → `flotilla-core` test-support

`providers/mod.rs` and `terminal/cleat.rs` both implement FIFO-queue command runners. Cleat's version adds call tracking. Merge into one `MockRunner` in core's test-support with optional call tracking:

```rust
pub struct MockRunner {
    responses: Mutex<VecDeque<CommandOutput>>,
    calls: Mutex<Vec<(String, Vec<String>)>>,  // always recorded
}
```

The `DiscoveryMockRunner` in `discovery/test_support.rs` is genuinely different (keyed responses, tool existence checks) and stays where it is.

### Provider trait mocks — keep separate

The `MockCheckoutManager` / `MockCloudAgent` / etc. implementations in `executor/tests.rs`, `refresh.rs`, and `discovery/test_support.rs` serve three distinct testing layers:

- **executor**: minimal single-shot stubs for unit tests
- **refresh**: pre-configured static data for integration tests
- **discovery**: stateful simulations for E2E tests

Merging these would make simple tests complex. They stay where they are.

---

## Phase 3: Extract Large Inline Test Modules

Four inline test modules exceed 1,000 lines. Extract each to a sibling directory, keeping the source file at its current path.

| Source file (unchanged) | New test file | Lines moved |
|---|---|---|
| `flotilla-core/src/data.rs` | `flotilla-core/src/data/tests.rs` | ~1,523 |
| `flotilla-tui/src/app/key_handlers.rs` | `flotilla-tui/src/app/key_handlers/tests.rs` | ~1,614 |
| `flotilla-tui/src/app/intent.rs` | `flotilla-tui/src/app/intent/tests.rs` | ~1,085 |
| `flotilla-daemon/src/peer/manager.rs` | `flotilla-daemon/src/peer/manager/tests.rs` | ~1,216 |

Each source file replaces its inline `#[cfg(test)] mod tests { ... }` with:

```rust
#[cfg(test)]
#[path = "data/tests.rs"]
mod tests;
```

The source file itself does not move or become a `mod.rs`. The directory must be created (e.g. `mkdir crates/flotilla-core/src/data/`) and contains only the test file. The extracted test file begins with `use super::*` (or explicit imports) since the module's parent is the declaring module.

Phase 3 depends on Phase 2 for `peer/manager.rs`: the `MockPeerSender` and `MockTransport` that currently live in its test module are deleted (they moved to `peer/test_support.rs` in Phase 2), reducing the extracted file further.

### Modules kept inline

- `app/mod.rs` (922 lines) — tightly coupled to `App` internals
- `refresh.rs` (727 lines) — custom mocks specific to refresh logic
- `server/tests.rs` — already a separate file

---

## Phase 4: Parameterize `data.rs` Accessor Tests

The `data.rs` test module contains ~44 accessor tests that follow identical patterns. Five tests for `kind()`, five for `branch()`, three for `description()`, and so on — each creates an item via a factory and asserts on a single accessor.

Collapse each group into a single parameterized test using a cases array:

```rust
#[test]
fn kind_returns_correct_variant() {
    let cases: Vec<(&str, CorrelationResult, WorkItemKind)> = vec![
        ("checkout", checkout_item("feat", "/tmp/a", false), WorkItemKind::Checkout),
        ("change_request", cr_item("Fix", "feat"), WorkItemKind::ChangeRequest),
        ("session", session_item("s1", "Work"), WorkItemKind::Session),
        ("issue", issue_item("1", "Bug"), WorkItemKind::Issue),
        ("remote_branch", remote_branch_item("main"), WorkItemKind::RemoteBranch),
    ];
    for (label, item, expected) in cases {
        assert_eq!(item.kind(), expected, "failed for {label}");
    }
}
```

This pattern applies to accessor groups: `kind`, `branch`, `description`, `checkout`, `change_request_key`, `session_key`, `issue_keys`, `workspace_refs`, `correlation_group_idx`, `as_correlated_mut`, and `identity`. Some tests construct items with custom setup (e.g. `branch_from_change_request_correlated` builds a `CorrelatedWorkItem` directly rather than using a factory), so the exact reduction should be validated during implementation. Estimated reduction: ~34-44 tests → ~10, eliminating ~200-300 lines of boilerplate.

The correlation tests (25 tests) stay as individual tests — each exercises a distinct scenario with different assertions.

---

## Expected Impact

| Metric | Before | After (est.) |
|---|---|---|
| `hp()` copies | 10 | 1 |
| `make_checkout()` copies | 6 | 1 builder |
| `make_issue/cr/session()` copies | 10 total | 3 builders |
| `MockPeerSender` copies | 3 | 1 |
| `MockTransport` copies | 2 | 1 |
| `CommandRunner` mock copies | 2 | 1 |
| Largest inline test module | 1,614 lines | 0 (all extracted) |
| `data.rs` accessor test count | ~44 | ~10 |
| Estimated lines removed | — | ~500-700 |

The line reduction is modest because the primary goal is structural: fewer copies to maintain, clearer test file navigation, and shared infrastructure that prevents re-duplication as the test suite grows.
