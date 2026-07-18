use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use flotilla_protocol::{
    qualified_path::{HostId, QualifiedPath},
    test_support::{TestChangeRequest, TestCheckout, TestSession},
};

use super::*;
use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{
        change_request::ChangeRequestTracker,
        coding_agent::CloudAgentService,
        discovery::{ProviderCategory, ProviderDescriptor},
        presentation::PresentationManager,
        terminal::TerminalPool,
        types::*,
        vcs::{CheckoutManager, Vcs},
    },
};

fn desc(name: &str) -> ProviderDescriptor {
    ProviderDescriptor::named(ProviderCategory::Vcs, name)
}

struct MockCheckoutManager {
    result: Result<Vec<(PathBuf, Checkout)>, String>,
}

impl MockCheckoutManager {
    fn ok(checkouts: Vec<(PathBuf, Checkout)>) -> Self {
        Self { result: Ok(checkouts) }
    }

    fn failing(msg: &str) -> Self {
        Self { result: Err(msg.to_string()) }
    }
}

#[async_trait]
impl CheckoutManager for MockCheckoutManager {
    async fn validate_target(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        _branch: &str,
        _intent: flotilla_protocol::CheckoutIntent,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn list_checkouts(&self, _repo_root: &ExecutionEnvironmentPath) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        self.result
            .as_ref()
            .map(|v| v.iter().map(|(p, co)| (ExecutionEnvironmentPath::new(p), co.clone())).collect())
            .map_err(|e| e.clone())
    }

    async fn create_checkout(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        _branch: &str,
        _create_branch: bool,
    ) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        Err("not implemented".to_string())
    }

    async fn remove_checkout(&self, _repo_root: &ExecutionEnvironmentPath, _branch: &str) -> Result<(), String> {
        Err("not implemented".to_string())
    }
}

struct MockChangeRequestTracker {
    change_requests_result: Result<Vec<(String, ChangeRequest)>, String>,
    merged_result: Result<Vec<String>, String>,
}

impl MockChangeRequestTracker {
    fn ok(change_requests: Vec<(String, ChangeRequest)>, merged_branches: Vec<String>) -> Self {
        Self { change_requests_result: Ok(change_requests), merged_result: Ok(merged_branches) }
    }

    fn failing(change_requests_msg: &str, merged_msg: &str) -> Self {
        Self { change_requests_result: Err(change_requests_msg.to_string()), merged_result: Err(merged_msg.to_string()) }
    }
}

#[async_trait]
impl ChangeRequestTracker for MockChangeRequestTracker {
    async fn list_change_requests(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        self.change_requests_result.clone()
    }

    async fn get_change_request(&self, _repo_root: &Path, _id: &str) -> Result<(String, ChangeRequest), String> {
        Err("not implemented".to_string())
    }

    async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn close_change_request(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn list_merged_branch_names(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<String>, String> {
        self.merged_result.clone()
    }
}

struct MockCloudAgent {
    result: Result<Vec<(String, CloudAgentSession)>, String>,
}

impl MockCloudAgent {
    fn ok(sessions: Vec<(String, CloudAgentSession)>) -> Self {
        Self { result: Ok(sessions) }
    }

    fn ok_named(_name: &str, sessions: Vec<(String, CloudAgentSession)>) -> Self {
        Self { result: Ok(sessions) }
    }

    fn failing(msg: &str) -> Self {
        Self { result: Err(msg.to_string()) }
    }

    fn failing_named(_name: &str, msg: &str) -> Self {
        Self { result: Err(msg.to_string()) }
    }
}

#[async_trait]
impl CloudAgentService for MockCloudAgent {
    async fn list_sessions(&self, _criteria: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
        self.result.clone()
    }

    async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn attach_command(&self, _session_id: &str) -> Result<String, String> {
        Ok("mock --attach".to_string())
    }
}

struct MockVcs {
    result: Result<Vec<String>, String>,
}

impl MockVcs {
    fn ok(branches: Vec<String>) -> Self {
        Self { result: Ok(branches) }
    }

    fn failing(msg: &str) -> Self {
        Self { result: Err(msg.to_string()) }
    }
}

#[async_trait]
impl Vcs for MockVcs {
    async fn resolve_repo_root(&self, _path: &ExecutionEnvironmentPath) -> Option<ExecutionEnvironmentPath> {
        None
    }

    async fn list_local_branches(&self, _repo_root: &ExecutionEnvironmentPath) -> Result<Vec<BranchInfo>, String> {
        Ok(vec![])
    }

    async fn list_remote_branches(&self, _repo_root: &ExecutionEnvironmentPath) -> Result<Vec<String>, String> {
        self.result.clone()
    }

    async fn commit_log(&self, _repo_root: &ExecutionEnvironmentPath, _branch: &str, _limit: usize) -> Result<Vec<CommitInfo>, String> {
        Ok(vec![])
    }

    async fn ahead_behind(&self, _repo_root: &ExecutionEnvironmentPath, _branch: &str, _reference: &str) -> Result<AheadBehind, String> {
        Ok(AheadBehind { ahead: 0, behind: 0 })
    }

    async fn working_tree_status(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        _checkout_path: &ExecutionEnvironmentPath,
    ) -> Result<WorkingTreeStatus, String> {
        Ok(WorkingTreeStatus::default())
    }
}

struct MockWorkspaceManager {
    result: Result<Vec<(String, Workspace)>, String>,
}

impl MockWorkspaceManager {
    fn ok(workspaces: Vec<(String, Workspace)>) -> Self {
        Self { result: Ok(workspaces) }
    }

    fn failing(msg: &str) -> Self {
        Self { result: Err(msg.to_string()) }
    }
}

#[async_trait]
impl PresentationManager for MockWorkspaceManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        self.result.clone()
    }

    async fn create_workspace(&self, _config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        Err("not implemented".to_string())
    }

    async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
        Ok(())
    }
    async fn delete_workspace(&self, _ws_ref: &str) -> Result<(), String> {
        Ok(())
    }
    fn binding_scope_prefix(&self) -> String {
        String::new()
    }
}

struct MockTerminalPool {
    result: Result<Vec<crate::providers::terminal::TerminalSession>, String>,
}

impl MockTerminalPool {
    fn ok(sessions: Vec<crate::providers::terminal::TerminalSession>) -> Self {
        Self { result: Ok(sessions) }
    }
}

#[async_trait]
impl TerminalPool for MockTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<crate::providers::terminal::TerminalSession>, String> {
        self.result.clone()
    }

    async fn ensure_session(
        &self,
        _session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &crate::providers::terminal::TerminalEnvVars,
    ) -> Result<(), String> {
        Ok(())
    }

    fn attach_args(
        &self,
        _session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &crate::providers::terminal::TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        Ok(vec![flotilla_protocol::arg::Arg::Literal("mock attach".into())])
    }

    async fn kill_session(&self, _session_name: &str) -> Result<(), String> {
        Ok(())
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from("/tmp/test-repo")
}

fn criteria() -> RepoCriteria {
    RepoCriteria::default()
}

fn make_workspace(name: &str) -> Workspace {
    Workspace { name: name.to_string(), correlation_keys: vec![], attachable_set_id: None }
}

fn test_attachable_store() -> SharedAttachableStore {
    crate::attachable::shared_in_memory_attachable_store()
}

fn test_agent_state_store() -> crate::agents::SharedAgentStateStore {
    crate::agents::shared_in_memory_agent_state_store()
}

fn refresh_error(category: &'static str) -> RefreshError {
    RefreshError { category, provider: String::new(), message: format!("{category} failure") }
}

async fn wait_for_snapshot(rx: &mut tokio::sync::watch::Receiver<Arc<RefreshSnapshot>>) -> Arc<RefreshSnapshot> {
    tokio::time::timeout(Duration::from_secs(2), rx.changed())
        .await
        .expect("timed out waiting for snapshot")
        .expect("snapshot channel closed");
    rx.borrow().clone()
}

struct CountingCloudAgent(AtomicUsize);

#[async_trait]
impl CloudAgentService for CountingCloudAgent {
    async fn list_sessions(&self, _criteria: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }

    async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn attach_command(&self, _session_id: &str) -> Result<String, String> {
        Ok(String::new())
    }
}

struct CountingWorkspaceManager(AtomicUsize);

#[async_trait]
impl PresentationManager for CountingWorkspaceManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }

    async fn create_workspace(&self, _config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        Err("not implemented".into())
    }

    async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
        Ok(())
    }

    async fn delete_workspace(&self, _ws_ref: &str) -> Result<(), String> {
        Ok(())
    }

    fn binding_scope_prefix(&self) -> String {
        String::new()
    }
}

struct BlockingFirstWorkspaceManager {
    calls: AtomicUsize,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl PresentationManager for BlockingFirstWorkspaceManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(vec![])
    }

    async fn create_workspace(&self, _config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        Err("not implemented".into())
    }

    async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
        Ok(())
    }

    async fn delete_workspace(&self, _ws_ref: &str) -> Result<(), String> {
        Ok(())
    }

    fn binding_scope_prefix(&self) -> String {
        String::new()
    }
}

#[tokio::test]
async fn trigger_during_initial_scan_runs_a_follow_up_refresh() {
    let manager = Arc::new(BlockingFirstWorkspaceManager {
        calls: AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut registry = ProviderRegistry::new();
    registry.presentation_managers.insert("workspace", desc("Workspace"), manager.clone());
    let _handle = RepoRefreshHandle::spawn(
        repo_root(),
        Arc::new(registry),
        criteria(),
        None,
        None,
        test_attachable_store(),
        test_agent_state_store(),
        Duration::from_secs(60),
    );

    manager.started.notified().await;
    _handle.trigger_refresh();
    manager.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("trigger received during the initial scan should remain pending");
}

struct BarrierCheckoutManager(Arc<tokio::sync::Barrier>);

#[async_trait]
impl CheckoutManager for BarrierCheckoutManager {
    async fn validate_target(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        _branch: &str,
        _intent: flotilla_protocol::CheckoutIntent,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn list_checkouts(&self, _repo_root: &ExecutionEnvironmentPath) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        self.0.wait().await;
        Ok(vec![])
    }

    async fn create_checkout(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        _branch: &str,
        _create_branch: bool,
    ) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        Err("not implemented".to_string())
    }

    async fn remove_checkout(&self, _repo_root: &ExecutionEnvironmentPath, _branch: &str) -> Result<(), String> {
        Err("not implemented".to_string())
    }
}

struct BarrierCloudAgent(Arc<tokio::sync::Barrier>);

#[async_trait]
impl CloudAgentService for BarrierCloudAgent {
    async fn list_sessions(&self, _criteria: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
        self.0.wait().await;
        Ok(vec![])
    }

    async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn attach_command(&self, _session_id: &str) -> Result<String, String> {
        Ok(String::new())
    }
}

#[tokio::test]
async fn full_refresh_runs_fast_and_slow_tiers_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut registry = ProviderRegistry::new();
    registry.checkout_managers.insert("checkout", desc("Checkout"), Arc::new(BarrierCheckoutManager(Arc::clone(&barrier))));
    registry.cloud_agents.insert("cloud", desc("Cloud"), Arc::new(BarrierCloudAgent(Arc::clone(&barrier))));

    let handle = RepoRefreshHandle::spawn(
        repo_root(),
        Arc::new(registry),
        criteria(),
        None,
        None,
        test_attachable_store(),
        test_agent_state_store(),
        Duration::from_secs(60),
    );
    let mut rx = handle.snapshot_rx.clone();

    tokio::time::timeout(Duration::from_secs(2), barrier.wait())
        .await
        .expect("fast and slow providers should both start before either finishes");
    wait_for_snapshot(&mut rx).await;
}

#[tokio::test(start_paused = true)]
async fn periodic_refresh_uses_fast_and_slow_provider_cadences() {
    let cloud_agent = Arc::new(CountingCloudAgent(AtomicUsize::new(0)));
    let workspace_manager = Arc::new(CountingWorkspaceManager(AtomicUsize::new(0)));
    let mut registry = ProviderRegistry::new();
    registry.cloud_agents.insert("cloud", desc("Cloud"), cloud_agent.clone());
    registry.presentation_managers.insert("workspace", desc("Workspace"), workspace_manager.clone());

    let handle = RepoRefreshHandle::spawn_with_schedule(
        repo_root(),
        Arc::new(registry),
        criteria(),
        None,
        None,
        test_attachable_store(),
        test_agent_state_store(),
        RefreshSchedule::without_stagger(Duration::from_secs(10), Duration::from_secs(60)),
    );
    let mut rx = handle.snapshot_rx.clone();
    rx.changed().await.expect("initial full refresh");
    assert_eq!(workspace_manager.0.load(Ordering::SeqCst), 1);
    assert_eq!(cloud_agent.0.load(Ordering::SeqCst), 1);

    tokio::time::advance(Duration::from_secs(10)).await;
    rx.changed().await.expect("fast refresh");
    assert_eq!(workspace_manager.0.load(Ordering::SeqCst), 2);
    assert_eq!(cloud_agent.0.load(Ordering::SeqCst), 1, "cloud agents should wait for the slow cadence");

    handle.trigger_refresh();
    rx.changed().await.expect("manual full refresh");
    assert_eq!(workspace_manager.0.load(Ordering::SeqCst), 3);
    assert_eq!(cloud_agent.0.load(Ordering::SeqCst), 2, "manual refresh should include slow providers");
}

#[tokio::test(start_paused = true)]
async fn staggered_schedule_delays_the_initial_full_refresh() {
    let workspace_manager = Arc::new(CountingWorkspaceManager(AtomicUsize::new(0)));
    let mut registry = ProviderRegistry::new();
    registry.presentation_managers.insert("workspace", desc("Workspace"), workspace_manager.clone());
    let schedule = RefreshSchedule {
        startup_offset: Duration::from_secs(3),
        fast: RefreshCadence { interval: Duration::from_secs(10), offset: Duration::from_secs(3) },
        slow: RefreshCadence { interval: Duration::from_secs(60), offset: Duration::from_secs(20) },
    };

    let handle = RepoRefreshHandle::spawn_with_schedule(
        repo_root(),
        Arc::new(registry),
        criteria(),
        None,
        None,
        test_attachable_store(),
        test_agent_state_store(),
        schedule,
    );
    let mut rx = handle.snapshot_rx.clone();
    handle.trigger_refresh();
    tokio::task::yield_now().await;
    assert!(!rx.has_changed().expect("refresh sender alive"));

    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(!rx.has_changed().expect("refresh sender alive"));

    tokio::time::advance(Duration::from_secs(1)).await;
    rx.changed().await.expect("staggered initial refresh");
    tokio::task::yield_now().await;
    assert_eq!(workspace_manager.0.load(Ordering::SeqCst), 1);
}

#[test]
fn repo_refresh_schedules_have_stable_distinct_offsets() {
    let fast = Duration::from_secs(10);
    let slow = Duration::from_secs(60);
    let first = RefreshSchedule::for_repo(Path::new("/repos/first"), fast, slow);
    let first_again = RefreshSchedule::for_repo(Path::new("/repos/first"), fast, slow);
    let second = RefreshSchedule::for_repo(Path::new("/repos/second"), fast, slow);

    assert_eq!(first.fast.offset, first_again.fast.offset);
    assert_eq!(first.slow.offset, first_again.slow.offset);
    assert_eq!(first.startup_offset, first_again.startup_offset);
    assert_ne!(first.fast.offset, second.fast.offset);
    assert_ne!(first.slow.offset, second.slow.offset);
    assert_ne!(first.startup_offset, second.startup_offset);
    assert!(first.startup_offset < Duration::from_secs(1));
    assert!(first.fast.offset < fast);
    assert!(first.slow.offset < slow);
}

#[tokio::test(start_paused = true)]
async fn fast_refresh_preserves_slow_provider_errors_and_health() {
    let mut registry = ProviderRegistry::new();
    registry.cloud_agents.insert("cloud", desc("Cloud"), Arc::new(MockCloudAgent::failing("offline")));
    let handle = RepoRefreshHandle::spawn_with_schedule(
        repo_root(),
        Arc::new(registry),
        criteria(),
        None,
        None,
        test_attachable_store(),
        test_agent_state_store(),
        RefreshSchedule::without_stagger(Duration::from_secs(10), Duration::from_secs(60)),
    );
    let mut rx = handle.snapshot_rx.clone();
    rx.changed().await.expect("initial full refresh");

    tokio::time::advance(Duration::from_secs(10)).await;
    rx.changed().await.expect("fast refresh");
    let snapshot = rx.borrow().clone();

    assert!(snapshot.errors.iter().any(|error| error.category == "sessions" && error.message == "offline"));
    assert_eq!(snapshot.provider_health.get(&("cloud_agent", "Cloud".into())), Some(&false));
}

#[test]
fn refresh_snapshot_default_is_empty() {
    let snap = RefreshSnapshot::default();
    assert!(snap.work_items.is_empty());
    assert!(snap.correlation_groups.is_empty());
    assert!(snap.errors.is_empty());
    assert!(snap.provider_health.is_empty());
    assert!(snap.providers.checkouts.is_empty());
    assert!(snap.providers.change_requests.is_empty());
    assert!(snap.providers.sessions.is_empty());
    assert!(snap.providers.branches.is_empty());
    assert!(snap.providers.workspaces.is_empty());
}

#[test]
fn compute_provider_health_empty_registry_returns_empty() {
    let health = compute_provider_health(&ProviderRegistry::new(), &[]);
    assert!(health.is_empty());
}

fn refresh_error_for(category: &'static str, provider: &str) -> RefreshError {
    RefreshError { category, provider: provider.to_string(), message: format!("{category} failure") }
}

#[test]
fn compute_provider_health_maps_error_categories() {
    let mut registry = ProviderRegistry::new();
    registry.cloud_agents.insert("claude", desc("MockCA"), Arc::new(MockCloudAgent::ok(vec![])));
    registry.change_requests.insert("github", desc("MockCR"), Arc::new(MockChangeRequestTracker::ok(vec![], vec![])));

    let cases = vec![
        (vec![], true, true),
        (vec![refresh_error_for("sessions", "MockCA")], false, true),
        (vec![refresh_error_for("PRs", "MockCR")], true, false),
        (vec![refresh_error_for("merged", "MockCR")], true, false),
        (vec![refresh_error("checkouts")], true, true),
        (vec![refresh_error_for("sessions", "MockCA"), refresh_error_for("PRs", "MockCR")], false, false),
    ];

    for (errors, expected_coding, expected_review) in cases {
        let health = compute_provider_health(&registry, &errors);
        assert_eq!(
            health.get(&("cloud_agent", "MockCA".to_string())),
            Some(&expected_coding),
            "cloud_agent health mismatch for errors: {errors:?}"
        );
        assert_eq!(
            health.get(&("change_request", "MockCR".to_string())),
            Some(&expected_review),
            "change_request health mismatch for errors: {errors:?}"
        );
    }
}

#[tokio::test]
async fn refresh_empty_registry_produces_empty_data() {
    let mut pd = ProviderData::default();
    let errors = refresh_providers(
        &mut pd,
        &repo_root(),
        &ProviderRegistry::new(),
        &criteria(),
        None,
        None,
        &test_attachable_store(),
        &test_agent_state_store(),
    )
    .await;

    assert!(errors.is_empty());
    assert!(pd.checkouts.is_empty());
    assert!(pd.change_requests.is_empty());
    assert!(pd.sessions.is_empty());
    assert!(pd.branches.is_empty());
    assert!(pd.workspaces.is_empty());
}

#[tokio::test]
async fn refresh_populates_all_provider_data_and_merged_wins_branch_conflict() {
    use flotilla_protocol::delta::BranchStatus;

    let mut registry = ProviderRegistry::new();
    registry.checkout_managers.insert(
        "wt",
        desc("wt"),
        Arc::new(MockCheckoutManager::ok(vec![(PathBuf::from("/tmp/wt/feat-a"), TestCheckout::new("feat-a").with_branch_key().build())])),
    );
    registry.change_requests.insert(
        "github",
        desc("github"),
        Arc::new(MockChangeRequestTracker::ok(
            vec![("42".to_string(), TestChangeRequest::new("Add feature", "feat-a").with_branch_key().build())],
            vec!["shared".to_string()],
        )),
    );
    registry.cloud_agents.insert(
        "claude",
        desc("claude"),
        Arc::new(MockCloudAgent::ok(vec![("sess-1".to_string(), TestSession::new("Debug").with_session_ref("mock", "sess-1").build())])),
    );
    registry.vcs.insert("git", desc("git"), Arc::new(MockVcs::ok(vec!["remote-only".to_string(), "shared".to_string()])));
    registry.presentation_managers.insert(
        "cmux",
        desc("cmux"),
        Arc::new(MockWorkspaceManager::ok(vec![("ws-1".to_string(), make_workspace("dev"))])),
    );

    let mut pd = ProviderData::default();
    let errors =
        refresh_providers(&mut pd, &repo_root(), &registry, &criteria(), None, None, &test_attachable_store(), &test_agent_state_store())
            .await;

    assert!(errors.is_empty());
    assert_eq!(pd.checkouts.len(), 1);
    assert_eq!(pd.change_requests.len(), 1);
    assert_eq!(pd.sessions.len(), 1);
    assert_eq!(pd.workspaces.len(), 1);
    assert_eq!(pd.branches.len(), 2);
    assert_eq!(pd.branches.get("remote-only").unwrap().status, BranchStatus::Remote);
    assert_eq!(pd.branches.get("shared").unwrap().status, BranchStatus::Merged);
}

#[tokio::test]
async fn refresh_uses_host_id_for_local_checkout_publication_when_available() {
    let host_id = HostId::new("host-123");
    let mut registry = ProviderRegistry::new();
    registry.checkout_managers.insert(
        "wt",
        desc("wt"),
        Arc::new(MockCheckoutManager::ok(vec![(PathBuf::from("/tmp/wt/feat-a"), TestCheckout::new("feat-a").with_branch_key().build())])),
    );

    let handle = RepoRefreshHandle::spawn(
        repo_root(),
        Arc::new(registry),
        criteria(),
        None,
        Some(host_id.clone()),
        test_attachable_store(),
        test_agent_state_store(),
        Duration::from_secs(3600),
    );

    let mut rx = handle.snapshot_rx.clone();
    let snapshot = wait_for_snapshot(&mut rx).await;
    let expected = QualifiedPath::host(host_id.clone(), "/tmp/wt/feat-a");
    assert!(snapshot.providers.checkouts.contains_key(&expected));
    assert!(
        !snapshot.providers.checkouts.contains_key(&QualifiedPath::from_host_name(&flotilla_protocol::HostName::local(), "/tmp/wt/feat-a")),
        "local publication should prefer the real host id when available"
    );
}

#[test]
fn project_attachable_data_populates_sets_and_ids() {
    let mut registry = ProviderRegistry::new();
    registry.presentation_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::ok(vec![])));
    registry.terminal_pools.insert("shpool", desc("shpool"), Arc::new(MockTerminalPool::ok(vec![])));

    let store_dir = tempfile::tempdir().expect("tempdir");
    let attachable_store =
        crate::attachable::shared_file_backed_attachable_store(&crate::path_context::DaemonHostPath::new(store_dir.path()));
    let (set_id, attachable_id) = {
        let mut store = attachable_store.lock().expect("lock store");
        let set_id = store.ensure_terminal_set(
            Some(flotilla_protocol::HostName::local()),
            Some(flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), PathBuf::from("/tmp/wt-feat")).into()),
        );
        let attachable_id = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/dev/0",
            crate::attachable::TerminalPurpose { checkout: "feat".into(), role: "dev".into(), index: 0 },
            "bash",
            crate::path_context::ExecutionEnvironmentPath::new("/tmp/wt-feat"),
            flotilla_protocol::TerminalStatus::Running,
        );
        store.replace_binding(crate::attachable::ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: "cmux".into(),
            object_kind: crate::attachable::BindingObjectKind::AttachableSet,
            object_id: set_id.to_string(),
            external_ref: "ws-1".into(),
        });
        (set_id, attachable_id)
    };

    let mut pd = ProviderData::default();
    pd.checkouts.insert(
        flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), PathBuf::from("/tmp/wt-feat")).into(),
        Checkout {
            branch: "feat".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
            host_name: None,
            environment_id: None,
        },
    );
    pd.workspaces.insert("ws-1".into(), make_workspace("dev"));

    let local_host = flotilla_protocol::HostName::local();
    project_attachable_data(&mut pd, &registry, &attachable_store, &local_host, None);

    assert_eq!(pd.attachable_sets.len(), 1);
    assert!(pd.attachable_sets.contains_key(&set_id));
    assert!(pd.managed_terminals.contains_key(&attachable_id));
    assert_eq!(pd.workspaces.get("ws-1").and_then(|ws| ws.attachable_set_id.as_ref()), Some(&set_id));

    attachable_store.lock().expect("lock store").remove_set(&set_id);
    project_attachable_data(&mut pd, &registry, &attachable_store, &local_host, None);
    assert!(pd.managed_terminals.is_empty(), "removed terminals must not survive the next projection");
}

#[test]
fn project_agent_data_removes_agents_missing_from_the_store() {
    let attachable_store = test_attachable_store();
    let agent_store = test_agent_state_store();
    let host = flotilla_protocol::HostName::local();
    let checkout = flotilla_protocol::HostPath::new(host.clone(), "/repo/wt-feat");
    let (set_id, attachable_id) = {
        let mut store = attachable_store.lock().expect("lock attachable store");
        let set_id = store.ensure_terminal_set(Some(host.clone()), Some(checkout.clone().into()));
        let attachable_id = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "test",
            "agent",
            crate::attachable::TerminalPurpose { checkout: "feat".into(), role: "agent".into(), index: 0 },
            "claude",
            ExecutionEnvironmentPath::new("/repo/wt-feat"),
            flotilla_protocol::TerminalStatus::Running,
        );
        (set_id, attachable_id)
    };
    agent_store.lock().expect("lock agent store").upsert(attachable_id.clone(), crate::agents::AgentEntry {
        harness: flotilla_protocol::AgentHarness::ClaudeCode,
        status: flotilla_protocol::AgentStatus::Active,
        model: None,
        session_title: None,
        session_id: None,
        last_event_epoch_secs: 0,
    });
    let mut pd = ProviderData::default();
    pd.checkouts.insert(checkout.into(), TestCheckout::new("feat").build());

    project_attachable_data(&mut pd, &ProviderRegistry::new(), &attachable_store, &host, None);
    project_agent_data(&mut pd, &agent_store);
    assert!(pd.attachable_sets.contains_key(&set_id));
    assert!(pd.agents.contains_key(attachable_id.as_str()));

    agent_store.lock().expect("lock agent store").remove(&attachable_id);
    project_agent_data(&mut pd, &agent_store);
    assert!(pd.agents.is_empty(), "removed agents must not survive the next projection");
}

#[tokio::test]
async fn refresh_reports_checkout_errors() {
    let mut registry = ProviderRegistry::new();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("checkout failed")));

    let mut pd = ProviderData::default();
    let errors =
        refresh_providers(&mut pd, &repo_root(), &registry, &criteria(), None, None, &test_attachable_store(), &test_agent_state_store())
            .await;

    assert!(errors.iter().any(|e| e.category == "checkouts"));
    assert!(pd.checkouts.is_empty());
}

#[tokio::test]
async fn refresh_collects_multiple_errors_and_preserves_successful_providers() {
    let mut registry = ProviderRegistry::new();
    registry.checkout_managers.insert(
        "wt",
        desc("wt"),
        Arc::new(MockCheckoutManager::ok(vec![(PathBuf::from("/tmp/wt/feat-a"), TestCheckout::new("feat-a").with_branch_key().build())])),
    );
    registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker::failing("pr fail", "merged fail")));
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::failing("sessions fail")));
    registry.vcs.insert("git", desc("git"), Arc::new(MockVcs::failing("branches fail")));
    registry.presentation_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("workspaces fail")));

    let mut pd = ProviderData::default();
    let errors =
        refresh_providers(&mut pd, &repo_root(), &registry, &criteria(), None, None, &test_attachable_store(), &test_agent_state_store())
            .await;

    let categories: HashSet<&str> = errors.iter().map(|e| e.category).collect();
    for expected in ["PRs", "merged", "sessions", "branches", "workspaces"] {
        assert!(categories.contains(expected), "missing error category: {expected}");
    }

    assert_eq!(pd.checkouts.len(), 1);
    assert!(pd.change_requests.is_empty());
    assert!(pd.sessions.is_empty());
    assert!(pd.branches.is_empty());
    assert!(pd.workspaces.is_empty());
}

#[tokio::test]
async fn spawn_produces_initial_snapshot() {
    let handle = RepoRefreshHandle::spawn(
        repo_root(),
        Arc::new(ProviderRegistry::new()),
        criteria(),
        None,
        None,
        test_attachable_store(),
        test_agent_state_store(),
        Duration::from_secs(3600),
    );

    let mut rx = handle.snapshot_rx.clone();
    let snapshot = wait_for_snapshot(&mut rx).await;
    assert!(snapshot.errors.is_empty());
    assert!(snapshot.work_items.is_empty());
    assert!(snapshot.provider_health.is_empty());
}

#[tokio::test]
async fn spawn_with_failing_provider_sets_error_and_unhealthy_health() {
    let mut registry = ProviderRegistry::new();
    registry.cloud_agents.insert("claude", desc("MockCA"), Arc::new(MockCloudAgent::failing("agent offline")));

    let handle = RepoRefreshHandle::spawn(
        repo_root(),
        Arc::new(registry),
        criteria(),
        None,
        None,
        test_attachable_store(),
        test_agent_state_store(),
        Duration::from_secs(3600),
    );

    let mut rx = handle.snapshot_rx.clone();
    let snapshot = wait_for_snapshot(&mut rx).await;
    assert!(snapshot.errors.iter().any(|e| e.category == "sessions"));
    assert_eq!(snapshot.provider_health.get(&("cloud_agent", "MockCA".to_string())), Some(&false));
}

#[tokio::test]
async fn trigger_refresh_produces_another_snapshot() {
    let handle = RepoRefreshHandle::spawn(
        repo_root(),
        Arc::new(ProviderRegistry::new()),
        criteria(),
        None,
        None,
        test_attachable_store(),
        test_agent_state_store(),
        Duration::from_secs(3600),
    );

    let mut rx = handle.snapshot_rx.clone();
    wait_for_snapshot(&mut rx).await;

    handle.trigger_refresh();
    let snapshot = wait_for_snapshot(&mut rx).await;
    assert!(snapshot.errors.is_empty());
}

#[test]
fn compute_provider_health_per_provider() {
    let mut registry = ProviderRegistry::new();
    registry.cloud_agents.insert("claude", desc("Claude"), Arc::new(MockCloudAgent::ok_named("Claude", vec![])));
    registry.cloud_agents.insert("cursor", desc("Cursor"), Arc::new(MockCloudAgent::ok_named("Cursor", vec![])));

    // Only Cursor fails
    let errors = vec![RefreshError { category: "sessions", provider: "Cursor".to_string(), message: "auth failed".to_string() }];

    let health = compute_provider_health(&registry, &errors);
    assert_eq!(health.get(&("cloud_agent", "Claude".to_string())), Some(&true));
    assert_eq!(health.get(&("cloud_agent", "Cursor".to_string())), Some(&false));
}

#[tokio::test]
async fn spawn_with_mixed_provider_health_isolates_failures() {
    let mut registry = ProviderRegistry::new();
    registry.cloud_agents.insert("claude", desc("Claude"), Arc::new(MockCloudAgent::ok_named("Claude", vec![])));
    registry.cloud_agents.insert("cursor", desc("Cursor"), Arc::new(MockCloudAgent::failing_named("Cursor", "auth failed")));

    let handle = RepoRefreshHandle::spawn(
        repo_root(),
        Arc::new(registry),
        criteria(),
        None,
        None,
        test_attachable_store(),
        test_agent_state_store(),
        Duration::from_secs(3600),
    );

    let mut rx = handle.snapshot_rx.clone();
    let snapshot = wait_for_snapshot(&mut rx).await;

    assert!(snapshot.errors.iter().any(|e| e.provider == "Cursor"));
    assert_eq!(snapshot.provider_health.get(&("cloud_agent", "Claude".to_string())), Some(&true));
    assert_eq!(snapshot.provider_health.get(&("cloud_agent", "Cursor".to_string())), Some(&false));
}

#[test]
fn project_attachable_data_only_includes_sets_matching_repo_checkouts() {
    let store = crate::attachable::shared_in_memory_attachable_store();
    let host = flotilla_protocol::HostName::local();
    let checkout_a = flotilla_protocol::HostPath::new(host.clone(), "/repo/wt-feat");
    let checkout_b = flotilla_protocol::HostPath::new(host.clone(), "/repo/wt-other");

    {
        let mut s = store.lock().expect("lock");
        s.ensure_terminal_set(Some(host.clone()), Some(checkout_a.clone().into()));
        s.ensure_terminal_set(Some(host.clone()), Some(checkout_b.clone().into()));
    }

    let mut pd = ProviderData::default();
    pd.checkouts.insert(checkout_a.clone().into(), Checkout {
        branch: "feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        host_name: None,
        environment_id: None,
    });

    let registry = ProviderRegistry::new();
    project_attachable_data(&mut pd, &registry, &store, &host, None);

    assert_eq!(pd.attachable_sets.len(), 1);
    let set = pd.attachable_sets.values().next().expect("one set");
    assert_eq!(set.checkout, Some(checkout_a.into()));
}

#[test]
fn project_attachable_data_set_appears_without_terminal_scan() {
    let store = crate::attachable::shared_in_memory_attachable_store();
    let host = flotilla_protocol::HostName::local();
    let checkout = flotilla_protocol::HostPath::new(host.clone(), "/repo/wt-feat");

    {
        let mut s = store.lock().expect("lock");
        s.ensure_terminal_set(Some(host.clone()), Some(checkout.clone().into()));
    }

    let mut pd = ProviderData::default();
    pd.checkouts.insert(checkout.clone().into(), Checkout {
        branch: "feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        host_name: None,
        environment_id: None,
    });

    let registry = ProviderRegistry::new();
    project_attachable_data(&mut pd, &registry, &store, &host, None);

    assert_eq!(pd.attachable_sets.len(), 1, "set should appear without terminal scan");
}

#[test]
fn project_attachable_data_infers_checkout_only_in_the_attachable_set_namespace() {
    let store = crate::attachable::shared_in_memory_attachable_store();
    let local_host = flotilla_protocol::HostName::new("local");
    let local_host_id = HostId::new("local-id");
    let remote_host = flotilla_protocol::HostName::new("remote");
    let checkout_path = PathBuf::from("/workspace");
    let set_id = {
        let mut store = store.lock().expect("lock");
        let set_id = store.ensure_terminal_set(Some(local_host.clone()), None);
        store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "discovered-session",
            crate::attachable::TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "bash",
            ExecutionEnvironmentPath::new(&checkout_path),
            flotilla_protocol::TerminalStatus::Running,
        );
        set_id
    };

    let remote_checkout = QualifiedPath::from_host_name(&remote_host, &checkout_path);
    let local_checkout = QualifiedPath::host(local_host_id.clone(), &checkout_path);
    let mut pd = ProviderData::default();
    pd.checkouts.insert(remote_checkout, TestCheckout::new("remote").build());

    project_attachable_data(&mut pd, &ProviderRegistry::new(), &store, &local_host, Some(&local_host_id));
    assert_eq!(store.lock().expect("lock").registry().sets[&set_id].checkout, None);

    pd.checkouts.insert(local_checkout.clone(), TestCheckout::new("local").build());
    project_attachable_data(&mut pd, &ProviderRegistry::new(), &store, &local_host, Some(&local_host_id));
    assert_eq!(store.lock().expect("lock").registry().sets[&set_id].checkout.as_ref(), Some(&local_checkout));
}

#[tokio::test]
async fn refresh_correlates_discovered_attachable_set_from_member_working_directories() {
    let checkout_path = PathBuf::from("/repo/wt-feat");
    let host = flotilla_protocol::HostName::local();
    let checkout = flotilla_protocol::HostPath::new(host.clone(), checkout_path.clone());
    let checkout_key: QualifiedPath = checkout.clone().into();
    let store = crate::attachable::shared_in_memory_attachable_store();
    let set_id = flotilla_protocol::AttachableSetId::new("discovered-set");
    let session_name = "flotilla-v2:discovered-set:discovered-terminal:feat:shell:0:%2Frepo%2Fwt-feat";

    let mut registry = ProviderRegistry::new();
    let mut checkout_data = TestCheckout::new("feat").build();
    checkout_data.correlation_keys = vec![flotilla_protocol::CorrelationKey::CheckoutPath(checkout_key)];
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::ok(vec![(checkout_path, checkout_data)])));
    registry.terminal_pools.insert(
        "shpool",
        desc("shpool"),
        Arc::new(MockTerminalPool::ok(vec![crate::providers::terminal::TerminalSession {
            session_name: session_name.into(),
            status: flotilla_protocol::TerminalStatus::Running,
            command: None,
            working_directory: None,
        }])),
    );
    let handle = RepoRefreshHandle::spawn(
        repo_root(),
        Arc::new(registry),
        criteria(),
        None,
        None,
        store,
        test_agent_state_store(),
        Duration::from_secs(3600),
    );

    let mut snapshots = handle.snapshot_rx.clone();
    let snapshot = wait_for_snapshot(&mut snapshots).await;

    assert_eq!(snapshot.providers.attachable_sets.get(&set_id).and_then(|set| set.checkout.as_ref()), Some(&checkout.into()));
    assert_eq!(snapshot.work_items.len(), 1, "checkout and discovered set should form one correlated work item");
    assert_eq!(snapshot.work_items[0].attachable_set_id(), Some(&set_id));
}
