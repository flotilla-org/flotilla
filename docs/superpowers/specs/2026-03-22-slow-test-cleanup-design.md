# Slow Test Cleanup: FakeVcs and Async resolve_repo_root

**Date:** 2026-03-22
**Branch:** `slow-test-cleanup`

## Problem

The `in_process_daemon.rs` test suite has ~24 tests that spawn real git processes (`git init`, `git config`, `git add`, `git commit`, `git remote add`) to set up repositories, then use `git_process_discovery(false)` which gives them a real `ProcessCommandRunner`. These tests verify daemon-level concerns (snapshots, events, correlation, host attribution) — not git integration. The git repo is scaffolding.

Additionally, `Vcs::resolve_repo_root()` is synchronous and bypasses the `CommandRunner` trait by calling `std::process::Command` directly. This means it cannot be tested via replay, traced, or mocked. The same logic is duplicated in `InProcessDaemon::normalize_repo_path()`, which is async and uses the runner — but is missing bare-repo detection.

## Scope

1. Make `Vcs::resolve_repo_root()` async, route through `CommandRunner`
2. Build `FakeVcs` + `FakeCheckoutManager` with shared mutable state
3. Convert `daemon_for_git_repo()` and related tests to use the fakes

Out of scope: `find_repo_root()` in `main.rs` (CLI settings paths), multi_host/peer/socket tests, SSH transport abstraction.

## Design

### 1. Async `resolve_repo_root`

**Trait change** in `crates/flotilla-core/src/providers/vcs/mod.rs`:

```rust
#[async_trait]
pub trait Vcs: Send + Sync {
    async fn resolve_repo_root(&self, path: &Path) -> Option<PathBuf>;
    // ... rest unchanged
}
```

**`GitVcs` implementation** in `crates/flotilla-core/src/providers/vcs/git.rs`:

Replace the two `std::process::Command::new("git")` calls with the `run!` macro (consistent with the rest of the codebase):

```rust
async fn resolve_repo_root(&self, path: &Path) -> Option<PathBuf> {
    let output = run!(self.runner, "git", &["rev-parse", "--path-format=absolute", "--git-common-dir"], path).ok()?;
    let git_dir = PathBuf::from(output.trim());

    let is_bare = run!(self.runner, "git", &["rev-parse", "--is-bare-repository"], path)
        .ok()
        .map(|s| s.trim() == "true")
        .unwrap_or(false);

    if is_bare {
        Some(git_dir)
    } else {
        git_dir.parent().map(|p| p.to_path_buf())
    }
}
```

**`InProcessDaemon::normalize_repo_path`** in `crates/flotilla-core/src/in_process.rs`:

Delete the hand-rolled git logic. Delegate to `GitVcs::resolve_repo_root`, preserving the existing canonicalization logic (needed for macOS `/var` → `/private/var` symlinks):

```rust
async fn normalize_repo_path(&self, path: &Path) -> (PathBuf, Option<PathBuf>) {
    let vcs = GitVcs::new(self.discovery.runner.clone());
    match vcs.resolve_repo_root(path).await {
        Some(repo_root) => {
            // Canonicalize to handle symlinks (e.g. /var -> /private/var on macOS).
            let canonical_root = std::fs::canonicalize(&repo_root).unwrap_or(repo_root);
            let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if canonical_root != canonical_path {
                (canonical_root, Some(path.to_path_buf()))
            } else {
                (canonical_root, None)
            }
        }
        None => (path.to_path_buf(), None),
    }
}
```

Note: `normalize_repo_path` hardcodes `GitVcs` — this is the same assumption the current code makes. When jj support lands, this should delegate to whatever VCS was discovered for the path. For now, the `FakeVcs` tests work because `normalize_repo_path` is only called from `add_repo()`, and the daemon tests that use `FakeVcs` pass paths via `InProcessDaemon::new()` (which does not call `normalize_repo_path`). Tests that call `add_repo` directly will need the `DiscoveryMockRunner` to include canned responses for `git rev-parse`, or `normalize_repo_path` will fall through to the `None` branch and return the path unchanged.

This also fixes the missing bare-repo handling that exists in the current `normalize_repo_path`.

**`config.rs::resolve_repo_roots()`**: Make `async fn`. The single callsite in `main.rs::run_tui()` is already inside a tokio runtime — just add `.await`.

**Stub/Mock impls** (`StubVcs` in model.rs, `MockVcs` in refresh.rs): Add `async`, still return `None`.

**`wt.rs`**: Only implements `CheckoutManager`, not `Vcs` — no changes needed.

### 2. `FakeVcs` + `FakeCheckoutManager`

Lives in test_support, behind the `test-support` feature flag.

**Relationship to existing `FakeCheckoutManager`:** There is already a `FakeCheckoutManager` in test_support that uses `Arc<TokioMutex<Vec<(PathBuf, Checkout)>>>`. The new design replaces it with a unified `FakeVcsState` that holds both VCS and checkout state. Existing callers of the old `FakeCheckoutManager` (via `fake_discovery_with_providers`) will be migrated to use the new unified state. This avoids having two competing patterns for the same concept.

**Shared state:**

```rust
pub struct FakeVcsState {
    pub root: PathBuf,
    pub branches: Vec<BranchInfo>,
    pub remote_branches: Vec<String>,
    pub checkouts: Vec<(PathBuf, Checkout)>,
    pub commit_log: Vec<CommitInfo>,
}
```

Wrapped in `Arc<RwLock<FakeVcsState>>`. Both `FakeVcs` and `FakeCheckoutManager` hold a clone of the same arc, so tests can mutate state between refreshes.

**Builder pattern:**

```rust
let state = FakeVcsState::builder()
    .root(&repo_path)
    .branch("main", true)
    .branch("feature/foo", false)
    .remote_branch("main")
    .checkout(&repo_path, "main", |c| c.is_main(true))
    .build();  // returns Arc<RwLock<FakeVcsState>>
```

**`FakeVcs` implements `Vcs`:**

| Method | Behavior |
|--------|----------|
| `resolve_repo_root(path)` | Returns `Some(state.root)` if path starts with root, else `None` |
| `list_local_branches(root)` | Returns `state.branches.clone()` |
| `list_remote_branches(root)` | Returns `state.remote_branches.clone()` |
| `commit_log(root, branch, limit)` | Returns `state.commit_log[..limit].to_vec()` |
| `ahead_behind(root, branch, ref)` | Returns `Ok(AheadBehind { ahead: 0, behind: 0 })` |
| `working_tree_status(root, path)` | Returns `Ok(WorkingTreeStatus { staged: 0, modified: 0, untracked: 0 })` |

**`FakeCheckoutManager` implements `CheckoutManager`:**

| Method | Behavior |
|--------|----------|
| `list_checkouts(root)` | Returns `state.checkouts.clone()` |
| `create_checkout(root, branch, create)` | Adds to `state.checkouts`, returns new entry |
| `remove_checkout(root, branch)` | Removes from `state.checkouts` |

**Discovery wiring — separate `FakeVcsFactory` and `FakeCheckoutManagerFactory`:**

The `Vcs` and `CheckoutManager` factory traits have separate `Output` types, so two factories are needed. Each holds a clone of the shared `Arc<RwLock<FakeVcsState>>` and always succeeds its probe.

```rust
pub fn fake_vcs_discovery(state: Arc<RwLock<FakeVcsState>>) -> DiscoveryRuntime {
    let mut runtime = fake_discovery(false);
    // FactoryRegistry fields are pub — push directly (no register_vcs method exists)
    runtime.factories.vcs.push(Box::new(FakeVcsFactory::new(state.clone())));
    runtime.factories.checkout_managers.push(Box::new(FakeCheckoutManagerFactory::new(state)));
    runtime
}
```

Note: `fake_discovery(false)` uses `FactoryRegistry::default_all()` which pre-populates `vcs` with `GitVcsFactory`. Since `GitVcsFactory` probes for a git checkout assertion in the environment (which fake discovery doesn't provide), its probe fails and the `FakeVcsFactory` succeeds. This ordering is safe.

### 3. Test Migration

**New helpers** in `in_process_daemon.rs`:

```rust
async fn daemon_for_fake_repo() -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>, RepoIdentity) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let state = FakeVcsState::builder()
        .root(repo.clone())
        .branch("main", true)
        .remote_branch("main")
        .checkout(&repo, "main", |c| c.is_main(true))
        .build();

    let mut discovery = fake_vcs_discovery(state);
    discovery.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;
    let identity = daemon.tracked_repo_identity_for_path(&repo).await.expect("identity");
    (temp, repo, daemon, identity)
}
```

**Multi-root variant** for `daemon_for_duplicate_fake_repos()`: `FakeVcsState` supports a single root. For multi-root tests, create two `FakeVcsState` instances (one per root path) and register two `FakeVcsFactory` instances. Both resolve to the same identity via `FixedRemoteHostDetector`.

```rust
async fn daemon_for_duplicate_fake_repos() -> (tempfile::TempDir, PathBuf, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    std::fs::create_dir_all(&repo_a).expect("create repo-a dir");
    std::fs::create_dir_all(&repo_b).expect("create repo-b dir");

    let state_a = FakeVcsState::builder()
        .root(repo_a.clone())
        .branch("main", true)
        .checkout(&repo_a, "main", |c| c.is_main(true))
        .build();
    let state_b = FakeVcsState::builder()
        .root(repo_b.clone())
        .branch("main", true)
        .checkout(&repo_b, "main", |c| c.is_main(true))
        .build();

    let mut discovery = fake_discovery(false);
    discovery.factories.vcs.push(Box::new(FakeVcsFactory::new(state_a.clone())));
    discovery.factories.vcs.push(Box::new(FakeVcsFactory::new(state_b.clone())));
    discovery.factories.checkout_managers.push(Box::new(FakeCheckoutManagerFactory::new(state_a)));
    discovery.factories.checkout_managers.push(Box::new(FakeCheckoutManagerFactory::new(state_b)));
    discovery.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo_a.clone(), repo_b.clone()], config, discovery, HostName::local()).await;
    (temp, repo_a, repo_b, daemon)
}
```

**Migration:** Replace `daemon_for_git_repo()` → `daemon_for_fake_repo()` and `daemon_for_duplicate_git_repos()` → `daemon_for_duplicate_fake_repos()` across all tests. Most tests need zero assertion changes.

**Additional callers to migrate:** Two tests (`add_and_remove_repo_updates_state_and_emits_events` at line ~1198 and `adding_local_clone_promotes_remote_only_identity_to_local_execution` at line ~1304) call `init_git_repo_with_local_bare_remote` directly without using the `daemon_for_git_repo` helper. These must also be migrated before the helper can be removed.

**Cleanup:** Remove `init_git_repo_with_local_bare_remote()`, `init_bare_git_remote()`, `init_git_repo_with_remote()`, and `local_bare_remote_discovery()` once all callers are migrated. Keep `git_process_discovery()` — it's still used by multi_host and peer tests.

## Files Changed

| File | Change |
|------|--------|
| `crates/flotilla-core/src/providers/vcs/mod.rs` | `resolve_repo_root` → `async fn` |
| `crates/flotilla-core/src/providers/vcs/git.rs` | Use `run!` macro instead of `std::process::Command` |
| `crates/flotilla-core/src/in_process.rs` | `normalize_repo_path` delegates to `GitVcs::resolve_repo_root`, preserving canonicalization |
| `crates/flotilla-core/src/config.rs` | `resolve_repo_roots` → `async fn` |
| `crates/flotilla-core/src/model.rs` | `StubVcs::resolve_repo_root` → async |
| `crates/flotilla-core/src/refresh.rs` | `MockVcs::resolve_repo_root` → async |
| `crates/flotilla-core/src/providers/discovery/test_support.rs` | Add `FakeVcs`, `FakeCheckoutManager`, `FakeVcsFactory`, `FakeCheckoutManagerFactory`, `FakeVcsState`, builder, `fake_vcs_discovery()`; replace existing `FakeCheckoutManager` |
| `crates/flotilla-core/tests/in_process_daemon.rs` | Replace `daemon_for_git_repo` helpers, migrate ~24 tests including standalone `init_git_repo_with_local_bare_remote` callers |
| `src/main.rs` | `.await` on `resolve_repo_roots()` |

## What Stays Unchanged

- `multi_host.rs` tests (6 real-git tests — future work)
- `peer_connect_flow.rs` tests (2 tests)
- `socket_roundtrip.rs` tests (3 tests)
- Provider-level record/replay tests
- `find_repo_root()` in `main.rs` (CLI settings paths)
- `wt.rs` (only implements `CheckoutManager`, not `Vcs`)
- SSH transport in `flotilla-daemon`
