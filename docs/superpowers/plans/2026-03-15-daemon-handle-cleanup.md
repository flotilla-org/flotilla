# DaemonHandle Cleanup Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clean up the DaemonHandle trait by removing duplicated methods, renaming misleading command variants, and unifying repo addressing to use RepoSelector consistently.

**Architecture:** The DaemonHandle trait loses three methods (add_repo, remove_repo, refresh) whose functionality already exists as CommandAction variants. Command/result/event variants are renamed for clarity (AddRepo→TrackRepoPath, RemoveRepo→UntrackRepo). Raw PathBuf repo references in commands and query methods are replaced with RepoSelector.

**Tech Stack:** Rust, flotilla-protocol (serde types), flotilla-core (DaemonHandle trait, InProcessDaemon), flotilla-client (SocketDaemon), flotilla-daemon (server dispatch), flotilla-tui (UI callers)

**Spec:** `docs/superpowers/specs/2026-03-15-daemon-handle-cleanup-design.md`

---

## Chunk 1: Protocol renames

### Task 1: Rename CommandAction::AddRepo → TrackRepoPath and RemoveRepo → UntrackRepo

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs` — enum variants, serde renames, description(), tests
- Modify: `crates/flotilla-core/src/executor.rs` — match arms
- Modify: `crates/flotilla-core/src/in_process.rs` — execute() match arms
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` — test command construction
- Modify: `crates/flotilla-daemon/src/server.rs` — extract_command_identity match
- Modify: `crates/flotilla-tui/src/app/file_picker.rs` — AddRepo construction
- Modify: `src/main.rs` — CLI command parsing

- [ ] **Step 1: Rename AddRepo → TrackRepoPath in protocol**

In `crates/flotilla-protocol/src/commands.rs`:
- Rename variant `AddRepo { path: PathBuf }` to `TrackRepoPath { path: PathBuf }`
- Update `#[serde(rename_all = "snake_case")]` will auto-generate `track_repo_path` — verify this is acceptable or add explicit `#[serde(rename = "track_repo_path")]`
- Update `description()` match arm from `CommandAction::AddRepo` to `CommandAction::TrackRepoPath`
- Update all test cases referencing `CommandAction::AddRepo`

- [ ] **Step 2: Rename RemoveRepo → UntrackRepo in protocol**

In `crates/flotilla-protocol/src/commands.rs`:
- Rename variant `RemoveRepo { repo: RepoSelector }` to `UntrackRepo { repo: RepoSelector }`
- Update `description()` match arm
- Update all test cases

- [ ] **Step 3: Update all callers of AddRepo/RemoveRepo across the codebase**

Callers to update (find with `cargo build` errors):
- `src/main.rs:367` — CLI `repo add` command: `CommandAction::AddRepo` → `CommandAction::TrackRepoPath`
- `src/main.rs:372` — CLI `repo remove` command: `CommandAction::RemoveRepo` → `CommandAction::UntrackRepo`
- `src/main.rs:558` — CLI test assert
- `crates/flotilla-core/src/executor.rs:691-692` — daemon-level command match arms
- `crates/flotilla-core/src/executor.rs:2456-2457` — executor tests
- `crates/flotilla-core/src/in_process.rs:1545,1568` — InProcessDaemon execute match arms
- `crates/flotilla-core/tests/in_process_daemon.rs:681,720` — test command construction
- `crates/flotilla-daemon/src/server.rs:203` — extract_command_identity match
- `crates/flotilla-tui/src/app/file_picker.rs:76,332` — file picker command construction and test
- `crates/flotilla-tui/src/app/key_handlers.rs:888,892` — key handler test matching on AddRepo

- [ ] **Step 4: Run tests**

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "refactor: rename AddRepo/RemoveRepo to TrackRepoPath/UntrackRepo"
```

### Task 2: Rename CommandResult::RepoAdded → RepoTracked and RepoRemoved → RepoUntracked

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs` — result variants, add resolved_from field
- Modify: `crates/flotilla-core/src/in_process.rs` — result construction
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` — result assertions
- Modify: `crates/flotilla-tui/src/cli.rs` — result formatting
- Modify: `crates/flotilla-tui/src/app/executor.rs` — result handling
- Modify: `crates/flotilla-protocol/src/peer.rs` — peer test using RepoAdded

- [ ] **Step 1: Rename result variants in protocol**

In `crates/flotilla-protocol/src/commands.rs`:
- `RepoAdded { path: PathBuf }` → `RepoTracked { path: PathBuf, resolved_from: Option<PathBuf> }`
- `RepoRemoved { path: PathBuf }` → `RepoUntracked { path: PathBuf }`
- Add `#[serde(default)]` on `resolved_from` for backwards-compatible deserialization
- Update roundtrip test cases

- [ ] **Step 2: Update all callers**

Callers to update:
- `crates/flotilla-core/src/in_process.rs:1556` — construct `RepoTracked { path, resolved_from: None }` (normalization comes in Task 6)
- `crates/flotilla-core/src/in_process.rs:1581` — construct `RepoUntracked { path }`
- `crates/flotilla-core/tests/in_process_daemon.rs:707,741` — test result assertions
- `crates/flotilla-tui/src/cli.rs:203-204,855,863` — format output and tests
- `crates/flotilla-tui/src/app/executor.rs:78,81` — result handling
- `crates/flotilla-protocol/src/peer.rs:369,379` — peer message test

- [ ] **Step 3: Run tests**

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "refactor: rename RepoAdded/RepoRemoved to RepoTracked/RepoUntracked"
```

### Task 3: Rename DaemonEvent::RepoAdded → RepoTracked and RepoRemoved → RepoUntracked

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs` — event variants and serde renames
- Modify: `crates/flotilla-core/src/in_process.rs` — event emission
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` — event assertions
- Modify: `crates/flotilla-tui/src/cli.rs` — event formatting
- Modify: `crates/flotilla-tui/src/app/mod.rs` — event handling
- Modify: `crates/flotilla-client/src/lib.rs` — event handler and tests

- [ ] **Step 1: Rename event variants in protocol**

In `crates/flotilla-protocol/src/lib.rs`:
- `RepoAdded(Box<RepoInfo>)` → `RepoTracked(Box<RepoInfo>)`, update serde rename to `"repo_tracked"`
- `RepoRemoved { repo_identity, path }` → `RepoUntracked { repo_identity, path }`, update serde rename to `"repo_untracked"`

- [ ] **Step 2: Update all callers**

Callers to update:
- `crates/flotilla-core/src/in_process.rs:1377,1885` — emit `DaemonEvent::RepoTracked`
- `crates/flotilla-core/src/in_process.rs:2016` — emit `DaemonEvent::RepoUntracked`
- `crates/flotilla-core/src/in_process.rs:1321` — doc comment reference
- `crates/flotilla-core/tests/in_process_daemon.rs:695,730,1210` — event match assertions
- `crates/flotilla-tui/src/cli.rs:231,234,753,770` — format event and tests
- `crates/flotilla-tui/src/app/mod.rs:386,387` — TUI event handler
- `crates/flotilla-client/src/lib.rs:477,482,1039,1042,1165,1178` — SocketDaemon event handler and tests

- [ ] **Step 3: Run tests**

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "refactor: rename DaemonEvent RepoAdded/RepoRemoved to RepoTracked/RepoUntracked"
```

## Chunk 2: Repo addressing fixes

### Task 4: Change issue commands from PathBuf to RepoSelector

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs` — four variant definitions and tests
- Modify: `crates/flotilla-core/src/in_process.rs` — execute() handlers that receive these commands
- Modify: `crates/flotilla-core/src/executor.rs` — executor match arms
- Modify: `crates/flotilla-tui/src/app/mod.rs` — SetIssueViewport and ClearIssueSearch construction
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` — SearchIssues construction
- Modify: `crates/flotilla-tui/src/app/navigation.rs` — FetchMoreIssues construction
- Modify: `crates/flotilla-tui/src/app/executor.rs` — background command classification

- [ ] **Step 1: Change variant definitions in protocol**

In `crates/flotilla-protocol/src/commands.rs`, change:
```rust
SetIssueViewport { repo: RepoSelector, visible_count: usize },
FetchMoreIssues { repo: RepoSelector, desired_count: usize },
SearchIssues { repo: RepoSelector, query: String },
ClearIssueSearch { repo: RepoSelector },
```

Update all test cases to use `RepoSelector::Path(PathBuf::from(...))` where they currently use raw `PathBuf`.

- [ ] **Step 2: Update InProcessDaemon execute handlers**

In `crates/flotilla-core/src/in_process.rs`, the handlers for these four commands extract the repo path. They now receive a `RepoSelector` and need to resolve it to a path first. Add a `resolve_repo_path` call (same pattern used for RemoveRepo/Refresh already).

Update at lines ~1512-1527.

- [ ] **Step 3: Update executor match arms**

In `crates/flotilla-core/src/executor.rs`, the daemon-level command check at lines ~694-697 matches these variants. Update the pattern match for the new field type. Also update the test assertions at lines ~2459-2462 which construct these commands with raw `PathBuf`.

- [ ] **Step 4: Update TUI callers**

Callers that construct these commands with a `PathBuf` need to wrap in `RepoSelector::Path(...)`:
- `crates/flotilla-tui/src/app/mod.rs:539` — `SetIssueViewport { repo: RepoSelector::Path(path), ... }`
- `crates/flotilla-tui/src/app/mod.rs:714` — `ClearIssueSearch { repo: RepoSelector::Path(repo) }`
- `crates/flotilla-tui/src/app/key_handlers.rs:185` — `SearchIssues { repo: RepoSelector::Path(repo), ... }`
- `crates/flotilla-tui/src/app/navigation.rs:100` — `FetchMoreIssues { repo: RepoSelector::Path(repo), ... }`
- Test assertions in `mod.rs:991`, `key_handlers.rs:864,1438`, `navigation.rs:371-376`

- [ ] **Step 5: Run tests**

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "refactor: change issue commands from PathBuf to RepoSelector"
```

### Task 5: Change query method signatures to use RepoSelector

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs` — trait definition
- Modify: `crates/flotilla-core/src/in_process.rs` — InProcessDaemon impl
- Modify: `crates/flotilla-tui/src/app/test_support.rs` — StubDaemon impl
- Modify: `crates/flotilla-client/src/lib.rs` — SocketDaemon impl
- Modify: `crates/flotilla-daemon/src/server.rs` — RPC dispatch
- Modify: `crates/flotilla-tui/src/cli.rs` — CLI callers
- Modify: all test files calling these methods

- [ ] **Step 1: Change trait signatures**

In `crates/flotilla-core/src/daemon.rs`:
```rust
async fn get_state(&self, repo: &RepoSelector) -> Result<Snapshot, String>;
async fn get_repo_detail(&self, repo: &RepoSelector) -> Result<RepoDetailResponse, String>;
async fn get_repo_providers(&self, repo: &RepoSelector) -> Result<RepoProvidersResponse, String>;
async fn get_repo_work(&self, repo: &RepoSelector) -> Result<RepoWorkResponse, String>;
```

Add `use flotilla_protocol::RepoSelector;` to imports.

- [ ] **Step 2: Update InProcessDaemon implementation**

In `crates/flotilla-core/src/in_process.rs`:
- `get_state` currently takes `&Path` and resolves via `path_identities`. Update to accept `&RepoSelector` and resolve via `resolve_repo_selector` (already exists for command dispatch).
- `get_repo_detail`, `get_repo_providers`, `get_repo_work` currently take `&str` slug and resolve via `resolve_slug`. Update to accept `&RepoSelector` and resolve via `resolve_repo_selector`.

- [ ] **Step 3: Update StubDaemon**

In `crates/flotilla-tui/src/app/test_support.rs`, update method signatures to match the new trait.

- [ ] **Step 4: Update SocketDaemon**

In `crates/flotilla-client/src/lib.rs`, update method signatures. The RPC serialization needs to send the `RepoSelector` as the param instead of a raw path or slug string.

- [ ] **Step 5: Update server dispatch**

In `crates/flotilla-daemon/src/server.rs`, the `"get_state"`, `"repo_detail"`, `"repo_providers"`, `"repo_work"` handlers need to deserialize `RepoSelector` from params instead of a raw string/path.

- [ ] **Step 6: Update CLI callers**

In `crates/flotilla-tui/src/cli.rs`:
- `run_repo_detail` calls `daemon.get_repo_detail(slug)` — wrap in `RepoSelector::Query(slug.into())`
- `run_repo_providers` calls `daemon.get_repo_providers(slug)` — same
- `run_repo_work` calls `daemon.get_repo_work(slug)` — same

- [ ] **Step 7: Update test callers**

All tests calling `daemon.get_state(&path)` change to `daemon.get_state(&RepoSelector::Path(path))`. Similarly for get_repo_detail/providers/work. Search all test files in:
- `crates/flotilla-core/tests/in_process_daemon.rs`
- `crates/flotilla-daemon/tests/socket_roundtrip.rs`
- `crates/flotilla-daemon/tests/multi_host.rs`
- `crates/flotilla-daemon/src/server.rs` (inline tests)

- [ ] **Step 8: Run tests**

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

- [ ] **Step 9: Commit**

```bash
git add -A && git commit -m "refactor: change query methods from Path/slug to RepoSelector"
```

## Chunk 3: Trait slimming and normalization

### Task 6: Remove add_repo, remove_repo, refresh from DaemonHandle trait

This is the core change. The implementation logic stays in `InProcessDaemon` as private methods; the trait surface shrinks.

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs` — remove three trait methods
- Modify: `crates/flotilla-core/src/in_process.rs` — keep implementation as private/pub(crate), update execute() to call directly
- Modify: `crates/flotilla-client/src/lib.rs` — remove SocketDaemon trait impls, route through execute()
- Modify: `crates/flotilla-tui/src/app/test_support.rs` — remove StubDaemon impls
- Modify: `crates/flotilla-tui/src/run.rs` — migrate refresh caller
- Modify: `crates/flotilla-daemon/src/server.rs` — remove RPC dispatch arms for "refresh"/"add_repo"/"remove_repo"
- Modify: `src/main.rs` — migrate add_repo caller at startup
- Modify: all test files calling these methods directly

- [ ] **Step 1: Remove methods from DaemonHandle trait**

In `crates/flotilla-core/src/daemon.rs`, delete:
```rust
async fn refresh(&self, repo: &Path) -> Result<(), String>;
async fn add_repo(&self, path: &Path) -> Result<(), String>;
async fn remove_repo(&self, path: &Path) -> Result<(), String>;
```

- [ ] **Step 2: Keep InProcessDaemon implementations as non-trait methods**

In `crates/flotilla-core/src/in_process.rs`:
- The `add_repo`, `remove_repo`, `refresh` methods stay but are no longer part of the trait impl block
- Move them to a separate `impl InProcessDaemon` block (or keep in existing non-trait block)
- The `execute()` method already delegates to these internally — that continues to work
- The server peer-overlay cleanup code at `server.rs:927,1243` calls `daemon.remove_repo()` directly on `InProcessDaemon` (not through the trait) — this continues to work since the method stays on the concrete type

- [ ] **Step 3: Remove SocketDaemon trait impls and route through execute()**

In `crates/flotilla-client/src/lib.rs`:
- Remove the `refresh`, `add_repo`, `remove_repo` methods from the `DaemonHandle` impl block
- These were sending dedicated RPC methods ("refresh", "add_repo", "remove_repo") — no longer needed

- [ ] **Step 4: Remove StubDaemon impls**

In `crates/flotilla-tui/src/app/test_support.rs`, remove the three stub methods.

- [ ] **Step 5: Migrate TUI refresh caller**

In `crates/flotilla-tui/src/run.rs:101`, change from:
```rust
daemon.refresh(&repo).await
```
to:
```rust
daemon.execute(Command {
    host: None,
    context_repo: None,
    action: CommandAction::Refresh { repo: Some(RepoSelector::Path(repo)) },
}).await
```

- [ ] **Step 6: Migrate CLI add_repo caller at startup**

In `src/main.rs:297`, the socket-mode startup calls `daemon.add_repo(&path)`. Change to:
```rust
daemon.execute(Command {
    host: None,
    context_repo: None,
    action: CommandAction::TrackRepoPath { path },
}).await
```

- [ ] **Step 7: Remove server RPC dispatch arms**

In `crates/flotilla-daemon/src/server.rs`, remove the `"refresh"`, `"add_repo"`, `"remove_repo"` RPC dispatch handlers at lines ~1836-1867. These are dead code once SocketDaemon stops sending them.

- [ ] **Step 8: Migrate test callers**

All tests calling `daemon.refresh(&path)`, `daemon.add_repo(&path)`, `daemon.remove_repo(&path)` must switch to `daemon.execute(Command { ... })`. This affects:
- `crates/flotilla-core/tests/in_process_daemon.rs` — ~10 calls (refresh, add_repo, remove_repo). Consider adding a test helper:
  ```rust
  async fn refresh(daemon: &InProcessDaemon, path: &Path) {
      daemon.execute(Command {
          host: None,
          context_repo: None,
          action: CommandAction::Refresh { repo: Some(RepoSelector::Path(path.into())) },
      }).await.expect("refresh");
  }
  ```
- `crates/flotilla-daemon/tests/socket_roundtrip.rs` — `client.refresh()`
- `crates/flotilla-daemon/tests/peer_connect_flow.rs` — `daemon.refresh()`
- `crates/flotilla-daemon/tests/multi_host.rs` — multiple `daemon.refresh()` calls
- `crates/flotilla-daemon/src/server.rs` — inline test setup calls

- [ ] **Step 9: Run tests**

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

- [ ] **Step 10: Commit**

```bash
git add -A && git commit -m "refactor: remove add_repo/remove_repo/refresh from DaemonHandle trait"
```

### Task 7: Add path normalization for TrackRepoPath

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs` — normalize path before tracking
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` — test normalization
- Modify: `crates/flotilla-core/src/providers/vcs/` — may need a utility to find repo root

- [ ] **Step 1: Write test for path normalization**

In `crates/flotilla-core/tests/in_process_daemon.rs`, add a test that:
- Creates a git repo with a worktree
- Calls `TrackRepoPath` with the worktree path
- Asserts the result is `RepoTracked { path: <repo_root>, resolved_from: Some(<worktree_path>) }`
- Asserts the repo is tracked under the root path, not the worktree path

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon -- track_repo_normalizes_worktree_path
```

Expected: FAIL — no normalization logic yet.

- [ ] **Step 3: Implement normalization**

In `crates/flotilla-core/src/in_process.rs`, in the `add_repo` implementation (now called for `TrackRepoPath`):
- After canonicalizing the path, run `git rev-parse --git-common-dir` in the path to find the actual repo root
- If the result differs from the input path, use the repo root as the tracking path and set `resolved_from`
- Fall back gracefully if git is not available or the path is not a git repo

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon -- track_repo_normalizes_worktree_path
```

- [ ] **Step 5: Run full test suite**

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: normalize worktree paths to repo root in TrackRepoPath"
```

### Task 8: Final formatting and verification

- [ ] **Step 1: Format**

```bash
cargo +nightly-2026-03-12 fmt
```

- [ ] **Step 2: Full CI check**

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

- [ ] **Step 3: Commit any formatting changes**

```bash
git add -A && git commit -m "chore: format"
```
