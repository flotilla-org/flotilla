# Test Code Consolidation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Eliminate duplicate test helpers, consolidate identical mocks, extract large inline test modules, and parameterize repetitive tests.

**Architecture:** Shared test data builders go in `flotilla-protocol` (all crates depend on it). Shared daemon mocks go in `flotilla-daemon/src/peer/test_support.rs`. Large inline test modules move to sibling `<module>/tests.rs` files.

**Tech Stack:** Rust, Cargo features (`test-support`), `#[path]` attribute for test module extraction.

**Spec:** `docs/superpowers/specs/2026-03-22-test-code-consolidation-design.md`

---

## File Map

### Created

| File | Responsibility |
|------|----------------|
| `crates/flotilla-protocol/src/test_support.rs` | Shared test builders: `hp()`, `TestCheckout`, `TestChangeRequest`, `TestSession`, `TestIssue` |
| `crates/flotilla-core/src/data/tests.rs` | Extracted + parameterized data.rs test module |
| `crates/flotilla-tui/src/app/key_handlers/tests.rs` | Extracted key_handlers.rs test module |
| `crates/flotilla-tui/src/app/intent/tests.rs` | Extracted intent.rs test module |
| `crates/flotilla-daemon/src/peer/manager/tests.rs` | Extracted manager.rs test module |

### Modified

| File | Changes |
|------|---------|
| `crates/flotilla-protocol/Cargo.toml` | Add `[features]` with `test-support` |
| `crates/flotilla-protocol/src/lib.rs` | Add `pub mod test_support` (feature-gated) |
| `crates/flotilla-core/Cargo.toml` | Forward `test-support` to protocol |
| `crates/flotilla-daemon/Cargo.toml` | Forward `test-support` to protocol; add protocol to dev-deps with feature |
| `crates/flotilla-tui/Cargo.toml` | Add protocol to dev-deps with `test-support` feature |
| `crates/flotilla-daemon/src/peer/test_support.rs` | Add `MockPeerSender`, `MockTransport`, `BlockingPeerSender`, `wait_for_command_result` |
| `crates/flotilla-daemon/src/peer/mod.rs` | Gate `test_support` with `cfg(any(test, feature = "test-support"))` |
| `crates/flotilla-core/src/providers/mod.rs` | Add call tracking to `MockRunner` |
| 10 files with `hp()` | Replace local `hp()` with import from `flotilla_protocol::test_support::hp` |
| 6 files with `make_checkout()` | Replace with `TestCheckout` builder |
| 3 files with `make_change_request()` | Replace with `TestChangeRequest` builder |
| 3 files with `make_session()` | Replace with `TestSession` builder |
| 4 files with `make_issue()` | Replace with `TestIssue` builder |
| `crates/flotilla-core/src/data.rs` | Replace inline test module with `#[path]` declaration |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Replace inline test module with `#[path]` declaration |
| `crates/flotilla-tui/src/app/intent.rs` | Replace inline test module with `#[path]` declaration |
| `crates/flotilla-daemon/src/peer/manager.rs` | Replace inline test module with `#[path]` declaration |
| `crates/flotilla-core/src/providers/terminal/cleat.rs` | Replace local `MockRunner` with import from `crate::providers::testing::MockRunner` |
| `crates/flotilla-daemon/src/server/tests.rs` | Replace local `CapturePeerSender`, `BlockingPeerSender`, `wait_for_command_result` with imports |
| `crates/flotilla-daemon/tests/multi_host.rs` | Replace local `MockPeerSender`, `MockTransport`, `make_checkout`, `wait_for_command_result` with imports |
| `crates/flotilla-daemon/src/peer/manager.rs` (test module) | Remove `MockPeerSender`, `MockTransport` definitions |

---

## Task 1: Create `flotilla-protocol` test_support module with `hp()` and builders

**Files:**
- Create: `crates/flotilla-protocol/src/test_support.rs`
- Modify: `crates/flotilla-protocol/Cargo.toml`
- Modify: `crates/flotilla-protocol/src/lib.rs`

- [x] **Step 1: Add `test-support` feature to protocol Cargo.toml**

In `crates/flotilla-protocol/Cargo.toml`, add a `[features]` section after `[package]`:

```toml
[features]
default = []
test-support = []
```

- [x] **Step 2: Add feature-gated module declaration to lib.rs**

In `crates/flotilla-protocol/src/lib.rs`, after line 10 (`pub mod snapshot;`), add:

```rust
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
```

- [x] **Step 3: Create `test_support.rs` with `hp()` and all builders**

Create `crates/flotilla-protocol/src/test_support.rs`:

```rust
//! Shared test builders for protocol types.
//!
//! Available when `cfg(test)` or the `test-support` feature is enabled.
//! All builders produce minimal structs with empty/default fields — callers
//! opt in to correlation keys and other detail via fluent methods.

use std::path::PathBuf;

use crate::{
    provider_data::{
        ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, CorrelationKey, Issue,
        SessionStatus,
    },
    HostName, HostPath,
};

/// Build a `HostPath` with a deterministic `"test-host"` hostname.
pub fn hp(path: &str) -> HostPath {
    HostPath::new(HostName::new("test-host"), PathBuf::from(path))
}

// ---------------------------------------------------------------------------
// TestCheckout
// ---------------------------------------------------------------------------

pub struct TestCheckout {
    branch: String,
    is_main: bool,
    correlation_keys: Vec<CorrelationKey>,
}

impl TestCheckout {
    pub fn new(branch: &str) -> Self {
        Self {
            branch: branch.to_string(),
            is_main: false,
            correlation_keys: Vec::new(),
        }
    }

    /// Set the checkout path. Adds a `CorrelationKey::CheckoutPath`.
    pub fn at(mut self, path: &str) -> Self {
        self.correlation_keys.push(CorrelationKey::CheckoutPath(hp(path)));
        self
    }

    pub fn is_main(mut self, val: bool) -> Self {
        self.is_main = val;
        self
    }

    /// Add a `CorrelationKey::Branch` for this checkout's branch name.
    pub fn with_branch_key(mut self) -> Self {
        self.correlation_keys
            .push(CorrelationKey::Branch(self.branch.clone()));
        self
    }

    pub fn build(self) -> Checkout {
        Checkout {
            branch: self.branch,
            is_main: self.is_main,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: self.correlation_keys,
            association_keys: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// TestChangeRequest
// ---------------------------------------------------------------------------

pub struct TestChangeRequest {
    title: String,
    branch: String,
    correlation_keys: Vec<CorrelationKey>,
}

impl TestChangeRequest {
    pub fn new(title: &str, branch: &str) -> Self {
        Self {
            title: title.to_string(),
            branch: branch.to_string(),
            correlation_keys: Vec::new(),
        }
    }

    /// Add a `CorrelationKey::Branch` for this CR's branch name.
    pub fn with_branch_key(mut self) -> Self {
        self.correlation_keys
            .push(CorrelationKey::Branch(self.branch.clone()));
        self
    }

    pub fn build(self) -> ChangeRequest {
        ChangeRequest {
            title: self.title,
            branch: self.branch,
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: self.correlation_keys,
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// TestSession
// ---------------------------------------------------------------------------

pub struct TestSession {
    title: String,
    status: SessionStatus,
    correlation_keys: Vec<CorrelationKey>,
}

impl TestSession {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            status: SessionStatus::Running,
            correlation_keys: Vec::new(),
        }
    }

    pub fn with_status(mut self, status: SessionStatus) -> Self {
        self.status = status;
        self
    }

    /// Add a `CorrelationKey::SessionRef`.
    pub fn with_session_ref(mut self, provider: &str, id: &str) -> Self {
        self.correlation_keys
            .push(CorrelationKey::SessionRef(provider.to_string(), id.to_string()));
        self
    }

    /// Add a `CorrelationKey::Branch`.
    pub fn with_branch_key(mut self, branch: &str) -> Self {
        self.correlation_keys
            .push(CorrelationKey::Branch(branch.to_string()));
        self
    }

    pub fn build(self) -> CloudAgentSession {
        CloudAgentSession {
            title: self.title,
            status: self.status,
            model: None,
            updated_at: None,
            correlation_keys: self.correlation_keys,
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// TestIssue
// ---------------------------------------------------------------------------

pub struct TestIssue {
    title: String,
    labels: Vec<String>,
}

impl TestIssue {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            labels: Vec::new(),
        }
    }

    pub fn with_labels(mut self, labels: Vec<String>) -> Self {
        self.labels = labels;
        self
    }

    pub fn build(self) -> Issue {
        Issue {
            title: self.title,
            labels: self.labels,
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }
}
```

- [x] **Step 4: Verify protocol crate compiles**

Run: `cargo build -p flotilla-protocol --features test-support`
Expected: compiles cleanly.

- [x] **Step 5: Run protocol tests**

Run: `cargo test -p flotilla-protocol`
Expected: all existing tests pass.

- [x] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/
git commit -m "feat: add test-support feature with shared test builders to flotilla-protocol"
```

---

## Task 2: Update downstream Cargo.toml files to enable `test-support`

**Files:**
- Modify: `crates/flotilla-core/Cargo.toml`
- Modify: `crates/flotilla-daemon/Cargo.toml`
- Modify: `crates/flotilla-tui/Cargo.toml`

- [x] **Step 1: Forward `test-support` in flotilla-core**

In `crates/flotilla-core/Cargo.toml`, change line 10:

```toml
test-support = []
```

to:

```toml
test-support = ["flotilla-protocol/test-support"]
```

- [x] **Step 2: Forward `test-support` in flotilla-daemon**

In `crates/flotilla-daemon/Cargo.toml`, change line 10:

```toml
test-support = []
```

to:

```toml
test-support = ["flotilla-protocol/test-support"]
```

Also update the existing `flotilla-protocol` dev-dependency (line 31) to enable the feature:

```toml
flotilla-protocol = { path = "../flotilla-protocol", features = ["test-support"] }
```

- [x] **Step 3: Add protocol dev-dependency with feature to flotilla-tui**

In `crates/flotilla-tui/Cargo.toml`, add to the `[dev-dependencies]` section:

```toml
flotilla-protocol = { path = "../flotilla-protocol", features = ["test-support"] }
```

- [x] **Step 4: Verify full workspace builds**

Run: `cargo build --workspace`
Expected: compiles cleanly.

- [x] **Step 5: Commit**

```bash
git add crates/flotilla-core/Cargo.toml crates/flotilla-daemon/Cargo.toml crates/flotilla-tui/Cargo.toml
git commit -m "chore: wire test-support feature through to flotilla-protocol"
```

---

## Task 3: Replace `hp()` across all 10 call sites

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs:249` (inline test `hp()`)
- Modify: `crates/flotilla-protocol/src/delta.rs:101` (inline test `hp()`)
- Modify: `crates/flotilla-protocol/src/snapshot.rs:152` (inline test `hp()`)
- Modify: `crates/flotilla-protocol/src/provider_data.rs:346` (inline test `hp()`)
- Modify: `crates/flotilla-core/src/data.rs:894` (inline test `hp()`)
- Modify: `crates/flotilla-core/src/delta.rs:141` (inline test `hp()`)
- Modify: `crates/flotilla-core/src/convert.rs:299` (inline test `hp()`)
- Modify: `crates/flotilla-core/src/providers/correlation.rs:201` (inline test `hp()`)
- Modify: `crates/flotilla-core/src/executor/tests.rs:42` (inline test `hp()`)
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:356` (inline test `hp()`)

- [x] **Step 1: Replace hp() in protocol crate's own test modules**

In each of the 4 protocol files (`lib.rs`, `delta.rs`, `snapshot.rs`, `provider_data.rs`), find the local `fn hp()` definition inside the `#[cfg(test)] mod tests` block and delete it. Add `use crate::test_support::hp;` to the test module's imports instead.

For protocol-internal test modules, `crate::test_support` is available because the module is gated with `#[cfg(any(test, feature = "test-support"))]` and tests always have `cfg(test)`.

- [x] **Step 2: Replace hp() in flotilla-core test modules**

In each of the 5 core files (`data.rs`, `delta.rs`, `convert.rs`, `correlation.rs`, `executor/tests.rs`), find the local `fn hp()` definition and delete it. Add `use flotilla_protocol::test_support::hp;` to the test module's imports.

**`executor/tests.rs` must keep a local `hp()` using `HostName::local()`.** The executor at `executor.rs:426` constructs lookup keys with `HostName::local()` — if test `hp()` uses `"test-host"` instead, key lookups will fail with "checkout not found" errors. Keep a local wrapper:

```rust
fn hp(path: &str) -> HostPath {
    HostPath::new(HostName::local(), PathBuf::from(path))
}
```

- [x] **Step 3: Replace hp() in flotilla-tui**

In `key_handlers.rs` (line 357-359), the existing `hp()` uses `HostName::local()`. Check whether any test in this file depends on the hostname matching `HostName::local()` (e.g. via assertions that compare against an `App` that uses `HostName::local()` internally). If so, keep a local wrapper like executor/tests.rs. If not, replace with `use flotilla_protocol::test_support::hp;`.

- [x] **Step 4: Run full workspace tests**

Run: `cargo test --workspace --locked`
Expected: all tests pass. If any fail due to hostname mismatch, keep a local `hp()` wrapper in that file.

- [x] **Step 5: Commit**

```bash
git commit -am "refactor: replace 10 local hp() copies with shared flotilla_protocol::test_support::hp"
```

---

## Task 4: Replace `make_checkout()` across 6 call sites with `TestCheckout`

**Files:**
- Modify: `crates/flotilla-core/src/data.rs:963-974`
- Modify: `crates/flotilla-core/src/refresh.rs:630-641`
- Modify: `crates/flotilla-core/src/executor/tests.rs:274-285`
- Modify: `crates/flotilla-daemon/src/peer/merge.rs:11-21`
- Modify: `crates/flotilla-daemon/tests/multi_host.rs:102-112`
- Modify: `crates/flotilla-tui/tests/support/mod.rs:221-234`

- [x] **Step 1: Replace make_checkout() in core/data.rs**

Delete the `make_checkout` function (lines 963-974). Add `use flotilla_protocol::test_support::TestCheckout;` to imports. Update all call sites in the test module:
- `make_checkout(branch, path, is_main)` becomes `TestCheckout::new(branch).at(path).is_main(is_main).with_branch_key().build()`

The existing version adds both `Branch` and `CheckoutPath` correlation keys.

- [x] **Step 2: Replace make_checkout() in core/refresh.rs**

Delete the `make_checkout` function (lines 630-641). Add `use flotilla_protocol::test_support::TestCheckout;` to imports. Update call sites:
- `make_checkout(branch)` becomes `TestCheckout::new(branch).with_branch_key().build()`

The existing version adds a `Branch` correlation key.

- [x] **Step 3: Replace make_checkout() in core/executor/tests.rs**

Delete the `make_checkout` function (lines 274-285). Add `use flotilla_protocol::test_support::TestCheckout;` to imports. Update call sites:
- `make_checkout(branch, _path)` becomes `TestCheckout::new(branch).build()`

The existing version adds no correlation keys.

- [x] **Step 4: Replace make_checkout() in daemon/peer/merge.rs**

Delete the `make_checkout` function (lines 11-21). Add `use flotilla_protocol::test_support::TestCheckout;` to imports. Update call sites:
- `make_checkout(branch)` becomes `TestCheckout::new(branch).build()`

- [x] **Step 5: Replace make_checkout() in daemon/tests/multi_host.rs**

Delete the `make_checkout` function (lines 102-112). Add `use flotilla_protocol::test_support::TestCheckout;` to imports. Update call sites:
- `make_checkout(branch)` becomes `TestCheckout::new(branch).build()`

- [x] **Step 6: Update tui/tests/support/mod.rs make_checkout()**

In `crates/flotilla-tui/tests/support/mod.rs` (lines 221-234), rewrite `make_checkout` to delegate to `TestCheckout`:

```rust
pub fn make_checkout(branch: &str, path: &str, is_main: bool) -> (flotilla_protocol::HostPath, Checkout) {
    let key = flotilla_protocol::test_support::hp(path);
    let checkout = TestCheckout::new(branch).at(path).is_main(is_main).with_branch_key().build();
    (key, checkout)
}
```

This keeps the tuple return signature that snapshot tests depend on.

- [x] **Step 7: Run workspace tests**

Run: `cargo test --workspace --locked`
Expected: all pass.

- [x] **Step 8: Commit**

```bash
git commit -am "refactor: replace 6 make_checkout() copies with TestCheckout builder"
```

---

## Task 5: Replace `make_change_request()`, `make_session()`, `make_issue()` with builders

**Files:**
- Modify: `crates/flotilla-core/src/data.rs` — `make_change_request` (line 976), `make_session` (line 989), `make_issue` (line 1007)
- Modify: `crates/flotilla-core/src/refresh.rs` — `make_change_request` (line 643), `make_session` (line 656)
- Modify: `crates/flotilla-core/src/executor/tests.rs` — `make_issue` (line 300), `make_session_for` (line 287)
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` — `make_issue` (line 2309)
- Modify: `crates/flotilla-tui/tests/support/mod.rs` — `make_change_request` (line 236), `make_issue` (line 250), `make_session` (line 261)

- [x] **Step 1: Replace make_change_request() in core/data.rs**

Delete the function. Update call sites to use `TestChangeRequest::new(title, branch).with_branch_key().build()`. The existing version adds a `Branch` correlation key.

- [x] **Step 2: Replace make_change_request() in core/refresh.rs**

Delete the function. Update call sites. The existing version adds a `Branch` correlation key: `TestChangeRequest::new(title, branch).with_branch_key().build()`.

- [x] **Step 3: Update make_change_request() in tui/tests/support/mod.rs**

Rewrite to delegate:

```rust
pub fn make_change_request(id: &str, title: &str, branch: &str) -> (String, ChangeRequest) {
    (id.to_string(), TestChangeRequest::new(title, branch).with_branch_key().build())
}
```

- [x] **Step 4: Replace make_session() in core/data.rs**

Delete the function. Update call sites. The existing version optionally adds `Branch` and always adds `SessionRef` keys:
- `make_session(id, title, Some(branch))` → `TestSession::new(title).with_session_ref("claude", id).with_branch_key(branch).build()`
- `make_session(id, title, None)` → `TestSession::new(title).with_session_ref("claude", id).build()`

- [x] **Step 5: Replace make_session() in core/refresh.rs**

Delete the function. The existing version adds a `SessionRef("mock", session_id)`:
- `make_session(title, session_id)` → `TestSession::new(title).with_session_ref("mock", session_id).build()`

- [x] **Step 6: Replace make_session_for() in core/executor/tests.rs**

Delete the function. Update call sites:
- `make_session_for(provider, id)` → `TestSession::new("test session").with_session_ref(provider, id).build()`

- [x] **Step 7: Replace make_issue() in core/data.rs, core/executor/tests.rs**

Delete both copies. Update call sites:
- `make_issue(_id, title)` → `TestIssue::new(title).build()`

- [x] **Step 8: Replace make_issue() in core/tests/in_process_daemon.rs**

Delete the function. The existing version sets `provider_name` and `provider_display_name`. The `TestIssue` builder defaults those to empty strings. Check whether the tests assert on those fields. If so, add `.with_provider("fake-issues", "Fake Issues")` to `TestIssue`, or keep a local helper that sets those fields after building:

```rust
fn make_issue(n: u32) -> (String, Issue) {
    let mut issue = TestIssue::new(&format!("Issue {n}")).build();
    issue.provider_name = "fake-issues".into();
    issue.provider_display_name = "Fake Issues".into();
    (n.to_string(), issue)
}
```

- [x] **Step 9: Update make_issue() and make_session() in tui/tests/support/mod.rs**

Rewrite to delegate:

```rust
pub fn make_issue(id: &str, title: &str) -> (String, Issue) {
    (id.to_string(), TestIssue::new(title).build())
}

pub fn make_session(id: &str, title: &str, status: SessionStatus) -> (String, CloudAgentSession) {
    (id.to_string(), TestSession::new(title).with_status(status).build())
}
```

- [x] **Step 10: Run workspace tests**

Run: `cargo test --workspace --locked`
Expected: all pass.

- [x] **Step 11: Commit**

```bash
git commit -am "refactor: replace make_change_request/session/issue copies with shared builders"
```

---

## Task 6: Consolidate `MockPeerSender`, `MockTransport`, `BlockingPeerSender` into `peer/test_support.rs`

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/test_support.rs`
- Modify: `crates/flotilla-daemon/src/peer/mod.rs:5`
- Modify: `crates/flotilla-daemon/src/peer/manager.rs:1324-1384`
- Modify: `crates/flotilla-daemon/src/server/tests.rs:51-89,229-241`
- Modify: `crates/flotilla-daemon/tests/multi_host.rs:37-92,129-141`

- [x] **Step 1: Gate `test_support` module with feature flag**

In `crates/flotilla-daemon/src/peer/mod.rs`, change line 5 from:

```rust
pub mod test_support;
```

to:

```rust
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
```

- [x] **Step 2: Add MockPeerSender to test_support.rs**

In `crates/flotilla-daemon/src/peer/test_support.rs`, add at the top (after existing imports). Use `Mutex` (not aliased) to match the majority pattern in consumer test modules:

```rust
use std::sync::Mutex;
use tokio::sync::Notify;
```

Then add after the existing `TestNetwork` impl:

```rust
// ---------------------------------------------------------------------------
// Mock PeerSender / PeerTransport
// ---------------------------------------------------------------------------

pub struct MockPeerSender {
    pub sent: Arc<Mutex<Vec<PeerWireMessage>>>,
}

impl MockPeerSender {
    pub fn new() -> (Self, Arc<Mutex<Vec<PeerWireMessage>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        (Self { sent: Arc::clone(&sent) }, sent)
    }
}

#[async_trait::async_trait]
impl PeerSender for MockPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        self.sent.lock().expect("lock").push(msg);
        Ok(())
    }

    async fn retire(&self, reason: flotilla_protocol::GoodbyeReason) -> Result<(), String> {
        self.sent.lock().expect("lock").push(PeerWireMessage::Goodbye { reason });
        Ok(())
    }
}
```

The `sent` field is `pub` so callers can construct via struct literal (`MockPeerSender { sent: Arc::clone(&existing) }`) — this matches the 40+ existing call sites in manager.rs and server/tests.rs that pass a pre-existing `Arc`. The `new()` constructor is a convenience for sites that don't need to share a buffer.

- [x] **Step 3: Add BlockingPeerSender to test_support.rs**

```rust
pub struct BlockingPeerSender {
    pub started: Arc<Notify>,
    pub release: Arc<Notify>,
    pub sent: Arc<Mutex<Vec<PeerWireMessage>>>,
}

#[async_trait::async_trait]
impl PeerSender for BlockingPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        self.started.notify_waiters();
        self.release.notified().await;
        self.sent.lock().expect("lock").push(msg);
        Ok(())
    }

    async fn retire(&self, reason: flotilla_protocol::GoodbyeReason) -> Result<(), String> {
        self.started.notify_waiters();
        self.release.notified().await;
        self.sent.lock().expect("lock").push(PeerWireMessage::Goodbye { reason });
        Ok(())
    }
}
```

- [x] **Step 4: Add MockTransport to test_support.rs**

```rust
use crate::peer::transport::{PeerConnectionStatus, PeerTransport};

pub struct MockTransport {
    status: PeerConnectionStatus,
    sender: Option<Arc<dyn PeerSender>>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self { status: PeerConnectionStatus::Connected, sender: None }
    }

    pub fn with_sender() -> (Self, Arc<Mutex<Vec<PeerWireMessage>>>) {
        let (mock_sender, sent) = MockPeerSender::new();
        let sender: Arc<dyn PeerSender> = Arc::new(mock_sender);
        let transport = Self { status: PeerConnectionStatus::Connected, sender: Some(sender) };
        (transport, sent)
    }
}

#[async_trait::async_trait]
impl PeerTransport for MockTransport {
    async fn connect(&mut self) -> Result<(), String> {
        self.status = PeerConnectionStatus::Connected;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        self.status = PeerConnectionStatus::Disconnected;
        Ok(())
    }

    fn status(&self) -> PeerConnectionStatus {
        self.status.clone()
    }

    async fn subscribe(&mut self) -> Result<tokio::sync::mpsc::Receiver<PeerWireMessage>, String> {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(rx)
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        self.sender.clone()
    }
}
```

- [x] **Step 5: Add wait_for_command_result() to test_support.rs**

```rust
use flotilla_protocol::DaemonEvent;

pub async fn wait_for_command_result(
    rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    command_id: u64,
    timeout: std::time::Duration,
) -> flotilla_protocol::commands::CommandValue {
    tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => return result,
                Ok(_) => continue,
                Err(e) => panic!("recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for command result")
}
```

- [x] **Step 6: Delete MockPeerSender and MockTransport from peer/manager.rs test module**

In `crates/flotilla-daemon/src/peer/manager.rs`, delete lines 1324-1384 (the `MockPeerSender` struct+impl and `MockTransport` struct+impl+impl). Replace with imports from test_support:

```rust
use crate::peer::test_support::{MockPeerSender, MockTransport};
```

Update any call sites that construct `MockPeerSender` or `MockTransport` to use the new `::new()` / `::with_sender()` constructors. The existing code constructs them inline (`MockPeerSender { sent: Arc::clone(&sent) }`) — these become `MockPeerSender::new()` which returns the tuple.

- [x] **Step 7: Delete CapturePeerSender, BlockingPeerSender, wait_for_command_result from server/tests.rs**

In `crates/flotilla-daemon/src/server/tests.rs`, delete lines 51-89 (both struct+impl blocks) and lines 229-241 (`wait_for_command_result`). Add imports:

```rust
use crate::peer::test_support::{BlockingPeerSender, MockPeerSender, wait_for_command_result};
```

Update construction sites — `CapturePeerSender { sent }` becomes `MockPeerSender { sent }` (or use `MockPeerSender::new()`). Update `wait_for_command_result(rx, id)` calls to pass a timeout: `wait_for_command_result(rx, id, StdDuration::from_secs(5))`.

- [x] **Step 8: Delete MockPeerSender, MockTransport, make_checkout, wait_for_command_result from multi_host.rs**

In `crates/flotilla-daemon/tests/multi_host.rs`, delete the `MockTransport` (lines 37-92), `MockPeerSender` (lines 51-66), `make_checkout` (lines 102-112), and `wait_for_command_result` (lines 129-141) definitions. Add imports:

```rust
use flotilla_daemon::peer::test_support::{MockPeerSender, MockTransport, wait_for_command_result};
use flotilla_protocol::test_support::TestCheckout;
```

Update `make_checkout(branch)` calls to `TestCheckout::new(branch).build()`. Update `wait_for_command_result(rx, id)` calls to pass timeout: `wait_for_command_result(rx, id, Duration::from_secs(10))`.

- [x] **Step 9: Run workspace tests**

Run: `cargo test --workspace --locked`
Expected: all pass.

- [x] **Step 10: Commit**

```bash
git commit -am "refactor: consolidate MockPeerSender, MockTransport, BlockingPeerSender into peer/test_support"
```

---

## Task 7: Consolidate `CommandRunner` mock in `flotilla-core`

**Files:**
- Modify: `crates/flotilla-core/src/providers/mod.rs:355-395`
- Modify: `crates/flotilla-core/src/providers/terminal/cleat.rs:106-131`

- [x] **Step 1: Add call tracking to the existing MockRunner in providers/mod.rs**

In `crates/flotilla-core/src/providers/mod.rs` (lines 355-395), the `testing` module contains `MockRunner`. Add a `calls` field:

```rust
pub struct MockRunner {
    responses: std::sync::Mutex<VecDeque<Result<String, String>>>,
    calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
}

impl MockRunner {
    pub fn new(responses: Vec<Result<String, String>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into()),
            calls: std::sync::Mutex::new(vec![]),
        }
    }

    pub fn remaining(&self) -> usize {
        self.responses.lock().expect("MockRunner responses mutex not poisoned").len()
    }

    pub fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().expect("calls").clone()
    }
}
```

Update the `run()` impl to record calls:

```rust
async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
    self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|a| (*a).into()).collect()));
    self.responses.lock().unwrap().pop_front().expect("MockRunner: no more responses")
}
```

Keep the existing `run_output()` implementation from `providers/mod.rs` (which maps `Ok` → success, `Err` → `CommandOutput` with `success: false`). The cleat.rs version is less precise (always returns `Ok(CommandOutput)`) — verify cleat tests still pass with the providers/mod.rs behavior.

- [x] **Step 2: Replace cleat.rs MockRunner with import**

In `crates/flotilla-core/src/providers/terminal/cleat.rs`, delete the local `MockRunner` struct and impl (lines 106-131). Replace with:

```rust
use crate::providers::testing::MockRunner;
```

Update any test code that accesses `runner.calls.lock()` directly to use the new `runner.calls()` method instead.

- [x] **Step 3: Run core tests**

Run: `cargo test -p flotilla-core --locked`
Expected: all pass.

- [x] **Step 4: Commit**

```bash
git commit -am "refactor: consolidate CommandRunner mocks into providers::testing::MockRunner"
```

---

## Task 8: Extract `data.rs` test module to `data/tests.rs`

**Files:**
- Modify: `crates/flotilla-core/src/data.rs:887-2371`
- Create: `crates/flotilla-core/src/data/tests.rs`

- [x] **Step 1: Create the directory**

Run: `mkdir -p crates/flotilla-core/src/data`

- [x] **Step 2: Move the test module body**

Cut the *contents* of `mod tests { ... }` — everything between the opening `{` (line 888) and the closing `}` (last line of file). Write these contents to `crates/flotilla-core/src/data/tests.rs`. The extracted file should start with the `use` imports that were inside the module (e.g. `use super::*;`) and contain all test functions and helpers. Do not include the `mod tests {` or closing `}` lines — those are replaced by the `#[path]` declaration.

- [x] **Step 3: Replace inline module with path declaration**

In `crates/flotilla-core/src/data.rs`, replace the entire `#[cfg(test)] mod tests { ... }` block with:

```rust
#[cfg(test)]
#[path = "data/tests.rs"]
mod tests;
```

- [x] **Step 4: Run data.rs tests**

Run: `cargo test -p flotilla-core --locked -- data::tests`
Expected: all pass.

- [x] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/data.rs crates/flotilla-core/src/data/
git commit -m "refactor: extract data.rs test module to data/tests.rs"
```

---

## Task 9: Extract `key_handlers.rs`, `intent.rs`, `manager.rs` test modules

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:334-1944`
- Create: `crates/flotilla-tui/src/app/key_handlers/tests.rs`
- Modify: `crates/flotilla-tui/src/app/intent.rs:230-1314`
- Create: `crates/flotilla-tui/src/app/intent/tests.rs`
- Modify: `crates/flotilla-daemon/src/peer/manager.rs:1314-2529`
- Create: `crates/flotilla-daemon/src/peer/manager/tests.rs`

- [x] **Step 1: Create directories**

```bash
mkdir -p crates/flotilla-tui/src/app/key_handlers
mkdir -p crates/flotilla-tui/src/app/intent
mkdir -p crates/flotilla-daemon/src/peer/manager
```

- [x] **Step 2: Extract key_handlers.rs tests**

Move the test module body from `key_handlers.rs` (line 335 onward) to `key_handlers/tests.rs`. Replace inline module with:

```rust
#[cfg(test)]
#[path = "key_handlers/tests.rs"]
mod tests;
```

- [x] **Step 3: Verify key_handlers tests**

Run: `cargo test -p flotilla-tui --locked -- app::key_handlers::tests`
Expected: all pass.

- [x] **Step 4: Extract intent.rs tests**

Move the test module body from `intent.rs` (line 231 onward) to `intent/tests.rs`. Replace inline module with:

```rust
#[cfg(test)]
#[path = "intent/tests.rs"]
mod tests;
```

- [x] **Step 5: Verify intent tests**

Run: `cargo test -p flotilla-tui --locked -- app::intent::tests`
Expected: all pass.

- [x] **Step 6: Extract manager.rs tests**

Move the test module body from `manager.rs` (line 1315 onward) to `manager/tests.rs`. The `MockPeerSender` and `MockTransport` definitions were already removed in Task 6 — they're imported from `test_support`. Replace inline module with:

```rust
#[cfg(test)]
#[path = "manager/tests.rs"]
mod tests;
```

- [x] **Step 7: Verify manager tests**

Run: `cargo test -p flotilla-daemon --locked -- peer::manager::tests`
Expected: all pass.

- [x] **Step 8: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/app/key_handlers/ \
       crates/flotilla-tui/src/app/intent.rs crates/flotilla-tui/src/app/intent/ \
       crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/peer/manager/
git commit -m "refactor: extract key_handlers, intent, and manager test modules to separate files"
```

---

## Task 10: Parameterize `data.rs` accessor tests

**Files:**
- Modify: `crates/flotilla-core/src/data/tests.rs` (created in Task 8)

- [x] **Step 1: Identify which accessor test groups are parameterizable**

Read through the extracted `data/tests.rs`. For each group delimited by `// ---` comment separators, check whether all tests in the group follow the pattern: create item → call accessor → assert single value. Skip tests that have custom setup (e.g. constructing `CorrelatedWorkItem` directly).

- [x] **Step 2: Parameterize `kind()` tests**

Replace the five individual `kind_*` tests (lines ~1130-1158 in the original) with a single test:

```rust
#[test]
fn kind_returns_correct_variant() {
    let cases = [
        ("checkout", checkout_item("/tmp/foo", None, false), WorkItemKind::Checkout),
        ("change_request", cr_item("42", "PR title"), WorkItemKind::ChangeRequest),
        ("session", session_item("sess-1", "Session title"), WorkItemKind::Session),
        ("issue", issue_item("7", "Fix bug"), WorkItemKind::Issue),
        ("remote_branch", remote_branch_item("feature/x"), WorkItemKind::RemoteBranch),
    ];
    for (label, item, expected) in cases {
        assert_eq!(item.kind(), expected, "failed for {label}");
    }
}
```

- [x] **Step 3: Parameterize `description()` tests**

Replace the three `description_*` tests with:

```rust
#[test]
fn description_returns_expected_value() {
    let cases = [
        ("correlated", cr_item("1", "Fix login flow"), "Fix login flow"),
        ("standalone_issue", issue_item("5", "Add caching"), "Add caching"),
        ("remote_branch", remote_branch_item("feature/auth"), "feature/auth"),
    ];
    for (label, item, expected) in cases {
        assert_eq!(item.description(), expected, "failed for {label}");
    }
}
```

- [x] **Step 4: Parameterize remaining accessor groups**

Apply the same pattern to other groups where all tests follow the create→access→assert pattern. For groups where some tests have custom setup (like `branch_from_change_request_correlated`), keep those as individual tests and parameterize only the mechanical ones.

- [x] **Step 5: Run data tests**

Run: `cargo test -p flotilla-core --locked -- data::tests`
Expected: all pass. Same number of logical assertions, fewer test functions.

- [x] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/data/tests.rs
git commit -m "refactor: parameterize data.rs accessor tests into table-driven tests"
```

---

## Task 11: Final verification

- [x] **Step 1: Run full CI checks**

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Expected: all three pass.

- [x] **Step 2: Fix any formatting or clippy issues**

Run `cargo +nightly-2026-03-12 fmt` if formatting check fails. Fix any clippy warnings introduced by the refactoring.

- [x] **Step 3: Commit fixes if needed**

```bash
git commit -am "chore: fix formatting and clippy warnings from test consolidation"
```
