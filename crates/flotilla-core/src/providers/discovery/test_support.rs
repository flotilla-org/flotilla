//! Shared mock runner for discovery tests.
//!
//! Provides `DiscoveryMockRunner` — a `CommandRunner` that returns canned
//! responses keyed by `(cmd, args)` and tracks which `cwd` paths and
//! `exists` calls were made.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use async_trait::async_trait;
use flotilla_protocol::{
    ChangeRequest, ChangeRequestStatus, Checkout, CorrelationKey, Issue, IssueChangeset, IssuePage, RepoIdentity, TerminalStatus, Workspace,
};
use tokio::sync::Mutex as TokioMutex;

use super::{DiscoveryRuntime, EnvironmentBag, Factory, FactoryRegistry, ProviderCategory, ProviderDescriptor, UnmetRequirement};
use crate::{
    attachable::{shared_file_backed_attachable_store, SharedAttachableStore},
    config::ConfigStore,
    providers::{
        change_request::ChangeRequestTracker, discovery::EnvVars, issue_tracker::IssueTracker, terminal::TerminalPool,
        vcs::CheckoutManager, workspace::WorkspaceManager, ChannelLabel, CommandOutput, CommandRunner,
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

pub fn test_attachable_store(config: &ConfigStore) -> SharedAttachableStore {
    shared_file_backed_attachable_store(config.base_path())
}

#[derive(Default)]
pub struct FakeDiscoveryProviders {
    pub checkout_manager: Option<Arc<dyn CheckoutManager>>,
    pub change_request: Option<Arc<dyn ChangeRequestTracker>>,
    pub issue_tracker: Option<Arc<dyn IssueTracker>>,
    pub workspace_manager: Option<Arc<dyn WorkspaceManager>>,
    pub terminal_pool: Option<Arc<dyn TerminalPool>>,
    pub attachable_store: Option<SharedAttachableStore>,
}

impl FakeDiscoveryProviders {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_checkout_manager(mut self, provider: Arc<dyn CheckoutManager>) -> Self {
        self.checkout_manager = Some(provider);
        self
    }

    pub fn with_change_request(mut self, provider: Arc<dyn ChangeRequestTracker>) -> Self {
        self.change_request = Some(provider);
        self
    }

    pub fn with_issue_tracker(mut self, provider: Arc<dyn IssueTracker>) -> Self {
        self.issue_tracker = Some(provider);
        self
    }

    pub fn with_workspace_manager(mut self, provider: Arc<dyn WorkspaceManager>) -> Self {
        self.workspace_manager = Some(provider);
        self
    }

    pub fn with_terminal_pool(mut self, provider: Arc<dyn TerminalPool>) -> Self {
        self.terminal_pool = Some(provider);
        self
    }

    pub fn with_attachable_store(mut self, store: SharedAttachableStore) -> Self {
        self.attachable_store = Some(store);
        self
    }
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
    /// Page numbers fetched via `list_issues_page`, for test assertions.
    pub pages_fetched: Arc<TokioMutex<Vec<u32>>>,
    /// When true, `list_issues_changed_since` returns `has_more: true` to
    /// trigger the escalation path in `refresh_issues_incremental`.
    pub force_escalation: Arc<AtomicBool>,
}

impl Default for FakeIssueTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeIssueTracker {
    pub fn new() -> Self {
        Self {
            issues: Arc::new(TokioMutex::new(Vec::new())),
            fetched_by_id: Arc::new(TokioMutex::new(Vec::new())),
            pages_fetched: Arc::new(TokioMutex::new(Vec::new())),
            force_escalation: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Pre-seed the issue store.
    pub async fn add_issues(&self, issues: Vec<(String, Issue)>) {
        self.issues.lock().await.extend(issues);
    }

    /// Enable forced escalation: `list_issues_changed_since` will return
    /// `has_more: true`, causing the daemon to abandon the incremental path
    /// and perform a full re-fetch.
    pub fn set_force_escalation(&self, enabled: bool) {
        self.force_escalation.store(enabled, Ordering::SeqCst);
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
        self.pages_fetched.lock().await.push(page);
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
        if self.force_escalation.load(Ordering::SeqCst) {
            // Return has_more: true with an empty changeset to trigger the
            // escalation path (full re-fetch) in refresh_issues_incremental.
            return Ok(IssueChangeset { updated: vec![], closed_ids: vec![], has_more: true });
        }
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

pub struct FakeWorkspaceManager {
    pub workspaces: Arc<TokioMutex<Vec<(String, Workspace)>>>,
    pub selected: Arc<TokioMutex<Vec<String>>>,
}

impl Default for FakeWorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeWorkspaceManager {
    pub fn new() -> Self {
        Self { workspaces: Arc::new(TokioMutex::new(Vec::new())), selected: Arc::new(TokioMutex::new(Vec::new())) }
    }

    pub async fn add_workspaces(&self, workspaces: Vec<(String, Workspace)>) {
        self.workspaces.lock().await.extend(workspaces);
    }
}

#[async_trait::async_trait]
impl WorkspaceManager for FakeWorkspaceManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        Ok(self.workspaces.lock().await.clone())
    }

    async fn create_workspace(&self, config: &crate::providers::types::WorkspaceConfig) -> Result<(String, Workspace), String> {
        let mut store = self.workspaces.lock().await;
        let ws_ref = format!("workspace:{}", store.len() + 1);
        let workspace = Workspace {
            name: config.name.clone(),
            directories: vec![config.working_directory.clone()],
            correlation_keys: vec![],
            attachable_set_id: None,
        };
        store.push((ws_ref.clone(), workspace.clone()));
        Ok((ws_ref, workspace))
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        self.selected.lock().await.push(ws_ref.to_string());
        Ok(())
    }
}

pub struct FakeTerminalPool {
    pub sessions: Arc<TokioMutex<Vec<super::super::terminal::TerminalSession>>>,
    pub killed: Arc<TokioMutex<Vec<String>>>,
}

impl Default for FakeTerminalPool {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTerminalPool {
    pub fn new() -> Self {
        Self { sessions: Arc::new(TokioMutex::new(Vec::new())), killed: Arc::new(TokioMutex::new(Vec::new())) }
    }

    pub async fn add_sessions(&self, sessions: Vec<super::super::terminal::TerminalSession>) {
        self.sessions.lock().await.extend(sessions);
    }
}

#[async_trait::async_trait]
impl TerminalPool for FakeTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<super::super::terminal::TerminalSession>, String> {
        Ok(self.sessions.lock().await.clone())
    }

    async fn ensure_session(&self, session_name: &str, command: &str, cwd: &Path) -> Result<(), String> {
        let mut sessions = self.sessions.lock().await;
        if sessions.iter().any(|s| s.session_name == session_name) {
            return Ok(());
        }
        sessions.push(super::super::terminal::TerminalSession {
            session_name: session_name.to_string(),
            status: TerminalStatus::Running,
            command: Some(command.to_string()),
            working_directory: Some(cwd.to_path_buf()),
        });
        Ok(())
    }

    async fn attach_command(
        &self,
        session_name: &str,
        _command: &str,
        _cwd: &Path,
        _env_vars: &super::super::terminal::TerminalEnvVars,
    ) -> Result<String, String> {
        Ok(format!("attach {session_name}"))
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        self.killed.lock().await.push(session_name.to_string());
        Ok(())
    }
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
        ProviderDescriptor::labeled_simple(
            ProviderCategory::CheckoutManager,
            "fake-checkouts",
            "Fake Checkouts",
            "CO",
            "Checkouts",
            "checkout",
        )
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

/// Factory that always returns a pre-constructed ChangeRequestTracker.
pub struct FakeChangeRequestFactory(pub Arc<dyn ChangeRequestTracker>);

#[async_trait::async_trait]
impl Factory for FakeChangeRequestFactory {
    type Output = dyn ChangeRequestTracker;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::ChangeRequest, "fake-cr", "Fake PRs", "PR", "Pull Requests", "pull request")
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn ChangeRequestTracker>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.0))
    }
}

pub struct FakeWorkspaceManagerFactory(pub Arc<dyn WorkspaceManager>);

#[async_trait::async_trait]
impl Factory for FakeWorkspaceManagerFactory {
    type Output = dyn WorkspaceManager;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(
            ProviderCategory::WorkspaceManager,
            "fake-workspaces",
            "Fake Workspaces",
            "WS",
            "Workspaces",
            "workspace",
        )
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn WorkspaceManager>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.0))
    }
}

pub struct FakeTerminalPoolFactory(pub Arc<dyn TerminalPool>);

#[async_trait::async_trait]
impl Factory for FakeTerminalPoolFactory {
    type Output = dyn TerminalPool;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(
            ProviderCategory::TerminalPool,
            "fake-terminals",
            "Fake Terminals",
            "TP",
            "Terminals",
            "terminal",
        )
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
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
    fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new()
            .with_checkout_manager_opt(checkout_manager)
            .with_change_request_opt(change_request)
            .with_issue_tracker_opt(issue_tracker),
    )
}

impl FakeDiscoveryProviders {
    fn with_checkout_manager_opt(mut self, provider: Option<Arc<dyn CheckoutManager>>) -> Self {
        self.checkout_manager = provider;
        self
    }

    fn with_change_request_opt(mut self, provider: Option<Arc<dyn ChangeRequestTracker>>) -> Self {
        self.change_request = provider;
        self
    }

    fn with_issue_tracker_opt(mut self, provider: Option<Arc<dyn IssueTracker>>) -> Self {
        self.issue_tracker = provider;
        self
    }
}

pub fn fake_discovery_with_provider_set(providers: FakeDiscoveryProviders) -> DiscoveryRuntime {
    let runner: Arc<dyn CommandRunner> =
        Arc::new(DiscoveryMockRunner::builder().on_run("git", &["--version"], Ok("git version 2.43.0".into())).build());

    let mut checkout_managers: Vec<Box<super::CheckoutManagerFactory>> = Vec::new();
    if let Some(cm) = providers.checkout_manager {
        checkout_managers.push(Box::new(FakeCheckoutManagerFactory(cm)));
    }

    let mut change_request_factories: Vec<Box<super::ChangeRequestFactory>> = Vec::new();
    if let Some(cr) = providers.change_request {
        change_request_factories.push(Box::new(FakeChangeRequestFactory(cr)));
    }

    let mut issue_tracker_factories: Vec<Box<super::IssueTrackerFactory>> = Vec::new();
    if let Some(it) = providers.issue_tracker {
        issue_tracker_factories.push(Box::new(FakeIssueTrackerFactory(it)));
    }

    let mut workspace_manager_factories: Vec<Box<super::WorkspaceManagerFactory>> = Vec::new();
    if let Some(ws) = providers.workspace_manager {
        workspace_manager_factories.push(Box::new(FakeWorkspaceManagerFactory(ws)));
    }

    let mut terminal_pool_factories: Vec<Box<super::TerminalPoolFactory>> = Vec::new();
    if let Some(pool) = providers.terminal_pool {
        terminal_pool_factories.push(Box::new(FakeTerminalPoolFactory(pool)));
    }

    let attachable_store = std::sync::OnceLock::new();
    if let Some(store) = providers.attachable_store {
        let _ = attachable_store.set(store);
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
            workspace_managers: workspace_manager_factories,
            terminal_pools: terminal_pool_factories,
        },
        attachable_store,
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
