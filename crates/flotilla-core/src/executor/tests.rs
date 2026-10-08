use std::{path::PathBuf, sync::Arc};

use super::{
    build_plan,
    checkout::{resolve_checkout_branch, CheckoutIntent, CheckoutResolutionScope, CheckoutService},
    ExecutorStepResolver, PlannerRefusal, RepoExecutionContext,
};
use crate::providers::environment::{EnvironmentKind, PrepareOpts, PreparedEnvironment, ProvisionOpts};
use crate::{
    environment_manager::EnvironmentManager,
    event_sink::RecordingEventSink,
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    provider_data::ProviderData,
    providers::{
        ai_utility::AiUtility,
        change_request::ChangeRequestTracker,
        coding_agent::CloudAgentService,
        discovery::{
            test_support::{fake_discovery, test_vcs_resolver, DiscoveryMockRunner, TestEnvVars},
            EnvironmentBag, ProviderCategory, ProviderDescriptor,
        },
        environment::ProvisionedMount,
        issue_tracker::IssueProvider,
        presentation::PresentationManager,
        registry::ProviderRegistry,
        terminal::{TerminalEnvVars, TerminalPool, TerminalSession, TerminalSessionTag},
        testing::MockRunner,
        types::*,
        vcs::write_branch_issue_links,
        CommandRunner,
    },
    step::{StepAction, StepExecutionContext, StepOutcome, StepResolver},
    vcs::{EnumeratedCheckout, Vcs},
};

fn desc(name: &str) -> ProviderDescriptor {
    ProviderDescriptor::named(ProviderCategory::Vcs, name)
}
use async_trait::async_trait;
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    qualified_path::HostId,
    test_support::{TestCheckout, TestIssue, TestSession},
    CheckoutSelector, CheckoutTarget, Command, CommandAction, CommandValue, HostName, HostPath, IssueChangeset, IssueRef, IssueSource,
    NodeId, RepoSelector,
};

fn hp(path: &str) -> HostPath {
    HostPath::new(HostName::local(), PathBuf::from(path))
}

// -----------------------------------------------------------------------
// Mock providers
// -----------------------------------------------------------------------

/// A mock CheckoutManager that returns a canned checkout or error.
struct MockCheckoutManager {
    validate_result: tokio::sync::Mutex<Option<Result<(), String>>>,
    create_result: tokio::sync::Mutex<Option<Result<(PathBuf, Checkout), String>>>,
    remove_result: tokio::sync::Mutex<Option<Result<(), String>>>,
}

impl MockCheckoutManager {
    fn succeeding(branch: &str, path: &str) -> Self {
        Self {
            validate_result: tokio::sync::Mutex::new(Some(Ok(()))),
            create_result: tokio::sync::Mutex::new(Some(Ok((
                PathBuf::from(path),
                Checkout {
                    branch: branch.to_string(),
                    is_main: false,
                    trunk_ahead_behind: None,
                    remote_ahead_behind: None,
                    working_tree: None,
                    last_commit: None,
                    host_name: None,
                    environment_id: None,
                },
            )))),
            remove_result: tokio::sync::Mutex::new(Some(Ok(()))),
        }
    }

    fn failing(msg: &str) -> Self {
        Self {
            validate_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
            create_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
            remove_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
        }
    }
}

#[async_trait]
impl Vcs for MockCheckoutManager {
    async fn validate_target(&self, _branch: &str, _intent: CheckoutIntent) -> Result<(), String> {
        self.validate_result.lock().await.take().expect("validate_target called more than expected")
    }

    async fn enumerate_checkouts(&self) -> Result<Vec<EnumeratedCheckout>, String> {
        self.list_checkouts().await.map(|checkouts| checkouts.into_iter().map(EnumeratedCheckout::from).collect())
    }

    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        Ok(vec![])
    }
    async fn create_checkout(&self, _branch: &str, _create_branch: bool) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        self.create_result
            .lock()
            .await
            .take()
            .expect("create_checkout called more than expected")
            .map(|(p, co)| (ExecutionEnvironmentPath::new(p), co))
    }
    async fn remove_checkout(&self, _branch: &str) -> Result<(), String> {
        self.remove_result.lock().await.take().expect("remove_checkout called more than expected")
    }
}

/// A mock WorkspaceManager that records calls and returns configurable results.
struct MockWorkspaceManager {
    existing: Vec<(String, Workspace)>,
    create_result: tokio::sync::Mutex<Result<(), String>>,
    select_result: tokio::sync::Mutex<Result<(), String>>,
    created_configs: tokio::sync::Mutex<Vec<WorkspaceAttachRequest>>,
    calls: tokio::sync::Mutex<Vec<String>>,
}

impl MockWorkspaceManager {
    fn succeeding() -> Self {
        Self {
            existing: vec![],
            create_result: tokio::sync::Mutex::new(Ok(())),
            select_result: tokio::sync::Mutex::new(Ok(())),
            created_configs: tokio::sync::Mutex::new(Vec::new()),
            calls: tokio::sync::Mutex::new(vec![]),
        }
    }

    fn failing(msg: &str) -> Self {
        Self {
            existing: vec![],
            create_result: tokio::sync::Mutex::new(Err(msg.to_string())),
            select_result: tokio::sync::Mutex::new(Err(msg.to_string())),
            created_configs: tokio::sync::Mutex::new(Vec::new()),
            calls: tokio::sync::Mutex::new(vec![]),
        }
    }

    fn with_existing(existing: Vec<(String, Workspace)>) -> Self {
        Self {
            existing,
            create_result: tokio::sync::Mutex::new(Ok(())),
            select_result: tokio::sync::Mutex::new(Ok(())),
            created_configs: tokio::sync::Mutex::new(Vec::new()),
            calls: tokio::sync::Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl PresentationManager for MockWorkspaceManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        self.calls.lock().await.push("list_workspaces".to_string());
        Ok(self.existing.clone())
    }
    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        self.created_configs.lock().await.push(config.clone());
        self.calls.lock().await.push(format!("create_workspace:{}", config.name));
        let result = self.create_result.lock().await;
        match &*result {
            Ok(()) => Ok(("mock-ref".to_string(), Workspace { name: config.name.clone() })),
            Err(e) => Err(e.clone()),
        }
    }
    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        self.calls.lock().await.push(format!("select_workspace:{ws_ref}"));
        let result = self.select_result.lock().await;
        result.clone()
    }
    async fn delete_workspace(&self, ws_ref: &str) -> Result<(), String> {
        self.calls.lock().await.push(format!("delete_workspace:{ws_ref}"));
        Ok(())
    }
    fn binding_scope_prefix(&self) -> String {
        String::new()
    }
}

/// A mock ChangeRequestTracker provider.
struct MockChangeRequestTracker;

#[async_trait]
impl ChangeRequestTracker for MockChangeRequestTracker {
    async fn list_change_requests(&self, _limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        Ok(vec![])
    }
    async fn get_change_request(&self, _id: &str) -> Result<(String, ChangeRequest), String> {
        Err("not implemented".to_string())
    }
    async fn open_in_browser(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    async fn close_change_request(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    async fn merge_change_request(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    async fn list_merged_branch_names(&self, _limit: usize) -> Result<Vec<String>, String> {
        Ok(vec![])
    }
}

struct MergeChangeRequestTracker {
    calls: tokio::sync::Mutex<Vec<String>>,
    result: Result<(), String>,
}

#[async_trait]
impl ChangeRequestTracker for MergeChangeRequestTracker {
    async fn list_change_requests(&self, _limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        Ok(vec![])
    }
    async fn get_change_request(&self, _id: &str) -> Result<(String, ChangeRequest), String> {
        Err("not implemented".to_string())
    }
    async fn open_in_browser(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    async fn close_change_request(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    async fn merge_change_request(&self, id: &str) -> Result<(), String> {
        self.calls.lock().await.push(id.to_string());
        self.result.clone()
    }
    async fn list_merged_branch_names(&self, _limit: usize) -> Result<Vec<String>, String> {
        Ok(vec![])
    }
}

/// In-memory fake for the external issue-source API boundary.
struct MockIssueProvider {
    fetched_by_id: tokio::sync::Mutex<Vec<Vec<String>>>,
    fetched_issues: Vec<(String, Issue)>,
}

impl MockIssueProvider {
    fn empty() -> Self {
        Self { fetched_by_id: tokio::sync::Mutex::new(Vec::new()), fetched_issues: Vec::new() }
    }

    fn with_fetched_issues(fetched_issues: Vec<(String, Issue)>) -> Self {
        Self { fetched_by_id: tokio::sync::Mutex::new(Vec::new()), fetched_issues }
    }
}

#[async_trait]
impl IssueProvider for MockIssueProvider {
    fn supports(&self, _source: &IssueSource) -> bool {
        true
    }

    async fn query(&self, _source: &IssueSource, _params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
        Ok(IssueResultPage { items: vec![], total: Some(0), has_more: false })
    }

    async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String> {
        self.fetched_by_id.lock().await.push(vec![reference.id.clone()]);
        self.fetched_issues
            .iter()
            .find(|(id, _)| id == &reference.id)
            .map(|(_, issue)| issue.clone())
            .ok_or_else(|| format!("issue {} not found", reference.id))
    }

    async fn fetch_by_ids(&self, source: &IssueSource, ids: &[String]) -> Result<Vec<Issue>, String> {
        self.fetched_by_id.lock().await.push(ids.to_vec());
        Ok(self
            .fetched_issues
            .iter()
            .filter(|(id, _)| ids.iter().any(|requested| requested == id))
            .map(|(id, issue)| {
                let mut issue = issue.clone();
                issue.reference = IssueRef { source: source.clone(), id: id.clone() };
                issue
            })
            .collect())
    }

    async fn list_changed_since(&self, _source: &IssueSource, _since: &str, _count: usize) -> Result<IssueChangeset, String> {
        Ok(IssueChangeset { updated: vec![], closed: vec![], has_more: false })
    }

    async fn open_in_browser(&self, _reference: &IssueRef) -> Result<(), String> {
        Ok(())
    }
}

/// A mock CloudAgentService provider.
struct MockCloudAgent {
    archive_result: tokio::sync::Mutex<Result<(), String>>,
    attach_command: String,
}

impl MockCloudAgent {
    fn succeeding() -> Self {
        Self { archive_result: tokio::sync::Mutex::new(Ok(())), attach_command: "mock-attach-cmd".to_string() }
    }

    fn failing(msg: &str) -> Self {
        Self { archive_result: tokio::sync::Mutex::new(Err(msg.to_string())), attach_command: "mock-attach-cmd".to_string() }
    }
}

#[async_trait]
impl CloudAgentService for MockCloudAgent {
    async fn list_sessions(&self, _criteria: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
        Ok(vec![])
    }
    async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
        let result = self.archive_result.lock().await;
        result.clone()
    }
    async fn attach_command(&self, session_id: &str) -> Result<String, String> {
        Ok(format!("{} {session_id}", self.attach_command))
    }
}

/// A mock AiUtility provider.
struct MockAiUtility {
    result: tokio::sync::Mutex<Result<String, String>>,
    contexts: tokio::sync::Mutex<Vec<String>>,
}

impl MockAiUtility {
    fn succeeding(name: &str) -> Self {
        Self { result: tokio::sync::Mutex::new(Ok(name.to_string())), contexts: Default::default() }
    }

    fn failing(msg: &str) -> Self {
        Self { result: tokio::sync::Mutex::new(Err(msg.to_string())), contexts: Default::default() }
    }
}

#[async_trait]
impl AiUtility for MockAiUtility {
    async fn generate_branch_name(&self, context: &str) -> Result<String, String> {
        self.contexts.lock().await.push(context.to_string());
        let result = self.result.lock().await;
        result.clone()
    }
}

// -----------------------------------------------------------------------
// Helper to build test fixtures
// -----------------------------------------------------------------------

fn empty_registry() -> ProviderRegistry {
    ProviderRegistry::new()
}

fn empty_data() -> ProviderData {
    ProviderData::default()
}

fn repo_root() -> ExecutionEnvironmentPath {
    ExecutionEnvironmentPath::new("/tmp/test-repo")
}

fn config_base() -> DaemonHostPath {
    DaemonHostPath::new("/tmp/test-config")
}

fn runner_ok() -> MockRunner {
    MockRunner::new(vec![])
}

fn repo_selector() -> RepoSelector {
    RepoSelector::Path(repo_root().into_path_buf())
}

fn local_command(action: CommandAction) -> Command {
    Command::builder().action(action).build()
}

fn command_with_host(host: &str, action: CommandAction) -> Command {
    Command::builder().action(action).node_id(NodeId::new(host)).build()
}

fn local_host() -> HostName {
    HostName::local()
}

fn node_id(name: &str) -> NodeId {
    NodeId::new(name)
}

fn local_node_id() -> NodeId {
    node_id("local-node")
}

fn local_environment_id() -> EnvironmentId {
    EnvironmentId::new("test-local-environment")
}

async fn empty_environment_manager() -> Arc<EnvironmentManager> {
    let discovery = fake_discovery(false);
    Arc::new(EnvironmentManager::new_local(&discovery, local_environment_id(), HostId::new("test-local-host-id")).await)
}

fn repo_identity() -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
}

fn fresh_checkout_action(branch: &str) -> CommandAction {
    CommandAction::Checkout { repo: repo_selector(), target: CheckoutTarget::FreshBranch(branch.to_string()), issue_ids: vec![] }
}

fn existing_branch_checkout_action(branch: &str) -> CommandAction {
    CommandAction::Checkout { repo: repo_selector(), target: CheckoutTarget::Branch(branch.to_string()), issue_ids: vec![] }
}

fn remove_checkout_action(branch: &str) -> CommandAction {
    CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query(branch.to_string()) }
}

fn assert_error_contains(result: CommandValue, expected_substring: &str) {
    match result {
        CommandValue::Error { message } => {
            assert!(message.contains(expected_substring), "expected error containing {expected_substring:?}, got {message:?}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

fn assert_error_eq(result: CommandValue, expected: &str) {
    match result {
        CommandValue::Error { message } => assert_eq!(message, expected),
        other => panic!("expected Error, got {:?}", other),
    }
}

fn assert_refusal_contains(refusal: PlannerRefusal, expected_substring: &str) {
    let message = refusal.message();
    assert!(message.contains(expected_substring), "expected refusal containing {expected_substring:?}, got {message:?}");
}

fn assert_checkout_created_branch(result: CommandValue, expected_branch: &str) {
    match result {
        CommandValue::CheckoutCreated { branch, .. } => {
            assert_eq!(branch, expected_branch);
        }
        other => panic!("expected CheckoutCreated, got {:?}", other),
    }
}

fn assert_checkout_status_branch(result: CommandValue, expected_branch: &str) {
    match result {
        CommandValue::CheckoutStatus(info) => {
            assert_eq!(info.branch, expected_branch);
        }
        other => panic!("expected CheckoutStatus, got {:?}", other),
    }
}

fn assert_checkout_removed_branch(result: CommandValue, expected_branch: &str) {
    match result {
        CommandValue::CheckoutRemoved { branch } => {
            assert_eq!(branch, expected_branch);
        }
        other => panic!("expected CheckoutRemoved, got {:?}", other),
    }
}

fn assert_branch_name_generated(result: CommandValue, expected_name: &str, expected_issue_ids: &[(&str, &str)]) {
    match result {
        CommandValue::BranchNameGenerated { name, issue_ids } => {
            assert_eq!(name, expected_name);
            let expected_issue_ids: Vec<_> =
                expected_issue_ids.iter().map(|(provider, id)| (provider.to_string(), id.to_string())).collect();
            assert_eq!(issue_ids, expected_issue_ids);
        }
        other => panic!("expected BranchNameGenerated, got {:?}", other),
    }
}

fn assert_ok(result: CommandValue) {
    assert!(matches!(result, CommandValue::Ok));
}

// -----------------------------------------------------------------------
// Tests: ArchiveSession
// -----------------------------------------------------------------------

#[tokio::test]
async fn archive_session_uses_provider_from_session_ref() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::failing("wrong provider")));
    registry.cloud_agents.insert("cursor", desc("cursor"), Arc::new(MockCloudAgent::succeeding()));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("cursor", "sess-1").build());
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, registry, data, runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn checkout_action_does_not_create_workspace() {
    // #2918: a personal checkout neither selects an existing PM workspace
    // nor creates a new one, even when a PM provider is available.
    let ws_mgr =
        Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:99".to_string(), Workspace { name: "feat-x".to_string() })]));

    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.presentation_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result =
        run_build_plan_to_completion_with(fresh_checkout_action("feat-x"), registry, empty_data(), runner, repo_root(), config_base())
            .await;

    assert_checkout_created_branch(result, "feat-x");
    let calls = ws_mgr.calls.lock().await;
    assert!(calls.is_empty(), "checkout must not call the presentation manager, got: {calls:?}");
}

// -----------------------------------------------------------------------
// Tests: CreateCheckout
// -----------------------------------------------------------------------

#[tokio::test]
async fn create_checkout_no_manager() {
    let registry = empty_registry();
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_build_plan_to_completion(fresh_checkout_action("feat-x"), registry, empty_data(), runner).await;

    assert_error_contains(result, "No VCS provider available");
}

#[tokio::test]
async fn create_checkout_success() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.presentation_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_build_plan_to_completion(fresh_checkout_action("feat-x"), registry, empty_data(), runner).await;

    assert_checkout_created_branch(result, "feat-x");
}

#[tokio::test]
async fn create_checkout_with_issue_ids_writes_git_config() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.presentation_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    // Two validation probes (branch absent locally/remotely), then the git config write.
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string()), Ok(String::new())]);

    let result = run_build_plan_to_completion(
        CommandAction::Checkout {
            repo: repo_selector(),
            target: CheckoutTarget::FreshBranch("feat-x".to_string()),
            issue_ids: vec![("github".to_string(), "42".to_string())],
        },
        registry,
        empty_data(),
        runner,
    )
    .await;

    assert_checkout_created_branch(result, "feat-x");
}

#[tokio::test]
async fn create_checkout_failure() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("branch already exists")));
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_build_plan_to_completion(fresh_checkout_action("feat-x"), registry, empty_data(), runner).await;

    assert_error_eq(result, "branch already exists");
}

#[tokio::test]
async fn create_checkout_success_ws_manager_fails_still_returns_created() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.presentation_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws failed")));
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_build_plan_to_completion(fresh_checkout_action("feat-x"), registry, empty_data(), runner).await;

    // Workspace failure is logged but checkout still reports success
    assert_checkout_created_branch(result, "feat-x");
}

// -----------------------------------------------------------------------
// Tests: RemoveCheckout
// -----------------------------------------------------------------------

#[tokio::test]
async fn remove_checkout_no_manager() {
    let registry = empty_registry();
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-old").into(), TestCheckout::new("old").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(remove_checkout_action("old"), registry, data, runner).await;

    assert_error_contains(result, "No VCS provider available");
}

#[tokio::test]
async fn remove_checkout_success() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-old").into(), TestCheckout::new("old").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(remove_checkout_action("old"), registry, data, runner).await;

    assert_checkout_removed_branch(result, "old");
}

#[tokio::test]
async fn remove_checkout_failure() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("cannot remove trunk")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-main").into(), TestCheckout::new("main").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(remove_checkout_action("main"), registry, data, runner).await;

    assert_error_eq(result, "cannot remove trunk");
}

// -----------------------------------------------------------------------
// Tests: RemoveCheckout — terminal lifecycle independence
// -----------------------------------------------------------------------

// Terminal-provider boundary double: expose a live session at the checkout and record kills.
struct MockTerminalPool {
    killed: tokio::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl TerminalPool for MockTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        Ok(vec![TerminalSession::builder()
            .session_name("existing-session".into())
            .status(flotilla_protocol::TerminalStatus::Running)
            .working_directory(ExecutionEnvironmentPath::new("/repo/wt-feat-x"))
            .build()])
    }
    async fn ensure_session(
        &self,
        _session_name: &str,
        _cmd: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
        _tags: &[TerminalSessionTag],
    ) -> Result<(), String> {
        Ok(())
    }
    fn attach_args(
        &self,
        session_name: &str,
        _cmd: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        Ok(vec![flotilla_protocol::arg::Arg::Literal(format!("attach:{session_name}"))])
    }
    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        self.killed.lock().await.push(session_name.to_string());
        Ok(())
    }
}

// #2918: removing a checkout leaves its live terminal for the TerminalSession lifecycle to manage.
#[tokio::test]
async fn remove_checkout_preserves_terminal_sessions() {
    let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });

    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.terminal_pools.insert("cleat", desc("cleat"), Arc::clone(&mock_pool) as Arc<dyn TerminalPool>);
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-x").into(), TestCheckout::new("feat-x").build());

    let runner = runner_ok();
    let result = run_build_plan_to_completion(remove_checkout_action("feat-x"), registry, data, runner).await;

    assert_checkout_removed_branch(result, "feat-x");
    assert!(mock_pool.killed.lock().await.is_empty(), "TerminalSession lifecycle owns terminal teardown");
}

// -----------------------------------------------------------------------
// Tests: FetchCheckoutStatus
// -----------------------------------------------------------------------

#[tokio::test]
async fn fetch_checkout_status_returns_checkout_status() {
    let registry = empty_registry();
    // fetch_checkout_status runs multiple git/gh commands concurrently via
    // tokio::join!. Provide enough error responses for all subprocess calls:
    //   - git rev-parse (upstream) -> Err
    //   - git rev-parse (origin/HEAD) -> Err
    //   - git status --porcelain -> Err
    //   - gh pr view -> Err
    let runner = MockRunner::new(vec![Err("err".to_string()), Err("err".to_string()), Err("err".to_string()), Err("err".to_string())]);

    let result = run_build_plan_to_completion(
        CommandAction::FetchCheckoutStatus {
            branch: "feat".to_string(),
            checkout_path: Some(PathBuf::from("/repo/wt")),
            change_request_id: Some("42".to_string()),
        },
        registry,
        empty_data(),
        runner,
    )
    .await;

    assert_checkout_status_branch(result, "feat");
}

#[tokio::test]
async fn fetch_checkout_status_populates_uncommitted_files() {
    let registry = empty_registry();
    let runner = MockRunner::new(vec![
        Err("err".to_string()),
        Err("err".to_string()),
        Ok(" M src/main.rs\n?? TODO.txt\n".to_string()),
        Err("err".to_string()),
    ]);

    let result = run_build_plan_to_completion(
        CommandAction::FetchCheckoutStatus {
            branch: "feat".to_string(),
            checkout_path: Some(PathBuf::from("/repo/wt")),
            change_request_id: None,
        },
        registry,
        empty_data(),
        runner,
    )
    .await;

    match result {
        CommandValue::CheckoutStatus(info) => {
            assert!(info.has_uncommitted);
            assert_eq!(info.uncommitted_files, vec![" M src/main.rs".to_string(), "?? TODO.txt".to_string(),]);
        }
        other => panic!("expected CheckoutStatus, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Tests: OpenChangeRequest
// -----------------------------------------------------------------------

#[tokio::test]
async fn open_change_request_no_provider() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::OpenChangeRequest { id: "42".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn open_change_request_with_provider() {
    let mut registry = empty_registry();
    registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker));
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::OpenChangeRequest { id: "42".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

// -----------------------------------------------------------------------
// Tests: CloseChangeRequest
// -----------------------------------------------------------------------

#[tokio::test]
async fn close_change_request_no_provider() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::CloseChangeRequest { id: "42".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn close_change_request_with_provider() {
    let mut registry = empty_registry();
    registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker));
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::CloseChangeRequest { id: "42".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

// -----------------------------------------------------------------------
// Tests: MergeChangeRequest
// -----------------------------------------------------------------------

#[tokio::test]
async fn merge_change_request_requires_confirmation() {
    let result = run_build_plan_to_completion(
        CommandAction::MergeChangeRequest { id: "42".to_string(), confirmed: false },
        empty_registry(),
        empty_data(),
        runner_ok(),
    )
    .await;

    assert_error_contains(result, "confirmation");
}

#[tokio::test]
async fn merge_change_request_requires_provider() {
    let result = run_build_plan_to_completion(
        CommandAction::MergeChangeRequest { id: "42".to_string(), confirmed: true },
        empty_registry(),
        empty_data(),
        runner_ok(),
    )
    .await;

    assert_error_contains(result, "change request provider");
}

#[tokio::test]
async fn merge_change_request_calls_provider() {
    let provider = Arc::new(MergeChangeRequestTracker { calls: tokio::sync::Mutex::new(Vec::new()), result: Ok(()) });
    let mut registry = empty_registry();
    registry.change_requests.insert("github", desc("github"), provider.clone());

    let result = run_build_plan_to_completion(
        CommandAction::MergeChangeRequest { id: "42".to_string(), confirmed: true },
        registry,
        empty_data(),
        runner_ok(),
    )
    .await;

    assert_ok(result);
    assert_eq!(provider.calls.lock().await.as_slice(), ["42"]);
}

#[tokio::test]
async fn merge_change_request_surfaces_provider_failure() {
    let provider = Arc::new(MergeChangeRequestTracker {
        calls: tokio::sync::Mutex::new(Vec::new()),
        result: Err("merge blocked by required checks".to_string()),
    });
    let mut registry = empty_registry();
    registry.change_requests.insert("github", desc("github"), provider);

    let result = run_build_plan_to_completion(
        CommandAction::MergeChangeRequest { id: "42".to_string(), confirmed: true },
        registry,
        empty_data(),
        runner_ok(),
    )
    .await;

    assert_error_eq(result, "merge blocked by required checks");
}

// -----------------------------------------------------------------------
// Tests: OpenIssue
// -----------------------------------------------------------------------

#[tokio::test]
async fn open_issue_no_provider() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result = run_build_plan_to_completion(CommandAction::OpenIssue { id: "10".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn open_issue_with_provider() {
    let mut registry = empty_registry();
    registry.issue_trackers.insert("github", desc("github"), Arc::new(MockIssueProvider::empty()));
    let runner = runner_ok();

    let result = run_build_plan_to_completion(CommandAction::OpenIssue { id: "10".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

// -----------------------------------------------------------------------
// Tests: LinkIssuesToChangeRequest
// -----------------------------------------------------------------------

fn link_request_response(body: &str) -> String {
    format!("HTTP/2.0 200 OK\n\n{}", serde_json::json!({"number": 55, "title": "PR", "head": {"ref": "feature"}, "body": body}))
}

fn install_link_provider(registry: &mut ProviderRegistry, runner: MockRunner) {
    use crate::providers::{change_request::github::GitHubChangeRequest, github_api::GhApiClient};
    let runner: Arc<dyn CommandRunner> = Arc::new(runner);
    let provider = GitHubChangeRequest::new("github".into(), "owner/repo".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
    registry.change_requests.insert("github", desc("github"), Arc::new(provider));
}

#[tokio::test]
async fn link_issues_success_with_existing_body() {
    let mut registry = empty_registry();
    // Subprocess boundary: the provider reads the explicitly addressed GitHub API.
    // Second call: gh pr edit succeeds
    let runner = MockRunner::new(vec![Ok(link_request_response("Existing PR body")), Ok(String::new())]);

    install_link_provider(&mut registry, runner);
    let result = run_build_plan_to_completion(
        CommandAction::LinkIssuesToChangeRequest {
            change_request_id: "55".to_string(),
            issue_ids: vec!["10".to_string(), "20".to_string()],
        },
        registry,
        empty_data(),
        MockRunner::new(vec![]),
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn link_issues_success_with_empty_body() {
    let mut registry = empty_registry();
    let runner = MockRunner::new(vec![
        Ok(link_request_response("  \n")), // empty/whitespace body
        Ok(String::new()),                 // edit succeeds
    ]);

    install_link_provider(&mut registry, runner);
    let result = run_build_plan_to_completion(
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
        registry,
        empty_data(),
        MockRunner::new(vec![]),
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn link_issues_view_fails() {
    let mut registry = empty_registry();
    let runner = MockRunner::new(vec![Err("gh not found".to_string())]);

    install_link_provider(&mut registry, runner);
    let result = run_build_plan_to_completion(
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
        registry,
        empty_data(),
        MockRunner::new(vec![]),
    )
    .await;

    assert_error_eq(result, "gh not found");
}

#[tokio::test]
async fn link_issues_edit_fails() {
    let mut registry = empty_registry();
    let runner = MockRunner::new(vec![Ok(link_request_response("body text")), Err("permission denied".to_string())]);

    install_link_provider(&mut registry, runner);
    let result = run_build_plan_to_completion(
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
        registry,
        empty_data(),
        MockRunner::new(vec![]),
    )
    .await;

    assert_error_eq(result, "permission denied");
}

// -----------------------------------------------------------------------
// Tests: ArchiveSession
// -----------------------------------------------------------------------

#[tokio::test]
async fn archive_session_not_found() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::ArchiveSession { session_id: "nonexistent".to_string() },
        registry,
        empty_data(),
        runner,
    )
    .await;

    assert_error_contains(result, "session not found");
}

#[tokio::test]
async fn archive_session_no_agent_provider() {
    let registry = empty_registry();
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, registry, data, runner).await;

    assert_error_contains(result, "No coding agent provider: claude");
}

#[tokio::test]
async fn archive_session_success() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, registry, data, runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn archive_session_agent_fails() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::failing("archive failed")));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, registry, data, runner).await;

    assert_error_eq(result, "archive failed");
}

// -----------------------------------------------------------------------
// Tests: GenerateBranchName
// -----------------------------------------------------------------------

#[tokio::test]
async fn generate_branch_name_ai_success() {
    let mut registry = empty_registry();
    registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::succeeding("feat/add-login")));
    registry.issue_trackers.insert(
        "github",
        desc("github"),
        Arc::new(MockIssueProvider::with_fetched_issues(vec![("42".into(), TestIssue::new("Add login feature").build())])),
    );
    let data = empty_data();
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, registry, data, runner)
            .await;

    assert_branch_name_generated(result, "feat/add-login", &[("github", "42")]);
}

#[tokio::test]
async fn generate_branch_name_ai_failure_uses_fallback() {
    let mut registry = empty_registry();
    registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::failing("API error")));
    let data = empty_data();
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, registry, data, runner)
            .await;

    assert_branch_name_generated(result, "issue-42", &[("issues", "42")]);
}

#[tokio::test]
async fn generate_branch_name_no_ai_provider_uses_fallback() {
    let registry = empty_registry();
    let data = empty_data();
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::GenerateBranchName { issue_keys: vec!["7".to_string()] }, registry, data, runner).await;

    // No issue tracker registered, defaults to "issues"
    assert_branch_name_generated(result, "issue-7", &[("issues", "7")]);
}

#[tokio::test]
async fn generate_branch_name_multiple_issues() {
    let mut registry = empty_registry();
    registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::succeeding("feat/login-and-signup")));
    registry.issue_trackers.insert(
        "github",
        desc("github"),
        Arc::new(MockIssueProvider::with_fetched_issues(vec![
            ("1".into(), TestIssue::new("Login feature").build()),
            ("2".into(), TestIssue::new("Signup feature").build()),
        ])),
    );
    let data = empty_data();
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::GenerateBranchName { issue_keys: vec!["1".to_string(), "2".to_string()] },
        registry,
        data,
        runner,
    )
    .await;

    assert_branch_name_generated(result, "feat/login-and-signup", &[("github", "1"), ("github", "2")]);
}

#[tokio::test]
async fn generate_branch_name_fetches_missing_issue_details() {
    let mut registry = empty_registry();
    let ai = Arc::new(MockAiUtility::succeeding("feat/from-fetched-issue"));
    registry.ai_utilities.insert("claude", desc("claude"), ai.clone());
    let fetched_issue = TestIssue::new("Fix login redirect").with_labels(vec!["bug".into(), "auth".into()]).build();
    registry.issue_trackers.insert(
        "github",
        desc("github"),
        Arc::new(MockIssueProvider::with_fetched_issues(vec![("42".to_string(), fetched_issue)])),
    );
    let data = empty_data();
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, registry, data, runner)
            .await;

    // On-demand source details (including labels) become the AI branch-naming context.
    assert_eq!(ai.contexts.lock().await.as_slice(), ["Fix login redirect #42 [bug, auth]"]);
    assert_branch_name_generated(result, "feat/from-fetched-issue", &[("github", "42")]);
}

#[tokio::test]
async fn generate_branch_name_unknown_issue_key_uses_requested_ids() {
    let registry = empty_registry();
    let data = empty_data();
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::GenerateBranchName { issue_keys: vec!["nonexistent".to_string()] },
        registry,
        data,
        runner,
    )
    .await;

    assert_branch_name_generated(result, "issue-nonexistent", &[("issues", "nonexistent")]);
}

#[tokio::test]
async fn generate_branch_name_unknown_issue_key_still_uses_ai_context() {
    let mut registry = empty_registry();
    registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::succeeding("feat/from-placeholder")));
    let data = empty_data();
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::GenerateBranchName { issue_keys: vec!["nonexistent".to_string()] },
        registry,
        data,
        runner,
    )
    .await;

    assert_branch_name_generated(result, "feat/from-placeholder", &[("issues", "nonexistent")]);
}

// -----------------------------------------------------------------------
// Tests: Daemon-level commands rejected
// -----------------------------------------------------------------------

#[tokio::test]
async fn daemon_level_commands_return_error() {
    let daemon_commands = vec![
        CommandAction::ConvoyWorkForceComplete { convoy: "convoy-a".to_string(), work: "implement".to_string(), message: None },
        CommandAction::TrackRepoPath { path: PathBuf::from("/repo") },
        CommandAction::UntrackRepo { repo: RepoSelector::Path(PathBuf::from("/repo")) },
        CommandAction::Refresh { repo: None },
    ];

    for cmd in daemon_commands {
        let result = run_build_plan(cmd, empty_registry(), empty_data(), runner_ok()).await;
        match result {
            Err(refusal) => assert_refusal_contains(refusal, "daemon-level command"),
            Ok(_) => panic!("expected Err for daemon-level command"),
        }
    }
}

// -----------------------------------------------------------------------
// Helper to run build_plan with Arc-wrapped arguments
// -----------------------------------------------------------------------

async fn run_build_plan(
    action: CommandAction,
    _registry: ProviderRegistry,
    providers_data: ProviderData,
    _runner: MockRunner,
) -> Result<crate::step::StepPlan, PlannerRefusal> {
    let _config_base = config_base();
    build_plan(local_command(action), Arc::new(providers_data), local_node_id(), local_host()).await
}

async fn run_build_plan_to_completion(
    action: CommandAction,
    registry: ProviderRegistry,
    providers_data: ProviderData,
    runner: MockRunner,
) -> CommandValue {
    let config_base = config_base();
    run_build_plan_to_completion_with(action, registry, providers_data, runner, repo_root(), config_base).await
}

async fn run_build_plan_to_completion_with(
    action: CommandAction,
    registry: ProviderRegistry,
    providers_data: ProviderData,
    runner: MockRunner,
    root: ExecutionEnvironmentPath,
    config_base: DaemonHostPath,
) -> CommandValue {
    use tokio_util::sync::CancellationToken;

    use crate::step::run_step_plan;

    let local_host = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root };
    let registry = Arc::new(registry);
    let providers_data = Arc::new(providers_data);
    let runner: Arc<dyn CommandRunner> = Arc::new(runner);

    let vcs_resolver: Arc<dyn crate::vcs::CheckoutVcsResolver> = match registry.vcs.preferred() {
        Some(vcs) => Arc::new(crate::vcs::FixedVcsResolver(vcs.clone())),
        None if matches!(action, CommandAction::Checkout { .. } | CommandAction::RemoveCheckout { .. }) => {
            Arc::new(crate::vcs::FixedVcsResolver(Arc::new(MockCheckoutManager::failing("No VCS provider available"))))
        }
        None => test_vcs_resolver(runner.clone()),
    };
    let plan = build_plan(local_command(action), Arc::clone(&providers_data), local_node_id(), local_host.clone()).await;

    match plan {
        Err(refusal) => refusal.into_command_value(),
        Ok(step_plan) => {
            let (cancel, tx) = (CancellationToken::new(), Arc::new(RecordingEventSink::default()));
            let resolver = ExecutorStepResolver {
                repo,
                registry,
                providers_data,
                runner: runner.clone(),
                vcs_resolver,
                env: Arc::new(TestEnvVars::default()),
                config_base,
                daemon_socket_path: None,

                local_host: local_host.clone(),
                environment_manager: empty_environment_manager().await,
            };
            run_step_plan(step_plan, 1, local_node_id(), repo_identity(), repo_root(), cancel, tx, &resolver).await
        }
    }
}

// -----------------------------------------------------------------------
// Tests: build_plan
// -----------------------------------------------------------------------

#[tokio::test]
async fn build_plan_create_checkout_returns_steps() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.presentation_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let data = empty_data();
    let runner = runner_ok();

    let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, data, runner).await;

    match plan {
        Ok(step_plan) => {
            assert_eq!(step_plan.steps.len(), 1, "checkout needs no personal workspace");
            assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
        }
        Err(_) => panic!("expected Ok, got Err"),
    }
}

#[tokio::test]
async fn checkout_command_succeeds_without_workspace_manager() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));

    let result = run_build_plan_to_completion(existing_branch_checkout_action("feat-x"), registry, empty_data(), runner_ok()).await;

    assert_checkout_created_branch(result, "feat-x");
}

#[tokio::test]
async fn build_plan_create_checkout_skips_existing() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.presentation_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    // Pre-populate with an existing checkout for the branch
    data.checkouts.insert(hp("/repo/wt-feat-x").into(), TestCheckout::new("feat-x").build());
    let runner = runner_ok();

    let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, data, runner).await;

    match plan {
        Ok(step_plan) => {
            assert_eq!(step_plan.steps.len(), 1, "checkout needs no personal workspace");
            assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
        }
        Err(_) => panic!("expected Ok, got Err"),
    }
}

#[tokio::test]
async fn checkout_plan_ends_after_checkout() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.presentation_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));

    let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, empty_data(), runner_ok()).await;

    match plan {
        Ok(step_plan) => {
            assert_eq!(step_plan.steps.len(), 1, "checkout ends after creating the checkout");
            assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
        }
        Err(_) => panic!("expected Ok"),
    }
}

#[tokio::test]
async fn build_plan_remove_checkout_returns_steps() {
    let mut registry = empty_registry();
    registry.vcs.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-old").into(), TestCheckout::new("old").build());
    let runner = runner_ok();

    let plan = run_build_plan(remove_checkout_action("old"), registry, data, runner).await;

    match plan {
        Ok(step_plan) => {
            // At least 1 step: remove checkout
            assert!(!step_plan.steps.is_empty(), "expected at least 1 step");
        }
        Err(_) => panic!("expected Ok, got Err"),
    }
}

#[tokio::test]
async fn build_plan_archive_session_returns_steps() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let plan = run_build_plan(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, registry, data, runner).await;

    match plan {
        Ok(step_plan) => {
            assert_eq!(step_plan.steps.len(), 1, "expected a single archive step");
            assert_eq!(step_plan.steps[0].description, "Archive session sess-1");
        }
        Err(_) => panic!("expected Ok, got Err"),
    }
}

#[tokio::test]
async fn build_plan_generate_branch_name_returns_steps() {
    let mut registry = empty_registry();
    registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::succeeding("feat/add-login")));
    let data = empty_data();
    let runner = runner_ok();

    let plan = run_build_plan(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, registry, data, runner).await;

    match plan {
        Ok(step_plan) => {
            assert_eq!(step_plan.steps.len(), 1, "expected a single branch-name step");
            assert_eq!(step_plan.steps[0].description, "Generate branch name");
        }
        Err(_) => panic!("expected Ok, got Err"),
    }
}

#[tokio::test]
async fn build_plan_archive_session_missing_session_returns_error() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::ArchiveSession { session_id: "missing".to_string() }, registry, empty_data(), runner)
            .await;

    assert_error_contains(result, "session not found");
}

#[tokio::test]
async fn build_plan_generate_branch_name_without_ai_returns_fallback() {
    let data = empty_data();
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] },
        empty_registry(),
        data,
        runner,
    )
    .await;

    assert_branch_name_generated(result, "issue-42", &[("issues", "42")]);
}

#[tokio::test]
async fn build_plan_simple_command_returns_ok() {
    let mut registry = empty_registry();
    registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker));
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::OpenChangeRequest { id: "42".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

// -----------------------------------------------------------------------
// Tests: environment checkout plan
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// Tests: resolve_checkout_branch
// -----------------------------------------------------------------------

#[test]
fn resolve_checkout_branch_path_found() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat").into(), TestCheckout::new("feat-branch").build());
    let local_host = HostName::local();

    let result = resolve_checkout_branch(
        &CheckoutSelector::Path(PathBuf::from("/repo/wt-feat")),
        &data,
        &local_host,
        &CheckoutResolutionScope::Local,
    );

    assert_eq!(result.expect("path lookup should succeed"), "feat-branch");
}

#[test]
fn resolve_checkout_branch_path_not_found() {
    let data = empty_data();
    let local_host = HostName::local();

    let result = resolve_checkout_branch(
        &CheckoutSelector::Path(PathBuf::from("/nonexistent")),
        &data,
        &local_host,
        &CheckoutResolutionScope::Local,
    );

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("checkout not found"));
}

#[test]
fn resolve_checkout_branch_query_exact_match() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat").into(), TestCheckout::new("feat-login").build());
    let local_host = HostName::local();

    let result =
        resolve_checkout_branch(&CheckoutSelector::Query("feat-login".to_string()), &data, &local_host, &CheckoutResolutionScope::Local);

    assert_eq!(result.expect("exact query should match"), "feat-login");
}

#[test]
fn resolve_checkout_branch_query_substring_match() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat").into(), TestCheckout::new("feat-login-page").build());
    let local_host = HostName::local();

    let result =
        resolve_checkout_branch(&CheckoutSelector::Query("login".to_string()), &data, &local_host, &CheckoutResolutionScope::Local);

    assert_eq!(result.expect("substring query should match"), "feat-login-page");
}

#[test]
fn resolve_checkout_branch_query_not_found() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat").into(), TestCheckout::new("feat-login").build());
    let local_host = HostName::local();

    let result =
        resolve_checkout_branch(&CheckoutSelector::Query("nonexistent".to_string()), &data, &local_host, &CheckoutResolutionScope::Local);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("checkout not found"));
}

#[test]
fn resolve_checkout_branch_query_ambiguous() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-a").into(), TestCheckout::new("feat-a").build());
    data.checkouts.insert(hp("/repo/wt-feat-b").into(), TestCheckout::new("feat-b").build());
    let local_host = HostName::local();

    let result = resolve_checkout_branch(&CheckoutSelector::Query("feat".to_string()), &data, &local_host, &CheckoutResolutionScope::Local);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("ambiguous"));
}

#[test]
fn resolve_checkout_branch_remote_any_ignores_local_matches() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-local").into(), TestCheckout::new("feat").build());
    let remote = HostPath::new(HostName::new("remote-box"), PathBuf::from("/repo/wt-feat-remote"));
    let mut remote_checkout = TestCheckout::new("feat").build();
    remote_checkout.host_name = Some(HostName::new("remote-box"));
    data.checkouts.insert(remote.into(), remote_checkout);
    let local_host = HostName::local();

    let result =
        resolve_checkout_branch(&CheckoutSelector::Query("feat".to_string()), &data, &local_host, &CheckoutResolutionScope::RemoteAny);

    assert_eq!(result.expect("remote scope should ignore local match"), "feat");
}

#[test]
fn resolve_checkout_branch_host_scope_matches_remote_host_name() {
    let mut data = empty_data();
    let remote_a = HostPath::new(HostName::new("alpha"), PathBuf::from("/repo/wt-feat-a"));
    let mut remote_a_checkout = TestCheckout::new("feat").build();
    remote_a_checkout.host_name = Some(HostName::new("alpha"));
    data.checkouts.insert(remote_a.into(), remote_a_checkout);
    let remote_b = HostPath::new(HostName::new("beta"), PathBuf::from("/repo/wt-feat-b"));
    let mut remote_b_checkout = TestCheckout::new("feat").build();
    remote_b_checkout.host_name = Some(HostName::new("beta"));
    data.checkouts.insert(remote_b.into(), remote_b_checkout);
    let local_host = HostName::local();

    let result = resolve_checkout_branch(
        &CheckoutSelector::Query("feat".to_string()),
        &data,
        &local_host,
        &CheckoutResolutionScope::Host(HostName::new("beta")),
    );

    assert_eq!(result.expect("host scope should resolve targeted remote host"), "feat");
}

// -----------------------------------------------------------------------
// Tests: write_branch_issue_links
// -----------------------------------------------------------------------

#[tokio::test]
async fn write_branch_issue_links_single_provider_multiple_issues() {
    let runner = MockRunner::new(vec![Ok(String::new())]);
    let issue_ids = vec![("github".to_string(), "10".to_string()), ("github".to_string(), "20".to_string())];

    write_branch_issue_links(repo_root().as_path(), "feat-x", &issue_ids, &runner).await;

    assert_eq!(runner.remaining(), 0, "single provider should consume exactly 1 response");
}

#[tokio::test]
async fn write_branch_issue_links_multiple_providers() {
    let runner = MockRunner::new(vec![Ok(String::new()), Ok(String::new())]);
    let issue_ids = vec![("github".to_string(), "10".to_string()), ("jira".to_string(), "PROJ-5".to_string())];

    write_branch_issue_links(repo_root().as_path(), "feat-x", &issue_ids, &runner).await;

    assert_eq!(runner.remaining(), 0, "two providers should consume exactly 2 responses");
}

#[tokio::test]
async fn write_branch_issue_links_git_error_tolerated() {
    let runner = MockRunner::new(vec![Err("git config failed".to_string())]);
    let issue_ids = vec![("github".to_string(), "10".to_string())];

    write_branch_issue_links(repo_root().as_path(), "feat-x", &issue_ids, &runner).await;

    assert_eq!(runner.remaining(), 0, "should still consume the response even on error");
}

#[tokio::test]
async fn write_branch_issue_links_empty_is_noop() {
    let runner = MockRunner::new(vec![]);

    write_branch_issue_links(repo_root().as_path(), "feat-x", &[], &runner).await;

    assert_eq!(runner.remaining(), 0, "empty issue_ids should make zero calls");
}

// -----------------------------------------------------------------------
// Tests: CheckoutService validation delegation
// -----------------------------------------------------------------------

#[tokio::test]
async fn checkout_service_validate_target_uses_checkout_manager() {
    let mut registry = ProviderRegistry::new();
    registry.vcs.insert("checkout", desc("checkout"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/tmp/feat-x")));
    let service = CheckoutService::new(registry.vcs.preferred().expect("fixture VCS").as_ref());

    let result = service.validate_target(&repo_root(), "new-branch", CheckoutIntent::FreshBranch).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn checkout_service_validate_target_propagates_checkout_manager_error() {
    let mut registry = ProviderRegistry::new();
    registry.vcs.insert("checkout", desc("checkout"), Arc::new(MockCheckoutManager::failing("branch already exists: existing")));
    let service = CheckoutService::new(registry.vcs.preferred().expect("fixture VCS").as_ref());

    let result = service.validate_target(&repo_root(), "existing", CheckoutIntent::FreshBranch).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("already exists"));
}

// -----------------------------------------------------------------------
// Tests: ExecutorStepResolver
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// Tests: Environment lifecycle actions
// -----------------------------------------------------------------------

use flotilla_protocol::{EnvironmentId, EnvironmentSpec, EnvironmentStatus, ImageId};

use crate::providers::environment::{EnvironmentHandle, EnvironmentProvider, ProvisionedEnvironment};

struct MockEnvironmentProvider {
    create_results: tokio::sync::Mutex<Vec<Result<EnvironmentHandle, String>>>,
    seen_create_opts: tokio::sync::Mutex<Vec<ProvisionOpts>>,
}

#[async_trait]
impl EnvironmentProvider for MockEnvironmentProvider {
    fn kind(&self) -> EnvironmentKind {
        EnvironmentKind::Docker
    }
    async fn prepare(&self, _spec: &flotilla_resources::EnvironmentSpec, _opts: &PrepareOpts) -> Result<PreparedEnvironment, String> {
        Ok(PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(&self, _id: EnvironmentId, _image: &PreparedEnvironment, opts: ProvisionOpts) -> Result<EnvironmentHandle, String> {
        self.seen_create_opts.lock().await.push(opts);
        self.create_results.lock().await.remove(0)
    }
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Ok(vec![])
    }
    async fn destroy(&self, _container_id: &str) -> Result<(), String> {
        Err("unused in test".to_string())
    }
}

struct MockProvisionedEnvironment {
    id: EnvironmentId,
    image: ImageId,
    runner: Arc<dyn CommandRunner>,
}

#[async_trait]
impl ProvisionedEnvironment for MockProvisionedEnvironment {
    fn id(&self) -> &EnvironmentId {
        &self.id
    }
    fn image(&self) -> &ImageId {
        &self.image
    }
    fn container_name(&self) -> Option<&str> {
        Some("mock-container")
    }
    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        vec![]
    }
    async fn status(&self) -> Result<EnvironmentStatus, String> {
        Ok(EnvironmentStatus::Running)
    }
    async fn env_vars(&self) -> Result<std::collections::HashMap<String, String>, String> {
        let mut vars = std::collections::HashMap::new();
        vars.insert("PATH".to_string(), "/usr/bin".to_string());
        Ok(vars)
    }
    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }
    async fn destroy(&self) -> Result<(), String> {
        Ok(())
    }
}

fn registry_with_env_provider(provider: Arc<dyn EnvironmentProvider>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    let desc = ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "Docker");
    registry.environment_providers.insert("docker", desc, provider);
    registry
}

async fn manager_with_provisioned_environment(
    env_id: &EnvironmentId,
    handle: EnvironmentHandle,
    registry: Option<Arc<ProviderRegistry>>,
) -> Arc<EnvironmentManager> {
    let manager = empty_environment_manager().await;
    manager
        .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), registry)
        .expect("register provisioned environment");
    manager
}

#[tokio::test]
async fn remove_checkout_resolves_for_remote_host() {
    let remote = HostName::new("remote-box");
    let remote_hp = HostPath::new(remote.clone(), PathBuf::from("/repo/wt-feat"));
    let mut data = empty_data();
    data.checkouts.insert(remote_hp.into(), TestCheckout::new("feat").build());

    let _config_base = config_base();
    let plan =
        build_plan(command_with_host("remote-box", remove_checkout_action("feat")), Arc::new(data), local_node_id(), local_host()).await;

    let plan = plan.expect("remote host target should resolve remote-owned checkout");
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(NodeId::new("remote-box")));
}

#[tokio::test]
async fn remove_checkout_disambiguates_by_target_host() {
    // Same branch on two hosts — command.host should disambiguate
    let local = hp("/repo/wt-feat");
    let remote = HostPath::new(HostName::new("remote-box"), PathBuf::from("/repo/wt-feat"));
    let mut data = empty_data();
    data.checkouts.insert(local.into(), TestCheckout::new("feat").build());
    data.checkouts.insert(remote.into(), TestCheckout::new("feat").build());

    let _config_base = config_base();
    let plan =
        build_plan(command_with_host("remote-box", remove_checkout_action("feat")), Arc::new(data), local_node_id(), local_host()).await;

    let plan = plan.expect("build_plan should resolve the targeted checkout");
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(NodeId::new("remote-box")));
    match &plan.steps[0].action {
        StepAction::RemoveCheckout { branch } => assert_eq!(branch, "feat"),
        other => panic!("expected RemoveCheckout step, got {other:?}"),
    }
}

#[tokio::test]
async fn remove_checkout_remote_node_ignores_local_duplicate_branch() {
    let local = hp("/repo/wt-feat-local");
    let remote = HostPath::new(HostName::new("remote-box"), PathBuf::from("/repo/wt-feat-remote"));
    let mut data = empty_data();
    data.checkouts.insert(local.into(), TestCheckout::new("feat").build());
    let mut remote_checkout = TestCheckout::new("feat").build();
    remote_checkout.host_name = Some(HostName::new("remote-box"));
    data.checkouts.insert(remote.into(), remote_checkout);

    let _config_base = config_base();
    let plan = build_plan(
        Command::builder()
            .action(remove_checkout_action("feat"))
            .node_id(NodeId::new("remote-box"))
            .context_repo(RepoSelector::Identity(repo_identity()))
            .build(),
        Arc::new(data),
        local_node_id(),
        local_host(),
    )
    .await
    .expect("build_plan should resolve remote checkout without considering local duplicate");

    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(NodeId::new("remote-box")));
    match &plan.steps[0].action {
        StepAction::RemoveCheckout { branch } => assert_eq!(branch, "feat"),
        other => panic!("expected RemoveCheckout step, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_checkout_status_targets_remote_node_when_command_is_remote() {
    let _registry = empty_registry();
    let _config_base = config_base();
    let plan = build_plan(
        Command::builder()
            .action(CommandAction::FetchCheckoutStatus {
                branch: "feat".to_string(),
                checkout_path: Some(PathBuf::from("/repo/wt")),
                change_request_id: None,
            })
            .node_id(NodeId::new("remote-box"))
            .context_repo(RepoSelector::Identity(repo_identity()))
            .build(),
        Arc::new(empty_data()),
        local_node_id(),
        local_host(),
    )
    .await
    .expect("build_plan should succeed");

    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(NodeId::new("remote-box")));
}

#[tokio::test]
async fn executor_step_resolver_create_environment() {
    let config_base = config_base();
    let env_id = EnvironmentId::new("env-test-1");
    let image_id = ImageId::new("flotilla:test-abc123");

    let mock_env: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
        id: env_id.clone(),
        image: image_id.clone(),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
    });

    let provider = Arc::new(MockEnvironmentProvider {
        create_results: tokio::sync::Mutex::new(vec![Ok(mock_env)]),
        seen_create_opts: tokio::sync::Mutex::new(vec![]),
    });
    let registry = registry_with_env_provider(provider.clone());
    // resolve_reference_repo calls `git rev-parse --git-common-dir`
    let runner = Arc::new(MockRunner::new(vec![Ok("/tmp/test-repo/.git".into())]));
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(registry),
        providers_data: Arc::new(empty_data()),
        runner: runner.clone(),
        vcs_resolver: test_vcs_resolver(runner),
        env: Arc::new(TestEnvVars::new([("GITHUB_TOKEN", "gh-test-token")])),
        config_base: config_base.clone(),
        daemon_socket_path: Some(DaemonHostPath::new("/tmp/flotilla.sock")),
        local_host: local_host(),
        environment_manager: empty_environment_manager().await,
    };

    let spec = EnvironmentSpec {
        image: flotilla_protocol::ImageSource::Registry("test:latest".into()),
        token_env_vars: vec!["GITHUB_TOKEN".into()],
    };
    let prior = vec![StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec })];
    let action = StepAction::CreateEnvironment { env_id: env_id.clone(), provider: "docker".into() };
    let context = StepExecutionContext::Host(local_node_id());
    let outcome = resolver.resolve("create env", &context, action, &prior).await;
    assert!(matches!(outcome, Ok(StepOutcome::Completed)));

    assert!(
        resolver.environment_manager.environment_runner(&env_id).is_some(),
        "environment manager should expose the created environment runner"
    );
    let seen_create_opts = provider.seen_create_opts.lock().await;
    assert_eq!(seen_create_opts.len(), 1);
    assert_eq!(seen_create_opts[0].tokens, vec![("GITHUB_TOKEN".to_string(), "gh-test-token".to_string())]);
}

#[tokio::test]
async fn executor_step_resolver_create_environment_errors_without_spec_outcome() {
    let config_base = config_base();
    let env_id = EnvironmentId::new("env-test-missing-image");
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(empty_registry()),
        providers_data: Arc::new(empty_data()),
        runner: Arc::new(runner_ok()),
        vcs_resolver: test_vcs_resolver(Arc::new(runner_ok())),
        env: Arc::new(TestEnvVars::default()),
        config_base: config_base.clone(),
        daemon_socket_path: Some(DaemonHostPath::new("/tmp/flotilla.sock")),
        local_host: local_host(),
        environment_manager: empty_environment_manager().await,
    };

    let action = StepAction::CreateEnvironment { env_id, provider: "docker".into() };
    let context = StepExecutionContext::Host(local_node_id());
    let outcome = resolver.resolve("create env", &context, action, &[]).await;

    assert!(outcome.is_err(), "create environment should fail without an environment spec");
    assert!(outcome.unwrap_err().contains("spec not produced by prior ReadEnvironmentSpec step"));
}

#[tokio::test]
async fn executor_step_resolver_destroy_environment() {
    let config_base = config_base();
    let env_id = EnvironmentId::new("env-destroy-1");
    let image_id = ImageId::new("flotilla:test");

    let mock_env: EnvironmentHandle =
        Arc::new(MockProvisionedEnvironment { id: env_id.clone(), image: image_id, runner: Arc::new(runner_ok()) });

    let environment_manager = manager_with_provisioned_environment(&env_id, mock_env, None).await;

    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(empty_registry()),
        providers_data: Arc::new(empty_data()),
        runner: Arc::new(runner_ok()),
        vcs_resolver: test_vcs_resolver(Arc::new(runner_ok())),
        env: Arc::new(TestEnvVars::default()),
        config_base: config_base.clone(),
        daemon_socket_path: None,
        local_host: local_host(),
        environment_manager,
    };

    let action = StepAction::DestroyEnvironment { env_id: env_id.clone() };
    let context = StepExecutionContext::Host(local_node_id());
    let outcome = resolver.resolve("destroy env", &context, action, &[]).await;
    assert!(matches!(outcome, Ok(StepOutcome::Completed)), "destroy should complete: {outcome:?}");

    assert!(
        resolver.environment_manager.environment_runner(&env_id).is_none(),
        "environment manager should remove the destroyed environment"
    );
}

#[tokio::test]
async fn executor_step_resolver_destroy_environment_not_found() {
    let config_base = config_base();
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(empty_registry()),
        providers_data: Arc::new(empty_data()),
        runner: Arc::new(runner_ok()),
        vcs_resolver: test_vcs_resolver(Arc::new(runner_ok())),
        env: Arc::new(TestEnvVars::default()),
        config_base: config_base.clone(),
        daemon_socket_path: None,
        local_host: local_host(),
        environment_manager: empty_environment_manager().await,
    };

    let action = StepAction::DestroyEnvironment { env_id: EnvironmentId::new("nonexistent") };
    let context = StepExecutionContext::Host(local_node_id());
    let outcome = resolver.resolve("destroy env", &context, action, &[]).await;
    assert!(outcome.is_err(), "should fail when handle not found");
    assert!(outcome.unwrap_err().contains("environment handle not found"));
}

// #2918: checkout creates code and optional issue links, never a personal workspace.
// Generate host, new-environment and existing-environment destinations, fresh/existing
// branches, and empty/nonempty issue lists; routing and returned checkout intent survive.
#[hegel::test]
fn checkout_plans_are_headless_for_every_destination(tc: hegel::TestCase) {
    use flotilla_protocol::ProvisioningTarget;
    use hegel::generators as gs;
    let destination = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let fresh = tc.draw(gs::booleans());
    let linked = tc.draw(gs::booleans());
    let target_host = HostName::new("checkout-host");
    let target_node = NodeId::new("checkout-node");
    let env_id = EnvironmentId::new("existing-environment");
    let target = match destination {
        0 => ProvisioningTarget::Host { host: target_host },
        1 => ProvisioningTarget::NewEnvironment { host: target_host, provider: "docker".into() },
        _ => ProvisioningTarget::ExistingEnvironment { host: target_host, env_id: env_id.clone() },
    };
    let branch = "headless-checkout";
    let issue_ids = if linked { vec![("github".into(), "42".into())] } else { vec![] };
    let cmd = Command::builder()
        .action(CommandAction::Checkout {
            repo: repo_selector(),
            target: if fresh { CheckoutTarget::FreshBranch(branch.into()) } else { CheckoutTarget::Branch(branch.into()) },
            issue_ids: issue_ids.clone(),
        })
        .node_id(target_node.clone())
        .provisioning_target(target)
        .build();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let plan = runtime.block_on(build_plan(cmd, Arc::new(empty_data()), local_node_id(), local_host())).expect("checkout plan");
    let checkout = plan.steps.iter().find(|step| matches!(step.action, StepAction::CreateCheckout { .. })).expect("checkout step");
    match &checkout.action {
        StepAction::CreateCheckout { branch: actual, create_branch, intent, issue_ids: actual_issues } => {
            assert_eq!(actual, branch);
            assert_eq!(*create_branch, fresh);
            assert_eq!(*intent, if fresh { CheckoutIntent::FreshBranch } else { CheckoutIntent::ExistingBranch });
            assert_eq!(actual_issues, &issue_ids);
        }
        _ => unreachable!(),
    }
    match (&checkout.host, destination) {
        (StepExecutionContext::Host(node), 0) => assert_eq!(node, &target_node),
        (StepExecutionContext::Environment(node, actual_env), 1 | 2) => {
            assert_eq!(node, &target_node);
            if destination == 2 {
                assert_eq!(actual_env, &env_id);
            }
        }
        other => panic!("wrong checkout destination: {other:?}"),
    }
    assert_eq!(plan.steps.len(), 1 + usize::from(linked) + if destination == 1 { 2 } else { 0 });
    assert_eq!(plan.steps.iter().filter(|step| matches!(step.action, StepAction::LinkIssuesToBranch { .. })).count(), usize::from(linked));
    assert!(plan.steps.iter().all(|step| matches!(
        step.action,
        StepAction::ReadEnvironmentSpec
            | StepAction::CreateEnvironment { .. }
            | StepAction::CreateCheckout { .. }
            | StepAction::LinkIssuesToBranch { .. }
    )));
}
