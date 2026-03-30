# Repo Identity Path Demotion Plan A

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `RepoIdentity` authoritative for remote routing and repo tracking while demoting repo-level paths to optional metadata and preserving explicit checkout paths only where execution really needs them.

**Architecture:** This plan migrates the system in three bounded stages. First, remote step routing stops carrying repo root paths when identity already determines execution. Second, peer replication stops treating host-local repo roots as required repo identity data and carries them only as optional host metadata. Third, repo-facing protocol and UI/client surfaces become identity-first with optional path metadata, creating a stable checkpoint before deleting synthetic remote paths in a later Plan B.

**Tech Stack:** Rust, serde, Tokio, cargo fmt, clippy, cargo test

**Notes:**
- This plan intentionally stops before removing synthetic remote paths or path-first request APIs. Those become simpler follow-on cleanup once the protocol boundary is identity-first.
- Do **not** remove checkout-scoped `checkout_path` fields in command and step payloads. Those are real execution inputs.
- Preserve knowledge of real host filesystem roots when a host actually has one checked out, but represent that as optional metadata rather than as the repo key.
- Use the sandbox-safe workspace test command from `AGENTS.md` if full workspace tests need socket-bind skips.

**Spec:** Builds on `docs/superpowers/specs/2026-03-14-repo-identity-and-command-affinity-design.md`

---

## File Map

- Modify: `crates/flotilla-core/src/step.rs`
- Modify: `crates/flotilla-daemon/src/server/remote_commands.rs`
- Modify: `crates/flotilla-daemon/src/server/peer_runtime.rs`
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-protocol/src/peer.rs`
- Modify: `crates/flotilla-protocol/src/snapshot.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-core/src/convert.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-client/src/lib.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`
- Modify: `crates/flotilla-daemon/src/server/request_dispatch.rs` only if request/response shapes change inside Plan A
- Modify tests in:
  - `crates/flotilla-daemon/src/server/tests.rs`
  - `crates/flotilla-daemon/src/peer/manager/tests.rs`
  - `crates/flotilla-daemon/tests/multi_host.rs`
  - `crates/flotilla-client/src/lib/tests.rs`
  - `crates/flotilla-core/tests/in_process_daemon.rs`
  - `crates/flotilla-protocol/src/lib/tests.rs`
  - `crates/flotilla-protocol/src/peer.rs` tests
  - `crates/flotilla-protocol/src/snapshot.rs` tests
  - `crates/flotilla-tui/src/app/tests.rs` and related CLI/TUI tests

## Invariants

- Remote execution must resolve repo roots from `RepoIdentity`, never from a path sent over the wire.
- Checkout-specific execution must continue to use explicit checkout paths.
- Peer overlay merge and replay bookkeeping remain keyed by `RepoIdentity`.
- Host-local repo roots remain observable as metadata when available, but remote-only repos are not required to fake one within this plan.
- Plan A ends when protocol consumers can index repos by identity without treating repo path as required identity data.

## Chunk 1: Remove Needless `repo_path` From Remote Step Routing

### Task 1: Prove remote step execution already uses identity, not routed repo path

**Files:**
- Inspect: `crates/flotilla-core/src/in_process.rs`
- Inspect: `crates/flotilla-core/src/step.rs`
- Inspect: `crates/flotilla-daemon/src/server/remote_commands.rs`
- Inspect: `crates/flotilla-daemon/src/server/peer_runtime.rs`
- Inspect: `crates/flotilla-protocol/src/peer.rs`

- [ ] **Step 1: Record the identity-first execution point**

Run: `rg -n "preferred_local_path_for_identity\\(|repo not tracked locally" crates/flotilla-core/src/in_process.rs`

Expected: `execute_remote_step_batch()` resolves the local repo from `request.repo_identity`

- [ ] **Step 2: Record the redundant routed path plumbing**

Run: `rg -n "RemoteStepRequest|repo_path: request\\.repo|repo_path," crates/flotilla-protocol/src/peer.rs crates/flotilla-daemon/src/server/remote_commands.rs crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server/peer_runtime.rs`

Expected: the routed message and forwarding code still carry `repo_path`

- [ ] **Step 3: Capture the requester-side use of repo path**

Run: `rg -n "RemoteStepBatchRequest|repo: ExecutionEnvironmentPath|EventForwardingProgressSink" crates/flotilla-core/src/step.rs`

Expected: requester-side progress/event reporting uses the repo path, but remote execution does not

### Task 2: Remove repo-root path from remote step wire messages while preserving requester-side event context

**Files:**
- Modify: `crates/flotilla-protocol/src/peer.rs`
- Modify: `crates/flotilla-core/src/step.rs`
- Modify: `crates/flotilla-daemon/src/server/remote_commands.rs`
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/server/peer_runtime.rs`
- Test: `crates/flotilla-daemon/src/server/tests.rs`
- Test: `crates/flotilla-daemon/src/peer/manager/tests.rs`

- [ ] **Step 1: Write failing protocol/manager tests**

Add tests that assert:
- `RoutedPeerMessage::RemoteStepRequest` roundtrips without `repo_path`
- inbound remote-step handling still surfaces `repo_identity`, `step_offset`, and steps correctly

- [ ] **Step 2: Change the protocol shape**

Implementation checklist:
- remove `repo_path` from `RoutedPeerMessage::RemoteStepRequest`
- remove `repo_path` from any related pattern matches and constructors
- keep `repo_identity` unchanged

- [ ] **Step 3: Keep requester-side path only in `RemoteStepBatchRequest` if needed for local event emission**

Implementation checklist:
- if `RemoteStepBatchRequest.repo` is used only for requester-side `CommandStepUpdate` events, keep it as local state
- do not reintroduce it into routed wire messages
- document that this field is requester-local event context, not a remote execution input

- [ ] **Step 4: Update routing code**

Implementation checklist:
- `RemoteCommandRouter::execute_batch()` should send a `RemoteStepRequest` without repo path
- `PeerManager::handle_routed()` should stop expecting or forwarding repo path for remote-step requests
- `PeerRuntime` should build `RemoteStepBatchRequest` on the receiving side from identity plus the steps, not from a transmitted repo path

- [ ] **Step 5: Run targeted tests**

Run: `cargo test -p flotilla-daemon --locked remote_step -- --nocapture`

Expected: PASS for remote step routing and forwarding coverage

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/peer.rs crates/flotilla-core/src/step.rs crates/flotilla-daemon/src/server/remote_commands.rs crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server/peer_runtime.rs crates/flotilla-daemon/src/server/tests.rs crates/flotilla-daemon/src/peer/manager/tests.rs
git commit -m "refactor: remove repo path from remote step routing"
```

## Chunk 2: Make Peer Repo Roots Optional Metadata Instead Of Required Identity Data

### Task 3: Introduce optional host repo root metadata for peer snapshots and resync

**Files:**
- Modify: `crates/flotilla-protocol/src/peer.rs`
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/server/peer_runtime.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Test: `crates/flotilla-daemon/tests/multi_host.rs`
- Test: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Write failing tests for remote-only peer data without a mandatory repo path**

Add tests that cover:
- peer snapshot application when repo identity is known but host repo root metadata is absent
- peer snapshot application when optional host repo root metadata is present
- resync snapshot roundtrip with optional host repo root metadata

- [ ] **Step 2: Replace required `repo_path` on peer snapshot/resync messages**

Implementation options:
- rename to `host_repo_root: Option<PathBuf>`, or
- add a new optional field and deprecate the old one within the same PR

Recommendation:
- use a new optional field immediately because this codebase is in a no-backwards-compatibility phase

- [ ] **Step 3: Update peer state storage**

Implementation checklist:
- change `PerRepoPeerState.repo_path` to optional host-root metadata
- rename the field so future readers do not mistake it for the repo key
- ensure overlay merge remains keyed only by `RepoIdentity`

- [ ] **Step 4: Update outbound local-state replication**

Implementation checklist:
- when a daemon sends local repo state to peers, include its local repo root as optional host metadata
- when a repo is remote-only on the receiving daemon, do not invent a local path at this layer

- [ ] **Step 5: Update resync handling**

Implementation checklist:
- `ResyncSnapshot` should carry optional host-root metadata instead of mandatory repo path
- requesters should continue to merge by identity
- if host-root metadata is absent, state application must still succeed

- [ ] **Step 6: Preserve real host root knowledge only for display/debug**

Implementation checklist:
- treat the optional host root as descriptive metadata
- do not use it for routing or execution lookup
- update comments in `peer_runtime.rs`, `peer.rs`, and `peer/manager.rs` to make that boundary explicit

- [ ] **Step 7: Run targeted multi-host tests**

Run: `cargo test -p flotilla-daemon --locked multi_host -- --nocapture`

Expected: PASS for peer overlay rebuild, resync, and remote-only repo scenarios

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-protocol/src/peer.rs crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server/peer_runtime.rs crates/flotilla-core/src/in_process.rs crates/flotilla-daemon/tests/multi_host.rs crates/flotilla-daemon/src/server/tests.rs
git commit -m "refactor: make peer repo roots optional metadata"
```

## Chunk 3: Make Repo-Facing Protocol And Consumers Identity-First With Optional Path Metadata

### Task 4: Convert repo-bearing protocol types to identity-first optional-path forms

**Files:**
- Modify: `crates/flotilla-protocol/src/snapshot.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-core/src/convert.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Test: `crates/flotilla-protocol/src/snapshot.rs`
- Test: `crates/flotilla-protocol/src/lib/tests.rs`

- [ ] **Step 1: Write failing protocol roundtrip tests**

Add tests that prove:
- `RepoInfo` can represent a repo with `identity` and no required path
- `RepoSnapshot` and `RepoDelta` can represent identity-first repos with optional path metadata
- `DaemonEvent::RepoTracked`, `RepoUntracked`, `CommandStarted`, `CommandFinished`, and `CommandStepUpdate` roundtrip without requiring a repo path

- [ ] **Step 2: Change protocol structs**

Implementation checklist:
- change `RepoInfo.path` to `Option<PathBuf>` or introduce a clearly named optional display/root field
- change `RepoSnapshot.repo` and `RepoDelta.repo` similarly
- change command lifecycle events to carry optional repo path metadata rather than a required path

- [ ] **Step 3: Update protocol producers**

Implementation checklist:
- `snapshot_to_proto()` should populate optional path metadata when the daemon has a preferred path
- command events emitted from `InProcessDaemon` should pass optional path metadata
- remote command forwarding should stop inventing fallback identities from path strings

- [ ] **Step 4: Keep local-only request APIs working without broadening scope**

Implementation checklist:
- do not redesign `Request::GetState { repo: PathBuf }` in Plan A
- instead, adapt the daemon internals so request/response boundaries can still produce optional path metadata while indexing by identity internally
- note the remaining request-path cleanup as Plan B scope

- [ ] **Step 5: Run protocol and daemon tests**

Run: `cargo test -p flotilla-protocol --locked`

Expected: PASS

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`

Expected: PASS

### Task 5: Update client and TUI consumers so path is presentation-only

**Files:**
- Modify: `crates/flotilla-client/src/lib.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`
- Modify: TUI/client tests under `crates/flotilla-client/src/lib/tests.rs`, `crates/flotilla-tui/src/app/tests.rs`, and related files

- [ ] **Step 1: Write failing client/TUI tests**

Add tests that prove:
- replay bookkeeping continues to key by `RepoIdentity`
- command tracking remains keyed by `RepoIdentity` even when event path metadata is absent
- CLI output still renders sensibly when command or snapshot events omit a repo path

- [ ] **Step 2: Update client event handling**

Implementation checklist:
- preserve identity-keyed replay logic
- stop assuming repo path is always present in repo events
- maintain compatibility with older local request flows that still provide a path

- [ ] **Step 3: Update TUI in-flight command and repo state handling**

Implementation checklist:
- keep `RepoIdentity` as the indexing key
- treat path as optional display data
- if `InFlightCommand.repo` must remain for rendering, change it to `Option<PathBuf>`

- [ ] **Step 4: Update CLI rendering**

Implementation checklist:
- render command/snapshot output using the repo path when present
- fall back to `RepoIdentity` text when path metadata is absent
- do not invent fake local paths in the UI

- [ ] **Step 5: Run client and TUI tests**

Run: `cargo test -p flotilla-client --locked`

Expected: PASS

Run: `cargo test -p flotilla-tui --locked`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/snapshot.rs crates/flotilla-protocol/src/lib.rs crates/flotilla-core/src/convert.rs crates/flotilla-core/src/in_process.rs crates/flotilla-client/src/lib.rs crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/cli.rs crates/flotilla-protocol/src/lib/tests.rs crates/flotilla-protocol/src/snapshot.rs crates/flotilla-client/src/lib/tests.rs crates/flotilla-tui/src/app/tests.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "refactor: make repo protocol identity-first"
```

## Chunk 4: Verification And Handoff

### Task 6: Verify the Plan A checkpoint

**Files:**
- Verify only

- [ ] **Step 1: Run formatting**

Run: `cargo +nightly-2026-03-12 fmt --check`

Expected: PASS

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Expected: PASS

- [ ] **Step 3: Run sandbox-safe full tests if working in Codex sandbox**

Run: `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`

Expected: PASS

- [ ] **Step 4: If not in sandbox, run the exact CI test command**

Run: `cargo test --workspace --locked`

Expected: PASS

- [ ] **Step 5: Summarize the Plan A stopping point**

Include:
- remote step routing no longer carries needless repo root paths
- peer replication treats host repo roots as optional metadata
- repo-bearing protocol and consumers are identity-first with optional path metadata
- synthetic remote paths and path-first request APIs remain for Plan B

## Plan B Preview

Do **not** start these tasks in this plan. They are the follow-on after the Plan A checkpoint is stable.

- remove synthetic remote repo paths and `add_virtual_repo()` path fakery
- shrink `path_identities` to local-path lookup only
- change path-first request APIs (`GetState`, `Refresh`, add/remove helpers where appropriate) to identity-aware selectors
- remove any remaining fallback code that reconstructs identity from path strings
