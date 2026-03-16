# Protocol Typed Messages Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace ad hoc daemon RPC JSON payloads with shared typed protocol enums and rename repo-specific snapshot types/events to `RepoSnapshot` and `RepoDelta`.

**Architecture:** Keep the existing top-level JSON message envelope and request `id` correlation, but replace `method` plus `serde_json::Value` with typed `Request` and `ResponseResult` enums in `flotilla-protocol`. Migrate the daemon and client to those enums, then update downstream crates for the repo snapshot rename and remove obsolete raw JSON helpers.

**Tech Stack:** Rust, serde, tokio, cargo test

---

## File Structure

- Modify: `crates/flotilla-protocol/src/lib.rs`
  Responsibility: define typed request/response protocol enums, rename daemon repo snapshot events, remove `RawResponse`-style public protocol parsing.
- Modify: `crates/flotilla-protocol/src/snapshot.rs`
  Responsibility: rename `Snapshot` to `RepoSnapshot` and adjust serde tests.
- Modify: `crates/flotilla-protocol/src/framing.rs`
  Responsibility: update framing tests to the new request shape.
- Modify: `crates/flotilla-client/src/lib.rs`
  Responsibility: send typed requests, receive typed response results, validate response variants centrally.
- Modify: `crates/flotilla-daemon/src/server.rs`
  Responsibility: dispatch on typed `Request` variants and build typed `ResponseResult` values.
- Modify: `crates/flotilla-core/src/daemon.rs`
  Responsibility: update trait signatures and comments for `RepoSnapshot` and `RepoDelta`.
- Modify: `crates/flotilla-core/src/convert.rs`
  Responsibility: return the renamed repo snapshot type.
- Modify: `crates/flotilla-core/src/in_process.rs`
  Responsibility: emit renamed daemon events and renamed repo snapshot types.
- Modify: `crates/flotilla-tui/src/app/mod.rs`
  Responsibility: consume `DaemonEvent::RepoSnapshot` and `DaemonEvent::RepoDelta`.
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
  Responsibility: update test factories for renamed repo snapshot types.
- Modify: `crates/flotilla-tui/src/cli.rs`
  Responsibility: update event formatting and tests for renamed repo snapshot events.
- Modify: `crates/flotilla-daemon/tests/socket_roundtrip.rs`
  Responsibility: update daemon socket integration expectations.
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
  Responsibility: update replay/event helpers to renamed repo snapshot events.
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
  Responsibility: update integration coverage for renamed repo snapshot events.

## Chunk 1: Protocol Types

### Task 1: Add failing protocol serde coverage for typed requests and responses

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-protocol/src/framing.rs`
- Test: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Add request round-trip tests for typed request variants**

Add tests that serialize and deserialize:

```rust
Message::Request { id: 42, request: Request::GetState { repo: PathBuf::from("/tmp/my-repo") } }
Message::Request { id: 7, request: Request::ListRepos }
Message::Request { id: 9, request: Request::Execute { command: sample_command() } }
```

- [ ] **Step 2: Add response round-trip tests for typed success and error responses**

Add tests that serialize and deserialize:

```rust
Message::Response { id: 1, response: ResponseResult::Ok(Response::ListRepos(vec![])) }
Message::Response { id: 2, response: ResponseResult::Ok(Response::Execute { command_id: 99 }) }
Message::Response { id: 3, response: ResponseResult::Err { message: "not found".into() } }
```

- [ ] **Step 3: Run protocol tests to verify they fail before implementation**

Run: `cargo test -p flotilla-protocol --locked`
Expected: FAIL with missing `Request`, `Response`, `ResponseResult`, and updated `Message` fields.

- [ ] **Step 4: Commit the failing tests**

```bash
git add crates/flotilla-protocol/src/lib.rs crates/flotilla-protocol/src/framing.rs
git commit -m "test: add typed protocol message coverage"
```

### Task 2: Implement typed request/response protocol enums

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-protocol/src/framing.rs`
- Test: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Define protocol enums and update `Message`**

Implement:

```rust
pub enum Request { /* approved RPC variants */ }
pub enum Response { /* approved success variants */ }
pub enum ResponseResult {
    Ok(Response),
    Err { message: String },
}
```

and change:

```rust
Message::Request { id, request: Request }
Message::Response { id, response: ResponseResult }
```

- [ ] **Step 2: Replace `ok_response` and `error_response` helpers**

Update helper constructors to be typed:

```rust
pub fn ok_response(id: u64, response: Response) -> Self
pub fn error_response(id: u64, message: impl Into<String>) -> Self
```

Remove `empty_ok_response` in favor of unit `Response` variants.

- [ ] **Step 3: Remove the public `RawResponse` helper**

Delete `RawResponse` from the shared protocol API unless a transport-only internal equivalent is still required elsewhere.

- [ ] **Step 4: Run protocol tests to verify they pass**

Run: `cargo test -p flotilla-protocol --locked`
Expected: PASS

- [ ] **Step 5: Commit the typed protocol model**

```bash
git add crates/flotilla-protocol/src/lib.rs crates/flotilla-protocol/src/framing.rs
git commit -m "refactor: type daemon protocol messages"
```

## Chunk 2: Repo Snapshot Rename

### Task 3: Add failing tests for repo snapshot naming

**Files:**
- Modify: `crates/flotilla-protocol/src/snapshot.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Test: `crates/flotilla-protocol/src/snapshot.rs`

- [ ] **Step 1: Update tests to the new names before implementation**

Rename test references to:

```rust
RepoSnapshot
RepoDelta
DaemonEvent::RepoSnapshot
DaemonEvent::RepoDelta
```

- [ ] **Step 2: Run protocol tests to verify rename fallout is exposed**

Run: `cargo test -p flotilla-protocol --locked`
Expected: FAIL with unresolved old type and variant names.

- [ ] **Step 3: Commit the failing rename tests**

```bash
git add crates/flotilla-protocol/src/lib.rs crates/flotilla-protocol/src/snapshot.rs
git commit -m "test: cover repo snapshot naming"
```

### Task 4: Implement renamed repo snapshot types and daemon events

**Files:**
- Modify: `crates/flotilla-protocol/src/snapshot.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Test: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Rename protocol snapshot structs**

Rename:

```rust
pub struct Snapshot -> pub struct RepoSnapshot
pub struct SnapshotDelta -> pub struct RepoDelta
```

Update exports and all protocol-local references.

- [ ] **Step 2: Rename repo snapshot daemon events**

Rename:

```rust
DaemonEvent::SnapshotFull -> DaemonEvent::RepoSnapshot
DaemonEvent::SnapshotDelta -> DaemonEvent::RepoDelta
```

Keep serde `kind` names explicit and aligned with the new terminology.

- [ ] **Step 3: Run protocol tests to verify the rename passes**

Run: `cargo test -p flotilla-protocol --locked`
Expected: PASS

- [ ] **Step 4: Commit the rename**

```bash
git add crates/flotilla-protocol/src/lib.rs crates/flotilla-protocol/src/snapshot.rs
git commit -m "refactor: rename repo snapshot protocol types"
```

## Chunk 3: Daemon And Client Migration

### Task 5: Add failing daemon dispatch tests for typed requests

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`
- Test: `crates/flotilla-daemon/src/server.rs`

- [ ] **Step 1: Update or add tests that call dispatch with typed requests**

Change test inputs from string methods plus JSON params to:

```rust
Message::Request { id, request: Request::AddRepo { path } }
Message::Request { id, request: Request::Cancel { command_id } }
```

- [ ] **Step 2: Run the targeted daemon tests to verify they fail**

Run: `cargo test -p flotilla-daemon --locked server:: -- --nocapture`
Expected: FAIL with mismatched `dispatch_request` signature or old response constructors.

- [ ] **Step 3: Commit the failing daemon tests**

```bash
git add crates/flotilla-daemon/src/server.rs
git commit -m "test: update daemon dispatch for typed requests"
```

### Task 6: Implement typed request dispatch in the daemon

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`
- Test: `crates/flotilla-daemon/src/server.rs`

- [ ] **Step 1: Change `dispatch_request` to match on `Request`**

Update the function shape from:

```rust
async fn dispatch_request(..., method: &str, params: serde_json::Value) -> Message
```

to:

```rust
async fn dispatch_request(..., request: Request) -> Message
```

- [ ] **Step 2: Delete manual JSON extract helpers that are no longer needed**

Remove or inline obsolete helpers such as:

```rust
extract_repo_path
extract_path_param
extract_str_param
```

Keep only helpers that still express real domain logic.

- [ ] **Step 3: Build typed responses directly**

Return variants like:

```rust
Message::ok_response(id, Response::GetState(snapshot))
Message::ok_response(id, Response::Execute { command_id })
Message::ok_response(id, Response::Cancel)
```

- [ ] **Step 4: Run daemon tests to verify they pass**

Run: `cargo test -p flotilla-daemon --locked server:: -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit the daemon migration**

```bash
git add crates/flotilla-daemon/src/server.rs
git commit -m "refactor: dispatch typed daemon requests"
```

### Task 7: Add failing client tests for typed request and response handling

**Files:**
- Modify: `crates/flotilla-client/src/lib.rs`
- Test: `crates/flotilla-client/src/lib.rs`

- [ ] **Step 1: Update test helpers to send typed response payloads**

Replace test fixtures like:

```rust
RawResponse { ok: true, data: Some(...), error: None }
```

with typed `Message::Response` or transport-local decoded equivalents carrying:

```rust
ResponseResult::Ok(Response::ReplaySince(events))
ResponseResult::Err { message: "internal error".into() }
```

- [ ] **Step 2: Add a response mismatch test**

Add a test that expects one response kind but receives another and asserts a protocol error such as:

```rust
"unexpected response kind: expected GetState, got ListRepos"
```

- [ ] **Step 3: Run client tests to verify they fail**

Run: `cargo test -p flotilla-client --locked`
Expected: FAIL with old `RawResponse` assumptions.

- [ ] **Step 4: Commit the failing client tests**

```bash
git add crates/flotilla-client/src/lib.rs
git commit -m "test: cover typed client protocol handling"
```

### Task 8: Implement typed protocol handling in the client

**Files:**
- Modify: `crates/flotilla-client/src/lib.rs`
- Test: `crates/flotilla-client/src/lib.rs`

- [ ] **Step 1: Update request sending to accept `Request`**

Change helpers from:

```rust
async fn request(&self, method: &str, params: serde_json::Value) -> Result<RawResponse, String>
```

to:

```rust
async fn request(&self, request: Request) -> Result<ResponseResult, String>
```

- [ ] **Step 2: Centralize response validation**

Add a small client helper that pattern-matches `ResponseResult` and validates the expected `Response` variant per call.

- [ ] **Step 3: Remove client dependence on protocol `RawResponse`**

Delete public imports and tests for `RawResponse::parse` / `parse_empty`.

- [ ] **Step 4: Run client tests to verify they pass**

Run: `cargo test -p flotilla-client --locked`
Expected: PASS

- [ ] **Step 5: Commit the client migration**

```bash
git add crates/flotilla-client/src/lib.rs
git commit -m "refactor: use typed daemon protocol in client"
```

## Chunk 4: Downstream Rename And Verification

### Task 9: Update downstream crates for repo snapshot names and events

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs`
- Modify: `crates/flotilla-core/src/convert.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
- Modify: `crates/flotilla-daemon/tests/socket_roundtrip.rs`
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`

- [ ] **Step 1: Rename imported types and event variants**

Update all compile sites to:

```rust
RepoSnapshot
RepoDelta
DaemonEvent::RepoSnapshot
DaemonEvent::RepoDelta
```

- [ ] **Step 2: Preserve existing behavior while renaming**

Do not alter event sequencing, replay behavior, or snapshot contents during these edits. This task is for terminology and typed protocol fallout only.

- [ ] **Step 3: Run targeted package tests**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon
cargo test -p flotilla-daemon --locked
cargo test -p flotilla-tui --locked
```

Expected: PASS

- [ ] **Step 4: Commit the downstream rename**

```bash
git add crates/flotilla-core/src/daemon.rs crates/flotilla-core/src/convert.rs crates/flotilla-core/src/in_process.rs crates/flotilla-core/tests/in_process_daemon.rs crates/flotilla-daemon/tests/socket_roundtrip.rs crates/flotilla-daemon/tests/multi_host.rs crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/test_support.rs crates/flotilla-tui/src/cli.rs
git commit -m "refactor: propagate repo snapshot protocol rename"
```

### Task 10: Run full verification and clean up obsolete code paths

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-client/src/lib.rs`
- Test: workspace

- [ ] **Step 1: Remove any dead compatibility code left behind**

Delete unused imports, stale comments about `method` and `params`, and any leftover helper functions or tests for removed raw JSON protocol paths.

- [ ] **Step 2: Run formatting**

Run:

```bash
cargo +nightly-2026-03-12 fmt
```

Expected: formatting completes without diffs afterward.

- [ ] **Step 3: Run final verification**

Run:

```bash
cargo test --workspace --locked
```

If sandbox restrictions block socket tests, run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: PASS

- [ ] **Step 4: Commit the cleanup and verification pass**

```bash
git add crates/flotilla-protocol/src/lib.rs crates/flotilla-daemon/src/server.rs crates/flotilla-client/src/lib.rs
git commit -m "chore: remove obsolete raw protocol paths"
```
