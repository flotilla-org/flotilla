//! Shared mock runner for discovery tests.
//!
//! Provides `DiscoveryMockRunner` — a `CommandRunner` that returns canned
//! responses keyed by `(cmd, args)` and tracks which `cwd` paths and
//! `exists` calls were made.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::{Arc, Mutex, OnceLock},
};

use async_trait::async_trait;
use flotilla_protocol::{ChangeRequest, ChangeRequestStatus, Checkout, CorrelationKey, Issue, IssueChangeset, IssuePage, RepoIdentity};
use tokio::sync::Mutex as TokioMutex;

use super::{DiscoveryRuntime, EnvironmentBag, Factory, FactoryRegistry, ProviderCategory, ProviderDescriptor, UnmetRequirement};
use crate::{
    config::ConfigStore,
    providers::{
        change_request::ChangeRequestTracker, discovery::EnvVars, issue_tracker::IssueTracker, vcs::CheckoutManager, ChannelLabel,
        CommandOutput, CommandRunner,
    },
};

type ResponseMap = HashMap<(String, String), Vec<Result<String, String>>>;

pub struct DiscoveryMockRunnerBuilder {
    responses: ResponseMap,
    tool_exists: HashMap<String, bool>,
}

pub struct DiscoveryMockRunner {
    responses: Mutex<ResponseMap>,
    tool_exists: HashMap<String, bool>,
    seen_cwds: Mutex<Vec<PathBuf>>,
    exists_calls: Mutex<Vec<(String, String)>>,
}

#[derive(Default)]
pub struct TestEnvVars {
    vars: HashMap<String, String>,
}

impl DiscoveryMockRunner {
    pub fn builder() -> DiscoveryMockRunnerBuilder {
        DiscoveryMockRunnerBuilder { responses: HashMap::new(), tool_exists: HashMap::new() }
    }

    #[allow(dead_code)]
    pub fn saw_cwd(&self, cwd: &Path) -> bool {
        self.seen_cwds.lock().expect("lock poisoned").iter().any(|p| p == cwd)
    }

    #[allow(dead_code)]
    pub fn exists_call_count(&self, cmd: &str) -> usize {
        self.exists_calls.lock().expect("lock poisoned").iter().filter(|(called, _)| called == cmd).count()
    }
}

pub fn init_git_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("create repo dir");
    let status = ProcessCommand::new("git").args(["init", "--initial-branch=main"]).arg(path).status().expect("run git init");
    assert!(status.success(), "git init should succeed");

    let repo = path.to_str().expect("repo path utf8");
    let status =
        ProcessCommand::new("git").args(["-C", repo, "config", "user.name", "Flotilla Tests"]).status().expect("configure git user.name");
    assert!(status.success(), "git config user.name should succeed");

    let status = ProcessCommand::new("git")
        .args(["-C", repo, "config", "user.email", "flotilla@example.com"])
        .status()
        .expect("configure git user.email");
    assert!(status.success(), "git config user.email should succeed");

    std::fs::write(path.join("README.md"), "hello\n").expect("write README");
    let status = ProcessCommand::new("git").args(["-C", repo, "add", "README.md"]).status().expect("run git add");
    assert!(status.success(), "git add should succeed");

    let status = ProcessCommand::new("git").args(["-C", repo, "commit", "-m", "init"]).status().expect("run git commit");
    assert!(status.success(), "git commit should succeed");
}

pub fn init_git_repo_with_remote(path: &Path, remote: &str) -> RepoIdentity {
    init_git_repo(path);
    let repo = path.to_str().expect("repo path utf8");
    let status = ProcessCommand::new("git").args(["-C", repo, "remote", "add", "origin", remote]).status().expect("git remote add origin");
    assert!(status.success(), "git remote add origin should succeed");
    RepoIdentity::from_remote_url(remote).expect("remote should produce repo identity")
}

impl DiscoveryMockRunnerBuilder {
    pub fn on_run(mut self, cmd: &str, args: &[&str], response: Result<String, String>) -> Self {
        let key = (cmd.to_string(), args.join(" "));
        self.responses.entry(key).or_default().push(response);
        self
    }

    pub fn tool_exists(mut self, cmd: &str, exists: bool) -> Self {
        self.tool_exists.insert(cmd.to_string(), exists);
        self
    }

    pub fn build(self) -> DiscoveryMockRunner {
        DiscoveryMockRunner {
            responses: Mutex::new(self.responses),
            tool_exists: self.tool_exists,
            seen_cwds: Mutex::new(Vec::new()),
            exists_calls: Mutex::new(Vec::new()),
        }
    }
}

impl TestEnvVars {
    pub fn new<K, V, I>(vars: I) -> Self
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        Self { vars: vars.into_iter().map(|(key, value)| (key.into(), value.into())).collect() }
    }
}

impl EnvVars for TestEnvVars {
    fn get(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
}

#[async_trait]
impl CommandRunner for DiscoveryMockRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        self.seen_cwds.lock().expect("lock poisoned").push(cwd.to_path_buf());
        let key = (cmd.to_string(), args.join(" "));
        let mut map = self.responses.lock().expect("lock poisoned");
        if let Some(queue) = map.get_mut(&key) {
            if !queue.is_empty() {
                return queue.remove(0);
            }
        }
        Err(format!("DiscoveryMockRunner: no response for {cmd} {}", args.join(" ")))
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        match self.run(cmd, args, cwd, label).await {
            Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), success: true }),
            Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, success: false }),
        }
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.exists_calls.lock().expect("lock poisoned").push((cmd.to_string(), args.join(" ")));
        self.tool_exists.get(cmd).copied().unwrap_or(false)
    }
}
/// Build a `DiscoveryRuntime` that uses no-op env and a minimal fake runner
/// (only responds to `git --version`). Avoids probing ambient host tools.
pub fn fake_discovery(follower: bool) -> super::DiscoveryRuntime {
    minimal_discovery_runtime(
        follower,
        std::sync::Arc::new(DiscoveryMockRunner::builder().on_run("git", &["--version"], Ok("git version 2.43.0".into())).build()),
    )
}

/// Build a `DiscoveryRuntime` that allows real git commands while still
/// avoiding ambient host-tool probes like gh, Codex, Claude, or cmux.
pub fn git_process_discovery(follower: bool) -> super::DiscoveryRuntime {
    minimal_discovery_runtime(follower, std::sync::Arc::new(crate::providers::ProcessCommandRunner))
}

fn minimal_discovery_runtime(follower: bool, runner: std::sync::Arc<dyn CommandRunner>) -> super::DiscoveryRuntime {
    let factories = if follower { super::FactoryRegistry::for_follower() } else { super::FactoryRegistry::default_all() };
    super::DiscoveryRuntime {
        runner,
        env: std::sync::Arc::new(TestEnvVars::default()),
        host_detectors: vec![Box::new(super::detectors::generic::CommandDetector::new(
            "git",
            &["--version"],
            super::detectors::generic::parse_first_dotted_version,
        ))],
        repo_detectors: super::detectors::default_repo_detectors(),
        factories,
        attachable_store: OnceLock::new(),
    }
}
// ---------------------------------------------------------------------------
// Fake providers for integration / E2E tests
// ---------------------------------------------------------------------------

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

impl Default for FakeIssueTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeIssueTracker {
    pub fn new() -> Self {
        Self { issues: Arc::new(TokioMutex::new(Vec::new())), fetched_by_id: Arc::new(TokioMutex::new(Vec::new())) }
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
        Ok(IssuePage { issues, total_count: Some(store.len() as u32), has_more })
    }

    async fn fetch_issues_by_id(&self, _repo_root: &Path, ids: &[String]) -> Result<Vec<(String, Issue)>, String> {
        self.fetched_by_id.lock().await.push(ids.to_vec());
        let store = self.issues.lock().await;
        Ok(store.iter().filter(|(id, _)| ids.contains(id)).cloned().collect())
    }

    async fn search_issues(&self, _repo_root: &Path, query: &str, limit: usize) -> Result<Vec<(String, Issue)>, String> {
        let store = self.issues.lock().await;
        let query_lower = query.to_lowercase();
        Ok(store.iter().filter(|(_, issue)| issue.title.to_lowercase().contains(&query_lower)).take(limit).cloned().collect())
    }

    async fn list_issues_changed_since(&self, repo_root: &Path, _since: &str, per_page: usize) -> Result<IssueChangeset, String> {
        let page = self.list_issues_page(repo_root, 1, per_page).await?;
        Ok(IssueChangeset { updated: page.issues, closed_ids: vec![], has_more: page.has_more })
    }
}

/// A configurable fake checkout manager for integration and E2E tests.
///
/// Pre-seed checkouts via `add_checkouts()`. Supports `create_checkout`
/// and `remove_checkout` for tests that exercise the full lifecycle.
pub struct FakeCheckoutManager {
    pub checkouts: Arc<TokioMutex<Vec<(PathBuf, Checkout)>>>,
}

impl Default for FakeCheckoutManager {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeCheckoutManager {
    pub fn new() -> Self {
        Self { checkouts: Arc::new(TokioMutex::new(Vec::new())) }
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

/// A configurable fake change request provider for integration and E2E tests.
///
/// Pre-seed change requests via `add_change_requests()`. Supports
/// `close_change_request` and merged branch tracking.
pub struct FakeChangeRequest {
    pub change_requests: Arc<TokioMutex<Vec<(String, ChangeRequest)>>>,
    pub merged_branches: Arc<TokioMutex<Vec<String>>>,
}

impl Default for FakeChangeRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeChangeRequest {
    pub fn new() -> Self {
        Self { change_requests: Arc::new(TokioMutex::new(Vec::new())), merged_branches: Arc::new(TokioMutex::new(Vec::new())) }
    }

    pub async fn add_change_requests(&self, crs: Vec<(String, ChangeRequest)>) {
        self.change_requests.lock().await.extend(crs);
    }
}

#[async_trait::async_trait]
impl ChangeRequestTracker for FakeChangeRequest {
    async fn list_change_requests(&self, _repo_root: &Path, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        let store = self.change_requests.lock().await;
        Ok(store.iter().take(limit).cloned().collect())
    }

    async fn get_change_request(&self, _repo_root: &Path, id: &str) -> Result<(String, ChangeRequest), String> {
        let store = self.change_requests.lock().await;
        store.iter().find(|(cr_id, _)| cr_id == id).cloned().ok_or_else(|| format!("change request {id} not found"))
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

// ---------------------------------------------------------------------------
// Factory wrappers
// ---------------------------------------------------------------------------

/// Factory that always returns a pre-constructed IssueTracker.
pub struct FakeIssueTrackerFactory(pub Arc<dyn IssueTracker>);

#[async_trait::async_trait]
impl Factory for FakeIssueTrackerFactory {
    type Output = dyn IssueTracker;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::IssueTracker, "fake-issues", "Fake Issues", "#", "Issues", "issue")
    }

    async fn probe_with_services(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
        _attachable_store: crate::attachable::SharedAttachableStore,
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
        ProviderDescriptor::labeled_simple(
            ProviderCategory::CheckoutManager,
            "fake-checkouts",
            "Fake Checkouts",
            "CO",
            "Checkouts",
            "checkout",
        )
    }

    async fn probe_with_services(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
        _attachable_store: crate::attachable::SharedAttachableStore,
    ) -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.0))
    }
}

/// Factory that always returns a pre-constructed ChangeRequestTracker.
pub struct FakeChangeRequestFactory(pub Arc<dyn ChangeRequestTracker>);

#[async_trait::async_trait]
impl Factory for FakeChangeRequestFactory {
    type Output = dyn ChangeRequestTracker;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::ChangeRequest, "fake-cr", "Fake PRs", "PR", "Pull Requests", "pull request")
    }

    async fn probe_with_services(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
        _attachable_store: crate::attachable::SharedAttachableStore,
    ) -> Result<Arc<dyn ChangeRequestTracker>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.0))
    }
}

/// Build a `DiscoveryRuntime` with fake providers injected.
///
/// The returned runtime has no host/repo detectors (environment assertions
/// are irrelevant since the fake factories always succeed). Suitable for
/// integration tests and RL environments where you want deterministic
/// provider data without probing the real filesystem.
pub fn fake_discovery_with_providers(
    checkout_manager: Option<Arc<dyn CheckoutManager>>,
    change_request: Option<Arc<dyn ChangeRequestTracker>>,
    issue_tracker: Option<Arc<dyn IssueTracker>>,
) -> DiscoveryRuntime {
    let runner: Arc<dyn CommandRunner> =
        Arc::new(DiscoveryMockRunner::builder().on_run("git", &["--version"], Ok("git version 2.43.0".into())).build());

    let mut checkout_managers: Vec<Box<super::CheckoutManagerFactory>> = Vec::new();
    if let Some(cm) = checkout_manager {
        checkout_managers.push(Box::new(FakeCheckoutManagerFactory(cm)));
    }

    let mut change_request_factories: Vec<Box<super::ChangeRequestFactory>> = Vec::new();
    if let Some(cr) = change_request {
        change_request_factories.push(Box::new(FakeChangeRequestFactory(cr)));
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
            change_requests: change_request_factories,
            issue_trackers: issue_tracker_factories,
            cloud_agents: vec![],
            ai_utilities: vec![],
            workspace_managers: vec![],
            terminal_pools: vec![],
        },
        attachable_store: OnceLock::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::{run_host_detectors, EnvironmentAssertion};

    #[tokio::test]
    async fn fake_discovery_uses_only_git_host_detector() {
        let runtime = fake_discovery(false);
        let bag = run_host_detectors(&runtime.host_detectors, &*runtime.runner, &*runtime.env).await;

        assert!(matches!(
            bag.assertions(),
            [EnvironmentAssertion::BinaryAvailable { name, version, .. }]
            if name == "git" && version.as_deref() == Some("2.43.0")
        ));
    }
}
