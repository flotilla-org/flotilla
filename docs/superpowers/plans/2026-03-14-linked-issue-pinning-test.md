# Linked Issue Pinning Flow Integration Test

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add reusable fake providers (`FakeIssueTracker`, `FakeCheckoutManager`, `FakeCodeReview`) to `test_support`, then use them in an integration test that verifies the linked-issue-pinning flow end-to-end through `InProcessDaemon`.

**Architecture:** Build rich, configurable fake providers in `crates/flotilla-core/src/providers/discovery/test_support.rs` (gated behind `test-support` feature). Each fake holds `Arc<Mutex<Vec<...>>>` state that can be pre-seeded and inspected. Wrap each in a trivial `Factory` impl that always succeeds (no environment probing). The integration test constructs a `DiscoveryRuntime` with these factories, creates an `InProcessDaemon`, triggers a refresh, and asserts the snapshot contains the pinned issue.

**Tech Stack:** Rust, tokio, async-trait, flotilla-core test-support feature

---

## File Structure

| File | Role |
|------|------|
| Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs` | Add `FakeIssueTracker`, `FakeCheckoutManager`, `FakeCodeReview` + factory wrappers |
| Modify: `crates/flotilla-core/tests/in_process_daemon.rs` | Add `linked_issue_pinning_flow` integration test |

---

## Chunk 1: Fake Providers and Integration Test

### Task 0: Add all new imports to test_support.rs

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

- [ ] **Step 1: Merge the following imports into the existing `use` block at the top of the file**

The file already imports `std::collections::HashMap`, `std::path::{Path, PathBuf}`, `std::sync::Mutex`, `async_trait`, and `crate::providers::{...}`. Add these new imports, following the project's std → external → crate grouping:

```rust
use std::sync::Arc;

use tokio::sync::Mutex as TokioMutex;

use flotilla_protocol::{
    ChangeRequest, ChangeRequestStatus, Checkout, CorrelationKey, Issue, IssueChangeset, IssuePage,
};

use crate::{
    config::ConfigStore,
    providers::{
        code_review::CodeReview,
        issue_tracker::IssueTracker,
        vcs::CheckoutManager,
        CommandRunner,
    },
};

use super::{DiscoveryRuntime, EnvironmentBag, Factory, FactoryRegistry, ProviderDescriptor, UnmetRequirement};
```

- [ ] **Step 2: Build and verify it compiles**

Run: `cargo build -p flotilla-core`
Expected: compiles (unused import warnings are fine at this stage)

### Task 1: Add FakeIssueTracker to test_support

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

The `FakeIssueTracker` should be a rich, reusable fake suitable for integration tests, E2E tests, and RL environments. It stores pages of issues and responds to `fetch_issues_by_id` by looking up from its internal store.

- [ ] **Step 1: Add FakeIssueTracker struct and IssueTracker impl**

Add below the existing `fake_discovery` function. The fake stores all issues in a shared `Arc<TokioMutex<...>>` so the test can pre-seed data and later inspect state. It supports pagination via `list_issues_page`, lookup via `fetch_issues_by_id`, search via `search_issues`, and incremental sync via `list_issues_changed_since`.

```rust
/// A configurable fake issue tracker for integration and E2E tests.
///
/// Pre-seed issues via `add_issues()`, then pass to a `DiscoveryRuntime`
/// via `FakeIssueTrackerFactory`. All methods operate on the shared store,
/// so issues added after construction are visible to subsequent calls.
pub struct FakeIssueTracker {
    /// Shared issue store: Vec<(id, Issue)> preserving insertion order.
    pub issues: Arc<TokioMutex<Vec<(String, Issue)>>>,
    /// IDs that were requested via `fetch_issues_by_id`, for test assertions.
    pub fetched_by_id: Arc<TokioMutex<Vec<Vec<String>>>>,
}

impl FakeIssueTracker {
    pub fn new() -> Self {
        Self {
            issues: Arc::new(TokioMutex::new(Vec::new())),
            fetched_by_id: Arc::new(TokioMutex::new(Vec::new())),
        }
    }

    /// Pre-seed the issue store.
    pub async fn add_issues(&self, issues: Vec<(String, Issue)>) {
        self.issues.lock().await.extend(issues);
    }
}

#[async_trait::async_trait]
impl IssueTracker for FakeIssueTracker {
    async fn list_issues(&self, _repo_root: &Path, limit: usize) -> Result<Vec<(String, Issue)>, String> {
        let store = self.issues.lock().await;
        Ok(store.iter().take(limit).cloned().collect())
    }

    async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn list_issues_page(&self, _repo_root: &Path, page: u32, per_page: usize) -> Result<IssuePage, String> {
        let store = self.issues.lock().await;
        let start = (page.saturating_sub(1) as usize) * per_page;
        let issues: Vec<_> = store.iter().skip(start).take(per_page).cloned().collect();
        let has_more = start + per_page < store.len();
        Ok(IssuePage {
            issues,
            total_count: Some(store.len() as u32),
            has_more,
        })
    }

    async fn fetch_issues_by_id(&self, _repo_root: &Path, ids: &[String]) -> Result<Vec<(String, Issue)>, String> {
        self.fetched_by_id.lock().await.push(ids.to_vec());
        let store = self.issues.lock().await;
        Ok(store.iter().filter(|(id, _)| ids.contains(id)).cloned().collect())
    }

    async fn search_issues(&self, _repo_root: &Path, query: &str, limit: usize) -> Result<Vec<(String, Issue)>, String> {
        let store = self.issues.lock().await;
        let query_lower = query.to_lowercase();
        Ok(store
            .iter()
            .filter(|(_, issue)| issue.title.to_lowercase().contains(&query_lower))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn list_issues_changed_since(&self, repo_root: &Path, _since: &str, per_page: usize) -> Result<IssueChangeset, String> {
        // Fake has no timestamp tracking — delegate to page 1
        let page = self.list_issues_page(repo_root, 1, per_page).await?;
        Ok(IssueChangeset {
            updated: page.issues,
            closed_ids: vec![],
            has_more: page.has_more,
        })
    }
}
```

- [ ] **Step 2: Build and verify it compiles**

Run: `cargo build -p flotilla-core`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/test_support.rs
git commit -m "feat: add FakeIssueTracker to test_support"
```

### Task 2: Add FakeCheckoutManager to test_support

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

- [ ] **Step 1: Add FakeCheckoutManager struct and CheckoutManager impl**

The fake holds a shared list of checkouts that the test pre-seeds. It supports creation and removal for completeness.

```rust
/// A configurable fake checkout manager for integration and E2E tests.
///
/// Pre-seed checkouts via `add_checkouts()`. Supports `create_checkout`
/// and `remove_checkout` for tests that exercise the full lifecycle.
pub struct FakeCheckoutManager {
    pub checkouts: Arc<TokioMutex<Vec<(PathBuf, Checkout)>>>,
}

impl FakeCheckoutManager {
    pub fn new() -> Self {
        Self {
            checkouts: Arc::new(TokioMutex::new(Vec::new())),
        }
    }

    pub async fn add_checkouts(&self, checkouts: Vec<(PathBuf, Checkout)>) {
        self.checkouts.lock().await.extend(checkouts);
    }
}

#[async_trait::async_trait]
impl CheckoutManager for FakeCheckoutManager {
    async fn list_checkouts(&self, _repo_root: &Path) -> Result<Vec<(PathBuf, Checkout)>, String> {
        Ok(self.checkouts.lock().await.clone())
    }

    async fn create_checkout(&self, repo_root: &Path, branch: &str, _create_branch: bool) -> Result<(PathBuf, Checkout), String> {
        let path = repo_root.join(branch);
        let checkout = Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![CorrelationKey::Branch(branch.to_string())],
            association_keys: vec![],
        };
        self.checkouts.lock().await.push((path.clone(), checkout.clone()));
        Ok((path, checkout))
    }

    async fn remove_checkout(&self, _repo_root: &Path, branch: &str) -> Result<(), String> {
        self.checkouts.lock().await.retain(|(_, co)| co.branch != branch);
        Ok(())
    }
}
```

- [ ] **Step 2: Build and verify it compiles**

Run: `cargo build -p flotilla-core`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/test_support.rs
git commit -m "feat: add FakeCheckoutManager to test_support"
```

### Task 3: Add FakeCodeReview to test_support

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

- [ ] **Step 1: Add FakeCodeReview struct and CodeReview impl**

```rust
/// A configurable fake code review provider for integration and E2E tests.
///
/// Pre-seed change requests via `add_change_requests()`. Supports
/// `close_change_request` and merged branch tracking.
pub struct FakeCodeReview {
    pub change_requests: Arc<TokioMutex<Vec<(String, ChangeRequest)>>>,
    pub merged_branches: Arc<TokioMutex<Vec<String>>>,
}

impl FakeCodeReview {
    pub fn new() -> Self {
        Self {
            change_requests: Arc::new(TokioMutex::new(Vec::new())),
            merged_branches: Arc::new(TokioMutex::new(Vec::new())),
        }
    }

    pub async fn add_change_requests(&self, crs: Vec<(String, ChangeRequest)>) {
        self.change_requests.lock().await.extend(crs);
    }
}

#[async_trait::async_trait]
impl CodeReview for FakeCodeReview {
    async fn list_change_requests(&self, _repo_root: &Path, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        let store = self.change_requests.lock().await;
        Ok(store.iter().take(limit).cloned().collect())
    }

    async fn get_change_request(&self, _repo_root: &Path, id: &str) -> Result<(String, ChangeRequest), String> {
        let store = self.change_requests.lock().await;
        store
            .iter()
            .find(|(cr_id, _)| cr_id == id)
            .cloned()
            .ok_or_else(|| format!("change request {id} not found"))
    }

    async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn close_change_request(&self, _repo_root: &Path, id: &str) -> Result<(), String> {
        let mut store = self.change_requests.lock().await;
        if let Some((_, cr)) = store.iter_mut().find(|(cr_id, _)| cr_id == id) {
            cr.status = ChangeRequestStatus::Closed;
            Ok(())
        } else {
            Err(format!("change request {id} not found"))
        }
    }

    async fn list_merged_branch_names(&self, _repo_root: &Path, limit: usize) -> Result<Vec<String>, String> {
        let store = self.merged_branches.lock().await;
        Ok(store.iter().take(limit).cloned().collect())
    }
}
```

- [ ] **Step 2: Build and verify it compiles**

Run: `cargo build -p flotilla-core`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/test_support.rs
git commit -m "feat: add FakeCodeReview to test_support"
```

### Task 4: Add factory wrappers and discovery helper

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

These factories always succeed — they ignore the environment bag entirely and return pre-constructed provider instances. This lets tests inject fake providers through `DiscoveryRuntime` without needing mock environment assertions.

- [ ] **Step 1: Add factory structs and a discovery runtime builder**

```rust
/// Factory that always returns a pre-constructed IssueTracker.
pub struct FakeIssueTrackerFactory(pub Arc<dyn IssueTracker>);

#[async_trait::async_trait]
impl Factory for FakeIssueTrackerFactory {
    type Output = dyn IssueTracker;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("fake-issues", "Fake Issues", "#", "Issues", "issue")
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn IssueTracker>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.0))
    }
}

/// Factory that always returns a pre-constructed CheckoutManager.
pub struct FakeCheckoutManagerFactory(pub Arc<dyn CheckoutManager>);

#[async_trait::async_trait]
impl Factory for FakeCheckoutManagerFactory {
    type Output = dyn CheckoutManager;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("fake-checkouts", "Fake Checkouts", "CO", "Checkouts", "checkout")
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.0))
    }
}

/// Factory that always returns a pre-constructed CodeReview.
pub struct FakeCodeReviewFactory(pub Arc<dyn CodeReview>);

#[async_trait::async_trait]
impl Factory for FakeCodeReviewFactory {
    type Output = dyn CodeReview;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("fake-cr", "Fake PRs", "PR", "Pull Requests", "pull request")
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CodeReview>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.0))
    }
}
```

- [ ] **Step 2: Add `fake_discovery_with_providers` helper**

This is the main test entry point — it builds a `DiscoveryRuntime` that produces the given fake providers for every repo, along with a minimal git runner.

```rust
/// Build a `DiscoveryRuntime` with fake providers injected.
///
/// The returned runtime has no host/repo detectors (environment assertions
/// are irrelevant since the fake factories always succeed). Suitable for
/// integration tests and RL environments where you want deterministic
/// provider data without probing the real filesystem.
pub fn fake_discovery_with_providers(
    checkout_manager: Option<Arc<dyn CheckoutManager>>,
    code_review: Option<Arc<dyn CodeReview>>,
    issue_tracker: Option<Arc<dyn IssueTracker>>,
) -> DiscoveryRuntime {
    let runner: Arc<dyn CommandRunner> =
        Arc::new(DiscoveryMockRunner::builder().on_run("git", &["--version"], Ok("git version 2.43.0".into())).build());

    let mut checkout_managers: Vec<Box<super::CheckoutManagerFactory>> = Vec::new();
    if let Some(cm) = checkout_manager {
        checkout_managers.push(Box::new(FakeCheckoutManagerFactory(cm)));
    }

    let mut code_review_factories: Vec<Box<super::CodeReviewFactory>> = Vec::new();
    if let Some(cr) = code_review {
        code_review_factories.push(Box::new(FakeCodeReviewFactory(cr)));
    }

    let mut issue_tracker_factories: Vec<Box<super::IssueTrackerFactory>> = Vec::new();
    if let Some(it) = issue_tracker {
        issue_tracker_factories.push(Box::new(FakeIssueTrackerFactory(it)));
    }

    DiscoveryRuntime {
        runner,
        env: Arc::new(TestEnvVars::default()),
        host_detectors: vec![],
        repo_detectors: vec![],
        factories: FactoryRegistry {
            vcs: vec![],
            checkout_managers,
            code_review: code_review_factories,
            issue_trackers: issue_tracker_factories,
            cloud_agents: vec![],
            ai_utilities: vec![],
            workspace_managers: vec![],
            terminal_pools: vec![],
        },
    }
}
```

- [ ] **Step 3: Build and verify it compiles**

Run: `cargo build -p flotilla-core`
Expected: compiles with no errors

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/test_support.rs
git commit -m "feat: add fake provider factories and discovery runtime builder"
```

### Task 5: Write the linked issue pinning integration test

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

This test exercises the full async flow from issue #123:
1. Creates a daemon with a `FakeCheckoutManager` returning a checkout with `AssociationKey::IssueRef("fake-issues", "42")`
2. Registers a `FakeIssueTracker` containing issue 42
3. Triggers a refresh — the refresh loop populates `last_snapshot.providers` with the checkout
4. `poll_snapshots()` broadcasts the initial snapshot, then calls `fetch_missing_linked_issues()`
5. That function finds issue 42 referenced but missing from the cache, calls `fetch_issues_by_id`
6. The fetched issue is pinned in the cache and a second snapshot is broadcast
7. The test receives the second snapshot and asserts issue 42 is present

- [ ] **Step 1: Write the failing test**

```rust
use flotilla_core::providers::discovery::test_support::{
    fake_discovery_with_providers, FakeCheckoutManager, FakeIssueTracker,
};
use flotilla_protocol::{AssociationKey, Checkout, CorrelationKey, Issue};

#[tokio::test]
async fn linked_issue_pinning_fetches_and_broadcasts_missing_issues() {
    // --- Arrange ---

    // Create a checkout that references issue #42
    let checkout_manager = Arc::new(FakeCheckoutManager::new());
    checkout_manager
        .add_checkouts(vec![(
            PathBuf::from("/tmp/repo/feat-branch"),
            Checkout {
                branch: "feat-branch".into(),
                is_main: false,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: vec![CorrelationKey::Branch("feat-branch".into())],
                association_keys: vec![AssociationKey::IssueRef("fake-issues".into(), "42".into())],
            },
        )])
        .await;

    // Create an issue tracker that has issue #42 available
    let issue_tracker = Arc::new(FakeIssueTracker::new());
    issue_tracker
        .add_issues(vec![(
            "42".into(),
            Issue {
                title: "Fix the widget".into(),
                labels: vec!["bug".into()],
                association_keys: vec![AssociationKey::IssueRef("fake-issues".into(), "42".into())],
                provider_name: "fake-issues".into(),
                provider_display_name: "Fake Issues".into(),
            },
        )])
        .await;

    let discovery = fake_discovery_with_providers(
        Some(checkout_manager.clone() as Arc<dyn flotilla_core::providers::vcs::CheckoutManager>),
        None,
        Some(issue_tracker.clone() as Arc<dyn flotilla_core::providers::issue_tracker::IssueTracker>),
    );

    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = Arc::new(flotilla_core::config::ConfigStore::with_base(temp.path().join("config")));
    let daemon = flotilla_core::in_process::InProcessDaemon::new(
        vec![repo.clone()],
        config,
        discovery,
        flotilla_protocol::HostName::local(),
    )
    .await;

    let mut rx = daemon.subscribe();

    // --- Act ---
    // Trigger a refresh. The refresh loop will:
    // 1. Call FakeCheckoutManager::list_checkouts → checkout with IssueRef("42")
    // 2. Broadcast initial snapshot (no issues yet)
    // 3. Call fetch_missing_linked_issues → finds "42" missing → calls fetch_issues_by_id
    // 4. Broadcast updated snapshot with pinned issue
    daemon.refresh(&repo).await.expect("refresh should succeed");

    // --- Assert ---
    // Collect snapshot events until we see one containing issue "42".
    // The first snapshot may not have the issue (it's fetched async after broadcast),
    // but the second one should.
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(flotilla_protocol::DaemonEvent::SnapshotFull(snap)) if snap.repo == repo => {
                    if snap.providers.issues.contains_key("42") {
                        return *snap;
                    }
                }
                // Ignore deltas and other events — the re-broadcast after
                // pinning sends a full snapshot since it rebuilds from scratch.
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for snapshot with pinned issue");

    // Verify the issue is present and correct
    let issue = found.providers.issues.get("42").expect("issue 42 should be in snapshot");
    assert_eq!(issue.title, "Fix the widget");
    assert_eq!(issue.labels, vec!["bug".to_string()]);

    // Verify fetch_issues_by_id was actually called (not just paginated)
    let fetched = issue_tracker.fetched_by_id.lock().await;
    assert!(!fetched.is_empty(), "fetch_issues_by_id should have been called");
    assert!(
        fetched.iter().any(|ids| ids.contains(&"42".to_string())),
        "fetch_issues_by_id should have been called with id '42'"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails (fake providers not yet wired)**

Run: `cargo test -p flotilla-core --test in_process_daemon linked_issue_pinning -- --nocapture`
Expected: compilation error — `fake_discovery_with_providers` not yet implemented

Note: This step will fail until the fake providers from Tasks 1-4 are implemented. When executing this plan, implement Tasks 1-4 first, then run this test.

- [ ] **Step 3: Once Tasks 1-4 are done, run the test and verify it passes**

Run: `cargo test -p flotilla-core --test in_process_daemon linked_issue_pinning -- --nocapture`
Expected: PASS

- [ ] **Step 4: Run the full test suite to check for regressions**

Run: `cargo test --locked`
Expected: all tests pass

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo +nightly fmt`
Expected: no warnings, code formatted

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "test: add linked issue pinning flow integration test (#123)"
```
