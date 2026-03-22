# Slow Test Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate ~20 slow tests that spawn real git processes by making `Vcs::resolve_repo_root` async and introducing `FakeVcs`/`FakeCheckoutManager` backed by shared mutable state.

**Architecture:** The `Vcs` trait's `resolve_repo_root` becomes async and routes through `CommandRunner` (not `std::process::Command`). A new `FakeVcsState` struct with `Arc<RwLock<...>>` backs both `FakeVcs` and `FakeCheckoutManager`, injected via factory traits. Daemon tests swap from real git repos to fake providers.

**Tech Stack:** Rust, async-trait, tokio, ratatui (TUI tests)

**Spec:** `docs/superpowers/specs/2026-03-22-slow-test-cleanup-design.md`

---

### Task 1: Make `Vcs::resolve_repo_root` async

Change the trait method signature and update all implementors. This is a mechanical change that touches many files but each change is small.

**Files:**
- Modify: `crates/flotilla-core/src/providers/vcs/mod.rs:14-24` (trait definition)
- Modify: `crates/flotilla-core/src/providers/vcs/git.rs:22-61` (GitVcs impl)
- Modify: `crates/flotilla-core/src/model.rs:144-149` (StubVcs impl)
- Modify: `crates/flotilla-core/src/refresh.rs:548-552` (MockVcs impl)

- [ ] **Step 1: Change the trait signature**

In `crates/flotilla-core/src/providers/vcs/mod.rs`, change line 18 from:

```rust
fn resolve_repo_root(&self, path: &Path) -> Option<PathBuf>;
```

to:

```rust
async fn resolve_repo_root(&self, path: &Path) -> Option<PathBuf>;
```

- [ ] **Step 2: Update GitVcs to use `run!` macro**

In `crates/flotilla-core/src/providers/vcs/git.rs`, replace the `resolve_repo_root` method (lines 24-61) with:

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

Note: The `run!` macro requires being inside an `async fn` and uses `.await` internally. It auto-generates the `ChannelLabel`. The 4-arg form `run!(runner, cmd, args, cwd)` uses `command_channel_label` for automatic labeling.

- [ ] **Step 3: Update StubVcs**

In `crates/flotilla-core/src/model.rs`, change the `resolve_repo_root` method in the `StubVcs` impl (around line 147) from `fn` to `async fn`. The body stays the same (`None`).

- [ ] **Step 4: Update MockVcs**

In `crates/flotilla-core/src/refresh.rs`, change the `resolve_repo_root` method in the `MockVcs` impl (around line 550) from `fn` to `async fn`. The body stays the same (`None`).

- [ ] **Step 5: Check compilation**

Run: `cargo build --workspace --locked 2>&1 | head -50`

Expected: Compilation errors in `config.rs` and `in_process.rs` (these call `resolve_repo_root` without `.await`). All other files should compile. Fix any other implementors the compiler finds.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "refactor: make Vcs::resolve_repo_root async"
```

### Task 2: Update callers of `resolve_repo_root`

Fix the compilation errors from Task 1 by making `config.rs::resolve_repo_roots` async and updating `normalize_repo_path` in `in_process.rs`.

**Files:**
- Modify: `crates/flotilla-core/src/config.rs:508-563` (resolve_repo_roots)
- Modify: `crates/flotilla-core/src/in_process.rs:1317-1352` (normalize_repo_path)
- Modify: `src/main.rs:281` (callsite)

- [ ] **Step 1: Make `resolve_repo_roots` async**

In `crates/flotilla-core/src/config.rs`, change line 510 from:

```rust
pub fn resolve_repo_roots(cli_roots: &[PathBuf], config: &ConfigStore) -> Vec<PathBuf> {
```

to:

```rust
pub async fn resolve_repo_roots(cli_roots: &[PathBuf], config: &ConfigStore) -> Vec<PathBuf> {
```

And change the call on line 549 from:

```rust
if let Some(repo_root) = git.resolve_repo_root(cwd) {
```

to:

```rust
if let Some(repo_root) = git.resolve_repo_root(cwd).await {
```

- [ ] **Step 2: Update main.rs callsite**

In `src/main.rs`, change line 281 from:

```rust
let roots = flotilla_core::config::resolve_repo_roots(&cli.repo_root, &config);
```

to:

```rust
let roots = flotilla_core::config::resolve_repo_roots(&cli.repo_root, &config).await;
```

- [ ] **Step 3: Rewrite `normalize_repo_path` to delegate to `GitVcs`**

In `crates/flotilla-core/src/in_process.rs`, replace the `normalize_repo_path` method (lines 1317-1352) with:

```rust
async fn normalize_repo_path(&self, path: &Path) -> (PathBuf, Option<PathBuf>) {
    use crate::providers::vcs::git::GitVcs;

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

- [ ] **Step 4: Verify full build**

Run: `cargo build --workspace --locked`

Expected: Clean compilation with no errors.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace --locked 2>&1 | tail -30`

Expected: All existing tests still pass. The behavior is unchanged — `GitVcs::resolve_repo_root` runs the same git commands through `CommandRunner` instead of `std::process::Command`, but `ProcessCommandRunner` delegates to real `tokio::process::Command` so the result is identical.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "refactor: update resolve_repo_root callers for async"
```

### Task 3: Build `FakeVcsState` and builder

Create the shared state struct and builder pattern that will back both `FakeVcs` and `FakeCheckoutManager`.

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

- [ ] **Step 1: Add `FakeVcsState` struct and builder**

Add the following to `crates/flotilla-core/src/providers/discovery/test_support.rs`. Place it after the existing `FakeCheckoutManager` (around line 390):

```rust
use std::sync::RwLock;
use crate::providers::types::BranchInfo;
// Checkout, CommitInfo, AheadBehind, WorkingTreeStatus are already imported
// from flotilla_protocol at the top of test_support.rs.
// Add any missing ones to the existing `use flotilla_protocol::{...}` block:
//   AheadBehind, CommitInfo, WorkingTreeStatus

/// Shared mutable state backing FakeVcs and FakeCheckoutManager.
/// Wrapped in Arc<RwLock<...>> so tests can mutate state between refreshes.
pub struct FakeVcsState {
    pub root: PathBuf,
    pub branches: Vec<BranchInfo>,
    pub remote_branches: Vec<String>,
    pub checkouts: Vec<(PathBuf, Checkout)>,
    pub commit_log: Vec<CommitInfo>,
}

pub struct FakeVcsStateBuilder {
    root: PathBuf,
    branches: Vec<BranchInfo>,
    remote_branches: Vec<String>,
    checkouts: Vec<(PathBuf, Checkout)>,
    commit_log: Vec<CommitInfo>,
}

impl FakeVcsState {
    pub fn builder() -> FakeVcsStateBuilder {
        FakeVcsStateBuilder {
            root: PathBuf::new(),
            branches: Vec::new(),
            remote_branches: Vec::new(),
            checkouts: Vec::new(),
            commit_log: Vec::new(),
        }
    }
}

impl FakeVcsStateBuilder {
    pub fn root(mut self, path: impl Into<PathBuf>) -> Self {
        self.root = path.into();
        self
    }

    pub fn branch(mut self, name: &str, is_trunk: bool) -> Self {
        self.branches.push(BranchInfo { name: name.to_string(), is_trunk });
        self
    }

    pub fn remote_branch(mut self, name: &str) -> Self {
        self.remote_branches.push(name.to_string());
        self
    }

    pub fn checkout(mut self, path: impl Into<PathBuf>, branch: &str, configure: impl FnOnce(CheckoutBuilder) -> CheckoutBuilder) -> Self {
        let builder = configure(CheckoutBuilder::new(branch));
        self.checkouts.push((path.into(), builder.build()));
        self
    }

    /// Add a pre-built checkout directly (for tests that need full control
    /// over correlation/association keys).
    pub fn checkout_raw(mut self, path: impl Into<PathBuf>, checkout: Checkout) -> Self {
        self.checkouts.push((path.into(), checkout));
        self
    }

    pub fn build(self) -> Arc<RwLock<FakeVcsState>> {
        Arc::new(RwLock::new(FakeVcsState {
            root: self.root,
            branches: self.branches,
            remote_branches: self.remote_branches,
            checkouts: self.checkouts,
            commit_log: self.commit_log,
        }))
    }
}

pub struct CheckoutBuilder {
    branch: String,
    is_main: bool,
    correlation_keys: Vec<CorrelationKey>,
    association_keys: Vec<crate::providers::correlation::AssociationKey>,
}

impl CheckoutBuilder {
    fn new(branch: &str) -> Self {
        Self {
            branch: branch.to_string(),
            is_main: false,
            correlation_keys: Vec::new(),
            association_keys: Vec::new(),
        }
    }

    pub fn is_main(mut self, val: bool) -> Self {
        self.is_main = val;
        self
    }

    pub fn correlation_key(mut self, key: CorrelationKey) -> Self {
        self.correlation_keys.push(key);
        self
    }

    pub fn association_key(mut self, key: crate::providers::correlation::AssociationKey) -> Self {
        self.association_keys.push(key);
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
            association_keys: self.association_keys,
        }
    }
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo build -p flotilla-core --locked --features test-support`

Expected: Compiles. The new types are defined but not yet used.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: add FakeVcsState struct and builder"
```

### Task 4: Implement `FakeVcs`

Create the `FakeVcs` struct implementing the `Vcs` trait, backed by `FakeVcsState`.

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

- [ ] **Step 1: Add `FakeVcs` implementation**

Add after the `FakeVcsState` code from Task 3:

```rust
pub struct FakeVcs {
    state: Arc<RwLock<FakeVcsState>>,
}

impl FakeVcs {
    pub fn new(state: Arc<RwLock<FakeVcsState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl crate::providers::vcs::Vcs for FakeVcs {
    async fn resolve_repo_root(&self, path: &Path) -> Option<PathBuf> {
        let state = self.state.read().expect("FakeVcsState lock poisoned");
        if path.starts_with(&state.root) || path == state.root {
            Some(state.root.clone())
        } else {
            None
        }
    }

    async fn list_local_branches(&self, _repo_root: &Path) -> Result<Vec<BranchInfo>, String> {
        let state = self.state.read().expect("FakeVcsState lock poisoned");
        Ok(state.branches.clone())
    }

    async fn list_remote_branches(&self, _repo_root: &Path) -> Result<Vec<String>, String> {
        let state = self.state.read().expect("FakeVcsState lock poisoned");
        Ok(state.remote_branches.clone())
    }

    async fn commit_log(&self, _repo_root: &Path, _branch: &str, limit: usize) -> Result<Vec<CommitInfo>, String> {
        let state = self.state.read().expect("FakeVcsState lock poisoned");
        Ok(state.commit_log.iter().take(limit).cloned().collect())
    }

    async fn ahead_behind(&self, _repo_root: &Path, _branch: &str, _reference: &str) -> Result<AheadBehind, String> {
        Ok(AheadBehind { ahead: 0, behind: 0 })
    }

    async fn working_tree_status(&self, _repo_root: &Path, _checkout_path: &Path) -> Result<WorkingTreeStatus, String> {
        Ok(WorkingTreeStatus { staged: 0, modified: 0, untracked: 0 })
    }
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo build -p flotilla-core --locked --features test-support`

Expected: Compiles cleanly.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: add FakeVcs backed by FakeVcsState"
```

### Task 5: Replace existing `FakeCheckoutManager` with state-backed version

Replace the existing `FakeCheckoutManager` in test_support with one backed by `FakeVcsState`. Update its two callers.

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs:343-390`
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` (two callers around lines 2111 and 2210)

- [ ] **Step 1: Replace `FakeCheckoutManager`**

In `crates/flotilla-core/src/providers/discovery/test_support.rs`, replace the existing `FakeCheckoutManager` struct and impl (around lines 343-390) with:

```rust
pub struct FakeCheckoutManager {
    state: Arc<RwLock<FakeVcsState>>,
}

impl FakeCheckoutManager {
    pub fn new(state: Arc<RwLock<FakeVcsState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl crate::providers::vcs::CheckoutManager for FakeCheckoutManager {
    async fn list_checkouts(&self, _repo_root: &Path) -> Result<Vec<(PathBuf, Checkout)>, String> {
        let state = self.state.read().expect("FakeVcsState lock poisoned");
        Ok(state.checkouts.clone())
    }

    async fn create_checkout(&self, _repo_root: &Path, branch: &str, _create_branch: bool) -> Result<(PathBuf, Checkout), String> {
        let mut state = self.state.write().expect("FakeVcsState lock poisoned");
        let path = state.root.join(branch);
        let checkout = Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        };
        state.checkouts.push((path.clone(), checkout.clone()));
        Ok((path, checkout))
    }

    async fn remove_checkout(&self, _repo_root: &Path, branch: &str) -> Result<(), String> {
        let mut state = self.state.write().expect("FakeVcsState lock poisoned");
        state.checkouts.retain(|(_, c)| c.branch != branch);
        Ok(())
    }
}
```

- [ ] **Step 2: Update callers in `in_process_daemon.rs`**

The two tests that use `FakeCheckoutManager::new()` directly (around lines 2111 and 2210) now need to pass a `FakeVcsState`. Update them to create a `FakeVcsState` and pass it:

For each test, replace:
```rust
let checkout_manager = Arc::new(FakeCheckoutManager::new());
checkout_manager.add_checkouts(vec![(path, checkout)]);
```

with:
```rust
let state = FakeVcsState::builder()
    .root(repo.clone())
    .checkout(path, "branch-name", |c| c.is_main(false)
        // If the checkout needs custom correlation/association keys,
        // add those via the builder
    )
    .build();
let checkout_manager = Arc::new(FakeCheckoutManager::new(state));
```

Check the exact checkout data each test sets up — the old `add_checkouts` passed fully-built `Checkout` structs with specific `association_keys` and `correlation_keys`. The new builder may need extension (e.g. `.association_key(...)` on `CheckoutBuilder`) to support those fields. If the builder doesn't cover a field, add it.

Also check `fake_discovery_with_providers` — it accepts a `FakeCheckoutManager`. Update its signature and callers to match the new constructor.

- [ ] **Step 3: Verify compilation and tests**

Run: `cargo test -p flotilla-core --locked --features test-support 2>&1 | tail -30`

Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "refactor: replace FakeCheckoutManager with state-backed version"
```

### Task 6: Add `FakeVcsFactory` and update `FakeCheckoutManagerFactory`

Create a `FakeVcsFactory` and update the existing `FakeCheckoutManagerFactory` to work with `FakeVcsState`.

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

**Naming conflict:** There is already a `FakeCheckoutManagerFactory` in test_support.rs (line 582) that wraps `Arc<dyn CheckoutManager>`. Replace it with the new state-backed version. Update its existing callers (in `fake_discovery_with_providers`) to construct a `FakeCheckoutManager` from state and wrap it.

- [ ] **Step 1: Add `FakeVcsFactory`**

Add after the `FakeCheckoutManager` code:

```rust
pub struct FakeVcsFactory {
    state: Arc<RwLock<FakeVcsState>>,
    /// Unique name for this factory instance — needed when multiple
    /// FakeVcsFactory instances are registered (e.g. multi-root tests).
    name: String,
}

impl FakeVcsFactory {
    pub fn new(state: Arc<RwLock<FakeVcsState>>) -> Self {
        let name = {
            let s = state.read().expect("lock");
            format!("fake-{}", s.root.file_name().and_then(|n| n.to_str()).unwrap_or("repo"))
        };
        Self { state, name }
    }
}

#[async_trait]
impl super::Factory for FakeVcsFactory {
    type Output = dyn crate::providers::vcs::Vcs;

    fn descriptor(&self) -> super::ProviderDescriptor {
        super::ProviderDescriptor::labeled_simple(super::ProviderCategory::Vcs, &self.name, "Fake Git", "", "", "")
    }

    async fn probe(
        &self,
        _env: &super::EnvironmentBag,
        _config: &crate::config::ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn crate::providers::CommandRunner>,
        _attachable_store: crate::attachable::SharedAttachableStore,
    ) -> Result<Arc<dyn crate::providers::vcs::Vcs>, Vec<super::UnmetRequirement>> {
        Ok(Arc::new(FakeVcs::new(self.state.clone())))
    }
}
```

The `name` field derives a unique implementation name from the root path's last component (e.g. `"fake-repo"`, `"fake-repo-a"`, `"fake-repo-b"`). This avoids the probe_all registry overwrite issue when multiple factories are registered.

- [ ] **Step 2: Replace existing `FakeCheckoutManagerFactory`**

Replace the existing `FakeCheckoutManagerFactory` (around line 582) that wraps `Arc<dyn CheckoutManager>` with the state-backed version:

```rust
pub struct FakeCheckoutManagerFactory {
    state: Arc<RwLock<FakeVcsState>>,
    name: String,
}

impl FakeCheckoutManagerFactory {
    pub fn new(state: Arc<RwLock<FakeVcsState>>) -> Self {
        let name = {
            let s = state.read().expect("lock");
            format!("fake-{}", s.root.file_name().and_then(|n| n.to_str()).unwrap_or("repo"))
        };
        Self { state, name }
    }
}

#[async_trait]
impl super::Factory for FakeCheckoutManagerFactory {
    type Output = dyn crate::providers::vcs::CheckoutManager;

    fn descriptor(&self) -> super::ProviderDescriptor {
        super::ProviderDescriptor::labeled(
            super::ProviderCategory::CheckoutManager,
            &self.name,
            &self.name,
            "Fake Checkouts",
            "CO",
            "Checkouts",
            "checkout",
        )
    }

    async fn probe(
        &self,
        _env: &super::EnvironmentBag,
        _config: &crate::config::ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn crate::providers::CommandRunner>,
        _attachable_store: crate::attachable::SharedAttachableStore,
    ) -> Result<Arc<dyn crate::providers::vcs::CheckoutManager>, Vec<super::UnmetRequirement>> {
        Ok(Arc::new(FakeCheckoutManager::new(self.state.clone())))
    }
}
```

Update callers of the old `FakeCheckoutManagerFactory(arc)` constructor (in `fake_discovery_with_providers`) to use the new state-backed version. The old pattern was `FakeCheckoutManagerFactory(checkout_manager_arc)` — the new pattern requires a `FakeVcsState`.

- [ ] **Step 3: Add `fake_vcs_discovery` helper**

Add a helper function that wires `FakeVcs` and `FakeCheckoutManager` into a `DiscoveryRuntime`:

```rust
pub fn fake_vcs_discovery(state: Arc<RwLock<FakeVcsState>>) -> super::DiscoveryRuntime {
    let mut runtime = fake_discovery(false);
    // Replace the default vcs/checkout_manager factories with fakes.
    runtime.factories.vcs = vec![Box::new(FakeVcsFactory::new(state.clone()))];
    runtime.factories.checkout_managers = vec![Box::new(FakeCheckoutManagerFactory::new(state))];
    runtime
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p flotilla-core --locked --features test-support`

Expected: Compiles cleanly.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: add FakeVcsFactory and fake_vcs_discovery helper"
```

### Task 7: Create `daemon_for_fake_repo` helper and migrate first batch of tests

Replace `daemon_for_git_repo()` with `daemon_for_fake_repo()` and migrate the tests that use it.

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Add `daemon_for_fake_repo` helper**

In `crates/flotilla-core/tests/in_process_daemon.rs`, add a new helper near the existing `daemon_for_git_repo` (around line 289):

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

Add the necessary imports at the top of the file:
```rust
use flotilla_core::providers::discovery::test_support::{fake_vcs_discovery, FakeVcsState};
```

- [ ] **Step 2: Replace `daemon_for_git_repo()` calls**

Find all calls to `daemon_for_git_repo()` in the test file and replace with `daemon_for_fake_repo()`. The return types are identical so the surrounding test code should not need changes.

Use search: every line containing `daemon_for_git_repo()` gets replaced with `daemon_for_fake_repo()`.

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon 2>&1 | tail -40`

Expected: All tests that used `daemon_for_git_repo()` still pass. If any test fails, it's likely because it depends on specific git state (branch names, checkout paths) that the fake doesn't match — adjust the `FakeVcsState` builder in `daemon_for_fake_repo` or create a test-specific variant.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "refactor: replace daemon_for_git_repo with daemon_for_fake_repo"
```

### Task 8: Migrate `daemon_for_duplicate_git_repos` and standalone callers

Convert the remaining real-git tests: `daemon_for_duplicate_git_repos()` and the two tests that call `init_git_repo_with_local_bare_remote` directly.

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Add `daemon_for_duplicate_fake_repos` helper**

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

    // Each FakeVcsFactory derives a unique implementation name from the root
    // path's last component ("fake-repo-a", "fake-repo-b"), avoiding the
    // probe_all registry overwrite issue.
    let mut discovery = fake_discovery(false);
    discovery.factories.vcs = vec![
        Box::new(FakeVcsFactory::new(state_a.clone())),
        Box::new(FakeVcsFactory::new(state_b.clone())),
    ];
    discovery.factories.checkout_managers = vec![
        Box::new(FakeCheckoutManagerFactory::new(state_a)),
        Box::new(FakeCheckoutManagerFactory::new(state_b)),
    ];
    discovery.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo_a.clone(), repo_b.clone()], config, discovery, HostName::local()).await;
    (temp, repo_a, repo_b, daemon)
}
```

- [ ] **Step 2: Replace `daemon_for_duplicate_git_repos()` calls**

Find all calls to `daemon_for_duplicate_git_repos()` and replace with `daemon_for_duplicate_fake_repos()`.

- [ ] **Step 3: Migrate standalone `init_git_repo_with_local_bare_remote` callers**

Find the tests around lines 1198 (`add_and_remove_repo_updates_state_and_emits_events`) and 1304 (`adding_local_clone_promotes_remote_only_identity_to_local_execution`) that call `init_git_repo_with_local_bare_remote` directly.

**Important caveat:** These tests call `daemon.add_repo()` which goes through `normalize_repo_path`, which now delegates to `GitVcs::resolve_repo_root` via the `CommandRunner`. If the runner is a mock (`DiscoveryMockRunner`), the `git rev-parse` commands won't have canned responses and `normalize_repo_path` will return the path unchanged (falls through to `None` branch). Read each test carefully:

- If the test does NOT depend on `normalize_repo_path` actually resolving a worktree to its main repo (i.e., it just uses `add_repo` on a plain directory), the fallback behavior is fine and the migration works.
- If the test DOES depend on real repo resolution, keep it using `git_process_discovery` for now and note it as a follow-up. Don't force-migrate tests that need real git behavior.

Convert what you can; leave what you can't with a `// TODO: migrate to FakeVcs when normalize_repo_path supports discovered VCS` comment.

- [ ] **Step 4: Run all tests**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon 2>&1 | tail -40`

Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "refactor: replace remaining real-git daemon tests with fakes"
```

### Task 9: Clean up dead code and run full CI checks

Remove the git-spawning helpers that are no longer needed and verify the full test suite.

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` (remove helpers)
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs` (remove helpers if unused)

- [ ] **Step 1: Remove unused helpers from test file**

In `crates/flotilla-core/tests/in_process_daemon.rs`, remove:
- `daemon_for_git_repo()` helper
- `daemon_for_duplicate_git_repos()` helper
- `init_git_repo_with_local_bare_remote()` helper (if defined in the test file)
- `init_bare_git_remote()` helper (if defined in the test file)

Check if `local_bare_remote_discovery()` is still used anywhere. If not, remove it too.

- [ ] **Step 2: Check for unused helpers in test_support**

Check if `init_git_repo_with_remote`, `init_git_repo`, or other git-spawning helpers in `test_support.rs` are still used by other test files (multi_host.rs, peer_connect_flow.rs, socket_roundtrip.rs). Only remove helpers that have zero remaining callers.

Run: `cargo build --workspace --locked 2>&1 | grep "unused\|dead_code" | head -20`

Remove any functions flagged as unused.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -20`

Expected: No warnings.

- [ ] **Step 4: Run fmt**

Run: `cargo +nightly-2026-03-12 fmt --check 2>&1 | head -20`

If there are formatting issues:
Run: `cargo +nightly-2026-03-12 fmt`

- [ ] **Step 5: Run full test suite**

Run: `cargo test --workspace --locked 2>&1 | tail -40`

Expected: All tests pass. The tests that were using real git should now be fast.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "chore: remove unused git-spawning test helpers"
```
