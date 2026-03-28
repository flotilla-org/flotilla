# High-Fidelity TUI Integration Testing Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a high-fidelity single-process TUI workflow harness that runs one real `App` over the real client/server daemon comms path, against a leader daemon and a connected follower daemon, and proves a remote checkout-removal flow surfaces progress before completion.

**Architecture:** First extract a shared `Message` session transport seam so client and server can communicate over either Unix sockets or an in-memory transport. Then refactor the client and server request path onto that seam, prove remote routing works over an in-memory client/server session plus real peer runtime, and finally build the TUI harness on top. Keep providers fake and stepped below the daemon boundary so intermediate states are deterministic and failures point at the real seam that agents keep breaking.

**Tech Stack:** Rust, Tokio, ratatui `TestBackend`, `InProcessDaemon`, daemon peer channel transport, `flotilla-core` fake discovery/test-support, `flotilla-daemon` request dispatch, new `flotilla-transport` crate

---

## File Structure

- Create: `crates/flotilla-transport/Cargo.toml`
  Responsibility: shared transport/session crate used by both client and daemon.
- Create: `crates/flotilla-transport/src/lib.rs`
  Responsibility: export the generic in-memory session primitive and `Message`-typed session API.
- Create: `crates/flotilla-transport/src/memory.rs`
  Responsibility: generic paired in-memory session endpoints and lifecycle behavior.
- Create: `crates/flotilla-transport/src/message.rs`
  Responsibility: `Message`-typed session wrapper and socket-backed constructors/adapters.
- Modify: `Cargo.toml`
  Responsibility: add the new crate to the workspace.
- Modify: `crates/flotilla-client/Cargo.toml`
  Responsibility: depend on `flotilla-transport`.
- Modify: `crates/flotilla-daemon/Cargo.toml`
  Responsibility: depend on `flotilla-transport`.
- Modify: `crates/flotilla-client/src/lib.rs`
  Responsibility: refactor `SocketDaemon` around a transport-agnostic session constructor.
- Modify: `crates/flotilla-client/src/lib/tests.rs`
  Responsibility: move client tests to the new session seam where possible.
- Modify: `crates/flotilla-daemon/src/server.rs`
  Responsibility: split socket acceptance from transport-agnostic request-session handling and expose test support as needed.
- Modify: `crates/flotilla-daemon/src/server/shared.rs`
  Responsibility: remove Unix-specific aliases from the higher-level request path and keep only shared helpers that still belong here.
- Modify: `crates/flotilla-daemon/src/server/client_connection.rs`
  Responsibility: run over a transport-agnostic `Message` session writer/reader.
- Create: `crates/flotilla-daemon/src/server/test_support.rs`
  Responsibility: in-process request-session and peer-runtime helpers behind `test-support`.
- Create: `crates/flotilla-daemon/tests/request_session_pair.rs`
  Responsibility: contract tests for client/server request handling over the in-memory session.
- Modify: `crates/flotilla-tui/Cargo.toml`
  Responsibility: add daemon test-support and client transport pieces as dev-dependencies if needed.
- Create: `crates/flotilla-tui/tests/support/high_fidelity.rs`
  Responsibility: stepped fake provider(s), leader/follower plus request-session harness, event draining helpers, and rendering helpers.
- Modify: `crates/flotilla-tui/tests/support/mod.rs`
  Responsibility: export the new high-fidelity support module and keep shared rendering helpers in one place.
- Create: `crates/flotilla-tui/tests/high_fidelity.rs`
  Responsibility: the first high-fidelity regression test for remote checkout removal with visible in-flight progress.

## Chunk 1: Shared Transport Session Crate

### Task 1: Add `flotilla-transport` with reusable in-memory and `Message` session types

**Files:**
- Create: `crates/flotilla-transport/Cargo.toml`
- Create: `crates/flotilla-transport/src/lib.rs`
- Create: `crates/flotilla-transport/src/memory.rs`
- Create: `crates/flotilla-transport/src/message.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Write the failing transport tests**

Add focused tests that prove:

```rust
#[tokio::test]
async fn memory_session_pair_delivers_messages_bidirectionally() { /* ... */ }

#[tokio::test]
async fn dropping_one_endpoint_closes_the_other_reader() { /* ... */ }
```

Use a simple generic message type first, then a `flotilla_protocol::Message` variant to prove the typed wrapper.

- [ ] **Step 2: Run the new transport tests to verify they fail**

Run: `cargo test -p flotilla-transport --locked`

Expected: FAIL because the crate and session types do not exist yet.

- [ ] **Step 3: Implement the minimal session API**

The target shape should be small and transport-focused:

```rust
pub struct SessionReader<M> { /* ... */ }
pub struct SessionWriter<M> { /* ... */ }
pub struct Session<M> {
    pub reader: SessionReader<M>,
    pub writer: SessionWriter<M>,
}

pub fn memory_session_pair<M>() -> (Session<M>, Session<M>);

pub struct MessageSession { /* wraps Session<Message> */ }
pub fn message_session_pair() -> (MessageSession, MessageSession);
```

Implementation notes:

- Keep the generic primitive independent of daemon/client logic.
- Model disconnect/close behavior explicitly.
- Do not move business logic into the new crate.
- Keep reconnect/session-rotation out of v1 unless the request path truly needs it.

- [ ] **Step 4: Run the transport tests to verify they pass**

Run: `cargo test -p flotilla-transport --locked`

Expected: PASS

- [ ] **Step 5: Commit the transport crate**

```bash
git add Cargo.toml crates/flotilla-transport
git commit -m "refactor: add shared message transport sessions"
```

## Chunk 2: Refactor The Client Onto The Session Boundary

### Task 2: Make `SocketDaemon` session-backed instead of Unix-socket-backed

**Files:**
- Modify: `crates/flotilla-client/Cargo.toml`
- Modify: `crates/flotilla-client/src/lib.rs`
- Modify: `crates/flotilla-client/src/lib/tests.rs`

- [ ] **Step 1: Write the failing client parity tests around a session constructor**

Add tests covering:

```rust
#[tokio::test]
async fn session_backed_daemon_sends_requests_and_receives_responses() { /* ... */ }

#[tokio::test]
async fn session_backed_daemon_streams_events_to_subscribers() { /* ... */ }
```

These should avoid real Unix sockets and use `message_session_pair()`.

- [ ] **Step 2: Run the client package tests to verify the new tests fail**

Run: `cargo test -p flotilla-client --locked`

Expected: FAIL because `SocketDaemon::from_session` or equivalent does not exist yet.

- [ ] **Step 3: Refactor `SocketDaemon`**

Target shape:

```rust
impl SocketDaemon {
    pub async fn connect(socket_path: &Path) -> Result<Arc<Self>, String> {
        let session = flotilla_transport::message::connect_unix_message_session(socket_path).await?;
        Self::from_session(session)
    }

    pub fn from_session(session: MessageSession) -> Result<Arc<Self>, String> { /* ... */ }
}
```

Implementation notes:

- Move reader/writer plumbing off raw `OwnedReadHalf`/`OwnedWriteHalf`.
- Preserve pending-request, timeout, disconnect, and replay/gap-recovery behavior.
- Keep `connect_or_spawn` behavior unchanged apart from constructing the session first.

- [ ] **Step 4: Run the client package tests to verify parity**

Run: `cargo test -p flotilla-client --locked`

Expected: PASS

- [ ] **Step 5: Commit the client refactor**

```bash
git add crates/flotilla-client/Cargo.toml crates/flotilla-client/src/lib.rs crates/flotilla-client/src/lib/tests.rs
git commit -m "refactor: back socket daemon with message sessions"
```

## Chunk 3: Refactor The Server Request Path Onto The Session Boundary

### Task 3: Add a transport-agnostic request-session entrypoint on the daemon side

**Files:**
- Modify: `crates/flotilla-daemon/Cargo.toml`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/src/server/shared.rs`
- Modify: `crates/flotilla-daemon/src/server/client_connection.rs`
- Create: `crates/flotilla-daemon/tests/request_session_pair.rs`

- [ ] **Step 1: Write the failing daemon request-session contract tests**

Add tests proving:

```rust
#[tokio::test]
async fn request_session_streams_daemon_events_to_clients() { /* ... */ }

#[tokio::test]
async fn request_session_dispatches_remote_execute_via_router() { /* ... */ }
```

The first should mirror the current `handle_client_streams_daemon_events_to_request_clients` test but without `UnixStream::pair()`. The second should be the first proof that a client request routed through the real request-dispatch path can reach remote-command handling.

- [ ] **Step 2: Run the daemon tests to verify they fail**

Run: `cargo test -p flotilla-daemon --locked --features test-support request_session`

Expected: FAIL because the transport-agnostic session entrypoint does not exist yet.

- [ ] **Step 3: Refactor the server request path**

Split the current request-client path into:

```rust
async fn handle_client(stream: tokio::net::UnixStream, ...) {
    let session = flotilla_transport::message::unix_message_session(stream);
    handle_client_session(session, ...).await;
}

async fn handle_client_session(session: MessageSession, ...) { /* existing request path */ }
```

Implementation notes:

- Keep the first-message dispatch split (`Request` vs `Hello`) in the socket wrapper if that is still the cleanest boundary.
- The request-client path should run over the session seam.
- Leave peer socket handling alone for now unless a tiny shared helper falls out naturally.

- [ ] **Step 4: Run the daemon package tests to verify parity**

Run: `cargo test -p flotilla-daemon --locked --features test-support`

Expected: PASS

- [ ] **Step 5: Commit the server refactor**

```bash
git add crates/flotilla-daemon/Cargo.toml crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/server/shared.rs crates/flotilla-daemon/src/server/client_connection.rs crates/flotilla-daemon/tests/request_session_pair.rs
git commit -m "refactor: split daemon request handling from unix sockets"
```

## Chunk 4: In-Process Request-Session And Peer Test Support

### Task 4: Add daemon test support for a real request client plus real peer runtime

**Files:**
- Create: `crates/flotilla-daemon/src/server/test_support.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/tests/request_session_pair.rs`

- [ ] **Step 1: Write the failing high-level daemon integration test**

```rust
#[tokio::test]
async fn in_memory_request_client_routes_remote_command_result() {
    let harness = spawn_in_memory_request_topology(/* leader daemon, follower daemon */).await;

    let command_id = harness
        .client
        .execute(Command {
            host: Some(HostName::new("follower")),
            environment: None,
            context_repo: Some(RepoSelector::Identity(harness.repo_identity.clone())),
            action: CommandAction::RemoveCheckout {
                checkout: CheckoutSelector::Query("feat-remote".into()),
            },
        })
        .await
        .expect("dispatch remote remove");

    let result = harness.wait_for_command_result(command_id).await;
    assert!(matches!(result, CommandValue::CheckoutRemoved { branch } if branch == "feat-remote"));
}
```

- [ ] **Step 2: Run the targeted daemon test to verify it fails**

Run: `cargo test -p flotilla-daemon --locked --features test-support in_memory_request_client_routes_remote_command_result`

Expected: FAIL because the request-topology helper does not exist yet.

- [ ] **Step 3: Implement the daemon-side helper**

Add a test-only helper that owns:

```rust
pub struct InMemoryRequestTopology {
    pub leader: Arc<InProcessDaemon>,
    pub follower: Arc<InProcessDaemon>,
    pub client: Arc<dyn DaemonHandle>,
    pub leader_host: HostName,
    pub follower_host: HostName,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}
```

Implementation notes:

- Build the leader/follower peer managers and connect them with the existing in-memory peer transport.
- Start the real peer runtime for both daemons.
- Start a real server-side request session for the leader daemon.
- Connect a real client-side daemon handle over `message_session_pair()`.
- Return tasks so the harness can abort cleanly on drop.

- [ ] **Step 4: Run the targeted daemon test to verify it passes**

Run: `cargo test -p flotilla-daemon --locked --features test-support in_memory_request_client_routes_remote_command_result`

Expected: PASS

- [ ] **Step 5: Commit the daemon test support**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/server/test_support.rs crates/flotilla-daemon/tests/request_session_pair.rs
git commit -m "test: add in-memory request topology support"
```

## Chunk 5: TUI High-Fidelity Harness And First Workflow

### Task 5: Write the failing high-fidelity TUI workflow test

**Files:**
- Modify: `crates/flotilla-tui/Cargo.toml`
- Create: `crates/flotilla-tui/tests/high_fidelity.rs`
- Create: `crates/flotilla-tui/tests/support/high_fidelity.rs`
- Modify: `crates/flotilla-tui/tests/support/mod.rs`
- Spec: `docs/superpowers/specs/2026-03-27-high-fidelity-tui-integration-testing-design.md`

- [ ] **Step 1: Write the failing remote removal progress test**

```rust
#[tokio::test]
async fn remote_checkout_removal_surfaces_progress_in_status_bar() {
    let mut harness = HighFidelityHarness::remote_checkout_removal().await;

    harness.press(key(KeyCode::Char('d')));
    harness.drive_until_delete_confirm_loaded().await;

    harness.press(key(KeyCode::Char('y')));
    harness.drive_until_progress_visible("Remove feat-remote").await;

    let frame = harness.render_to_string();
    assert!(frame.contains("Remove feat-remote"));

    harness.release_remote_remove();
    harness.drive_until_workflow_complete().await;

    let final_frame = harness.render_to_string();
    assert!(!final_frame.contains("Remove feat-remote"));
    assert!(!harness.remote_checkout_still_present());
}
```

The test should use the real user flow:

- leader TUI sees a follower-owned checkout item
- `d` opens delete confirm and queues `FetchCheckoutStatus`
- real daemon events populate the confirm dialog
- `y` triggers `RemoveCheckout`
- the follower-side stepped provider blocks mid-remove
- the status bar shows in-flight progress before the provider is released

- [ ] **Step 2: Run the TUI test to verify it fails**

Run: `cargo test -p flotilla-tui --locked --test high_fidelity remote_checkout_removal_surfaces_progress_in_status_bar`

Expected: FAIL because the harness, stepped provider support, and request-topology support do not exist yet.

- [ ] **Step 3: Implement the TUI high-fidelity harness**

Add the daemon dev-dependency:

```toml
[dev-dependencies]
flotilla-daemon = { path = "../flotilla-daemon", features = ["test-support"] }
```

Add a stepped fake checkout manager in `crates/flotilla-tui/tests/support/high_fidelity.rs`:

```rust
struct SteppedCheckoutManager {
    inner: FakeCheckoutManager,
    remove_started: Notify,
    remove_release: Notify,
}

#[async_trait]
impl CheckoutManager for SteppedCheckoutManager {
    async fn remove_checkout(&self, repo_root: &ExecutionEnvironmentPath, branch: &str) -> Result<(), String> {
        self.remove_started.notify_waiters();
        self.remove_release.notified().await;
        self.inner.remove_checkout(repo_root, branch).await
    }
}
```

Add a `HighFidelityHarness` that owns:

```rust
pub struct HighFidelityHarness {
    pub app: App,
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
    client: Arc<dyn DaemonHandle>,
    client_rx: tokio::sync::broadcast::Receiver<DaemonEvent>,
    stepped_checkouts: Arc<SteppedCheckoutManager>,
    _topology: InMemoryRequestTopology,
}
```

Required helper methods:

- `remote_checkout_removal() -> Self`
- `press(KeyEvent)`
- `dispatch_queued_commands().await`
- `drain_daemon_events().await`
- `drive_until_*()` helpers with bounded timeouts
- `render_to_string()`
- `release_remote_remove()`
- `remote_checkout_still_present()`

Implementation notes:

- Build leader and follower daemons from fake discovery plus deterministic provider state.
- Seed a follower-owned checkout so the leader view includes a removable remote work item.
- Subscribe the app through the real client-side daemon handle broadcast channel.
- Drive the same ordering as `run_event_loop`: input, queued command dispatch, background updates, daemon event application, render.
- Assert on rendered status-bar text for the user-visible progress check; do not assert only on internal fields like `in_flight.description`.

- [ ] **Step 4: Run the TUI test to verify it passes**

Run: `cargo test -p flotilla-tui --locked --test high_fidelity remote_checkout_removal_surfaces_progress_in_status_bar`

Expected: PASS

- [ ] **Step 5: Commit the TUI harness and first regression test**

```bash
git add crates/flotilla-tui/Cargo.toml crates/flotilla-tui/tests/high_fidelity.rs crates/flotilla-tui/tests/support/mod.rs crates/flotilla-tui/tests/support/high_fidelity.rs
git commit -m "test: add high-fidelity tui remote workflow coverage"
```

## Chunk 6: Focused Verification And Follow-Up Assessment

### Task 6: Run focused package verification and note remaining follow-up areas

**Files:**
- Modify: `crates/flotilla-daemon/tests/request_session_pair.rs` (only if verification exposes flakiness)
- Modify: `crates/flotilla-tui/tests/high_fidelity.rs` (only if verification exposes timing or assertion issues)
- Modify: `crates/flotilla-transport/src/message.rs` (only if verification exposes disconnect semantics issues)

- [ ] **Step 1: Run the transport package**

Run: `cargo test -p flotilla-transport --locked`

Expected: PASS

- [ ] **Step 2: Run the client package**

Run: `cargo test -p flotilla-client --locked`

Expected: PASS

- [ ] **Step 3: Run the daemon package**

Run: `cargo test -p flotilla-daemon --locked --features test-support`

Expected: PASS

- [ ] **Step 4: Run the TUI package**

Run: `cargo test -p flotilla-tui --locked`

Expected: PASS

- [ ] **Step 5: If package-level failures appear, tighten the harness rather than weakening assertions**

Fix likely issues in this order:

1. missing bounded waits in async helpers
2. request session disconnect semantics not matching socket behavior
3. peer runtime not fully settled before first TUI action
4. render helper asserting before events are drained
5. provider stepping leaking across tests

- [ ] **Step 6: Re-run the focused package commands**

Run: `cargo test -p flotilla-transport --locked`

Run: `cargo test -p flotilla-client --locked`

Run: `cargo test -p flotilla-daemon --locked --features test-support`

Run: `cargo test -p flotilla-tui --locked`

Expected: PASS

- [ ] **Step 7: Commit any verification-driven fixes**

```bash
git add crates/flotilla-transport crates/flotilla-client crates/flotilla-daemon crates/flotilla-tui
git commit -m "test: stabilize high-fidelity tui transport harness"
```

## Notes For Execution

- Do not shortcut this into scripted `DaemonHandle` fakes. The whole point is to preserve the real `App -> client daemon handle -> request session -> server dispatch -> peer runtime -> daemon events -> App` loop.
- Do not pull embedded-mode routing fixes into this work unless they become an unavoidable blocker after the request-session path is proven.
- Prefer one explicit remote-checkout-removal scenario over a generic harness abstraction that tries to serve every future test on day one.
- If the first rendered-progress assertion proves noisy, keep the rendered assertion and make the helper more deterministic; do not replace it with a purely internal-state assertion.
- Reassess whether peer socket code should also adopt the new transport seam only after the request-client path is landed and verified.
