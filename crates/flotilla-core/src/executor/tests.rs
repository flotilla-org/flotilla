use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use super::{
    build_plan,
    checkout::{resolve_checkout_branch, validate_checkout_target, write_branch_issue_links, CheckoutIntent},
    session_actions::resolve_attach_command,
    workspace_config, ExecutorStepResolver, RepoExecutionContext,
};
use crate::{
    attachable::{AttachableStore, BindingObjectKind, ProviderBinding, SharedAttachableStore},
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    provider_data::ProviderData,
    providers::{
        ai_utility::AiUtility,
        change_request::ChangeRequestTracker,
        coding_agent::CloudAgentService,
        discovery::{ProviderCategory, ProviderDescriptor},
        issue_tracker::IssueTracker,
        registry::ProviderRegistry,
        terminal::TerminalPool,
        testing::MockRunner,
        types::*,
        vcs::CheckoutManager,
        workspace::WorkspaceManager,
        CommandRunner,
    },
    step::{StepAction, StepExecutionContext, StepOutcome, StepResolver},
};

fn desc(name: &str) -> ProviderDescriptor {
    ProviderDescriptor::named(ProviderCategory::Vcs, name)
}
use async_trait::async_trait;
use flotilla_protocol::{
    arg::Arg,
    test_support::{TestCheckout, TestIssue, TestSession},
    CheckoutSelector, CheckoutTarget, Command, CommandAction, CommandValue, HostName, PreparedTerminalCommand, QualifiedPath, RepoSelector,
    ResolvedPaneCommand, TerminalStatus,
};

fn hp(path: &str) -> QualifiedPath {
    QualifiedPath::from_host_path(&local_host(), PathBuf::from(path))
}

fn remote_host() -> HostName {
    HostName::new("test-remote")
}

// -----------------------------------------------------------------------
// Mock providers
// -----------------------------------------------------------------------

/// A mock CheckoutManager that returns a canned checkout or error.
struct MockCheckoutManager {
    create_result: tokio::sync::Mutex<Option<Result<(PathBuf, Checkout), String>>>,
    remove_result: tokio::sync::Mutex<Option<Result<(), String>>>,
}

impl MockCheckoutManager {
    fn succeeding(branch: &str, path: &str) -> Self {
        Self {
            create_result: tokio::sync::Mutex::new(Some(Ok((PathBuf::from(path), Checkout {
                branch: branch.to_string(),
                is_main: false,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: vec![],
                association_keys: vec![],
                environment_id: None,
            })))),
            remove_result: tokio::sync::Mutex::new(Some(Ok(()))),
        }
    }

    fn failing(msg: &str) -> Self {
        Self {
            create_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
            remove_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
        }
    }
}

#[async_trait]
impl CheckoutManager for MockCheckoutManager {
    async fn list_checkouts(&self, _repo_root: &ExecutionEnvironmentPath) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        Ok(vec![])
    }
    async fn create_checkout(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        _branch: &str,
        _create_branch: bool,
    ) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        self.create_result
            .lock()
            .await
            .take()
            .expect("create_checkout called more than expected")
            .map(|(p, co)| (ExecutionEnvironmentPath::new(p), co))
    }
    async fn remove_checkout(&self, _repo_root: &ExecutionEnvironmentPath, _branch: &str) -> Result<(), String> {
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
impl WorkspaceManager for MockWorkspaceManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        self.calls.lock().await.push("list_workspaces".to_string());
        Ok(self.existing.clone())
    }
    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        self.created_configs.lock().await.push(config.clone());
        self.calls.lock().await.push(format!("create_workspace:{}", config.name));
        let result = self.create_result.lock().await;
        match &*result {
            Ok(()) => {
                Ok(("mock-ref".to_string(), Workspace { name: config.name.clone(), correlation_keys: vec![], attachable_set_id: None }))
            }
            Err(e) => Err(e.clone()),
        }
    }
    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        self.calls.lock().await.push(format!("select_workspace:{ws_ref}"));
        let result = self.select_result.lock().await;
        result.clone()
    }
    fn binding_scope_prefix(&self) -> String {
        String::new()
    }
}

/// A mock ChangeRequestTracker provider.
struct MockChangeRequestTracker;

#[async_trait]
impl ChangeRequestTracker for MockChangeRequestTracker {
    async fn list_change_requests(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        Ok(vec![])
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
        Ok(vec![])
    }
}

/// A mock IssueTracker provider.
struct MockIssueTracker;

#[async_trait]
impl IssueTracker for MockIssueTracker {
    async fn list_issues(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<(String, Issue)>, String> {
        Ok(vec![])
    }
    async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
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

    fn with_attach(attach_command: &str) -> Self {
        Self { archive_result: tokio::sync::Mutex::new(Ok(())), attach_command: attach_command.to_string() }
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
}

impl MockAiUtility {
    fn succeeding(name: &str) -> Self {
        Self { result: tokio::sync::Mutex::new(Ok(name.to_string())) }
    }

    fn failing(msg: &str) -> Self {
        Self { result: tokio::sync::Mutex::new(Err(msg.to_string())) }
    }
}

#[async_trait]
impl AiUtility for MockAiUtility {
    async fn generate_branch_name(&self, _context: &str) -> Result<String, String> {
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
    Command { host: None, provisioning_target: None, context_repo: None, action }
}

fn command_with_host(host: &str, action: CommandAction) -> Command {
    Command { host: Some(HostName::new(host)), provisioning_target: None, context_repo: None, action }
}

fn local_host() -> HostName {
    HostName::new("test-local")
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

fn test_attachable_store(base: &DaemonHostPath) -> SharedAttachableStore {
    crate::attachable::shared_file_backed_attachable_store(base)
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
// Tests: CreateWorkspaceForCheckout
// -----------------------------------------------------------------------

#[tokio::test]
async fn create_workspace_for_checkout_not_found() {
    let registry = empty_registry();
    let data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() },
        registry,
        data,
        runner,
    )
    .await;

    assert_error_contains(result, "checkout not found");
}

#[tokio::test]
async fn create_workspace_for_checkout_success_without_ws_manager() {
    let registry = empty_registry();
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    let path = PathBuf::from("/repo/wt-feat");
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() },
        registry,
        data,
        runner,
    )
    .await;

    assert_ok(result);
}

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
async fn create_workspace_for_checkout_success_with_ws_manager() {
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    let path = PathBuf::from("/repo/wt-feat");
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() },
        registry,
        data,
        runner,
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn create_workspace_for_checkout_persists_workspace_binding() {
    let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
    let mut data = empty_data();
    let checkout_path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(&DaemonHostPath::new(temp.path()));

    let result = run_build_plan_to_completion_with(
        CommandAction::CreateWorkspaceForCheckout { checkout_path: checkout_path.clone(), label: "feat".into() },
        registry,
        data,
        runner,
        repo_root(),
        DaemonHostPath::new(temp.path()),
        attachable_store,
    )
    .await;

    assert_ok(result);
    let store = AttachableStore::with_base(&crate::path_context::DaemonHostPath::new(temp.path()));
    let object_id = store
        .lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "mock-ref")
        .expect("workspace binding should exist");
    let set = store.registry().sets.values().find(|set| set.id.as_str() == object_id).expect("set should exist");
    assert_eq!(set.checkout, Some(QualifiedPath::from_host_path(&local_host(), checkout_path)));
}

#[tokio::test]
async fn create_workspace_for_checkout_ws_manager_fails() {
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws creation failed")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    let path = PathBuf::from("/repo/wt-feat");
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() },
        registry,
        data,
        runner,
    )
    .await;

    assert_error_eq(result, "ws creation failed");
}
#[tokio::test]
async fn prepare_terminal_for_checkout_returns_terminal_commands() {
    let registry = empty_registry();
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::PrepareTerminalForCheckout { checkout_path: path.clone(), commands: vec![] },
        registry,
        data,
        runner,
    )
    .await;

    match result {
        CommandValue::TerminalPrepared { repo_identity, target_host, branch, checkout_path, attachable_set_id, commands } => {
            assert_eq!(repo_identity, flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
            assert_eq!(target_host, local_host());
            assert_eq!(branch, "feat");
            assert_eq!(checkout_path, path);
            assert!(attachable_set_id.is_some(), "prepare should allocate an attachable set");
            assert_eq!(commands, vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("claude".into())] }]);
        }
        other => panic!("expected TerminalPrepared, got {other:?}"),
    }
}

#[tokio::test]
async fn prepare_terminal_for_checkout_includes_attachable_set_id_when_present() {
    let registry = empty_registry();
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(&DaemonHostPath::new(temp.path()));
    {
        let mut store = attachable_store.lock().expect("store lock");
        let ensured_set_id =
            store.ensure_terminal_set(Some(local_host()), Some(QualifiedPath::from_host_path(&local_host(), path.clone())));
        store.save().expect("save attachable store");
        assert_eq!(
            store.registry().sets.get(&ensured_set_id).and_then(|set| set.checkout.clone()),
            Some(QualifiedPath::from_host_path(&local_host(), path.clone()))
        );
    }

    let result = run_build_plan_to_completion_with(
        CommandAction::PrepareTerminalForCheckout { checkout_path: path.clone(), commands: vec![] },
        registry,
        data,
        runner,
        repo_root(),
        DaemonHostPath::new(temp.path()),
        attachable_store,
    )
    .await;

    match result {
        CommandValue::TerminalPrepared { attachable_set_id, .. } => {
            let set_id = attachable_set_id.expect("attachable set id");
            let store = AttachableStore::with_base(&crate::path_context::DaemonHostPath::new(temp.path()));
            assert!(store.registry().sets.contains_key(&set_id), "prepare should reuse persisted set");
        }
        other => panic!("expected TerminalPrepared, got {other:?}"),
    }
}

#[tokio::test]
async fn prepare_terminal_for_checkout_creates_and_persists_attachable_set() {
    let registry = empty_registry();
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(&DaemonHostPath::new(temp.path()));

    let result = run_build_plan_to_completion_with(
        CommandAction::PrepareTerminalForCheckout { checkout_path: path.clone(), commands: vec![] },
        registry,
        data,
        runner,
        repo_root(),
        DaemonHostPath::new(temp.path()),
        attachable_store,
    )
    .await;

    let set_id = match result {
        CommandValue::TerminalPrepared { attachable_set_id, .. } => attachable_set_id.expect("attachable set id"),
        other => panic!("expected TerminalPrepared, got {other:?}"),
    };

    let store = AttachableStore::with_base(&crate::path_context::DaemonHostPath::new(temp.path()));
    let set = store.registry().sets.get(&set_id).expect("set should exist");
    assert_eq!(set.checkout, Some(QualifiedPath::from_host_path(&local_host(), path)));
    assert!(temp.path().join("attachables").join("registry.json").exists(), "registry should be written");
}

#[tokio::test]
async fn create_workspace_from_prepared_terminal_wraps_remote_commands_in_ssh() {
    let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(&DaemonHostPath::new(temp.path()));
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("create repo root");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let result = run_build_plan_to_completion_with(
        CommandAction::CreateWorkspaceFromPreparedTerminal {
            target_host: HostName::new("desktop"),
            branch: "feat".into(),
            checkout_path: PathBuf::from("/remote/feat"),
            attachable_set_id: None,
            commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash -l".into())] }],
        },
        registry,
        empty_data(),
        runner,
        ExecutionEnvironmentPath::new(repo_root.clone()),
        DaemonHostPath::new(temp.path()),
        attachable_store,
    )
    .await;

    assert_ok(result);
    let created = workspace_manager.created_configs.lock().await;
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].working_directory, ExecutionEnvironmentPath::new(&repo_root));
    let resolved = &created[0].attach_commands;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].0, "main");
    assert!(resolved[0].1.contains("ssh -t"));
    assert!(resolved[0].1.contains("desktop.local"));
    assert!(resolved[0].1.contains("/remote/feat"));
    assert!(resolved[0].1.contains("bash -l"));
    assert!(resolved[0].1.contains("${SHELL:-/bin/sh} -l -c"), "expected login shell wrapper, got: {}", resolved[0].1);
}

#[tokio::test]
async fn create_workspace_from_prepared_terminal_prefixes_name_with_host() {
    let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(&DaemonHostPath::new(temp.path()));
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("create repo root");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let result = run_build_plan_to_completion_with(
        CommandAction::CreateWorkspaceFromPreparedTerminal {
            target_host: HostName::new("desktop"),
            branch: "feat".into(),
            checkout_path: PathBuf::from("/remote/feat"),
            attachable_set_id: None,
            commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash".into())] }],
        },
        registry,
        empty_data(),
        runner,
        ExecutionEnvironmentPath::new(repo_root),
        DaemonHostPath::new(temp.path()),
        attachable_store,
    )
    .await;

    assert_ok(result);
    let created = workspace_manager.created_configs.lock().await;
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].name, "feat@desktop", "workspace name should be branch@host");
}

#[tokio::test]
async fn create_workspace_from_prepared_terminal_persists_remote_attachable_set_binding() {
    let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(&DaemonHostPath::new(temp.path()));
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("create repo root");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let set_id = flotilla_protocol::AttachableSetId::new("set-remote");
    let result = run_build_plan_to_completion_with(
        CommandAction::CreateWorkspaceFromPreparedTerminal {
            target_host: HostName::new("desktop"),
            branch: "feat".into(),
            checkout_path: PathBuf::from("/remote/feat"),
            attachable_set_id: Some(set_id.clone()),
            commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash".into())] }],
        },
        registry,
        empty_data(),
        runner,
        ExecutionEnvironmentPath::new(repo_root),
        DaemonHostPath::new(temp.path()),
        attachable_store,
    )
    .await;

    assert_ok(result);
    let store = AttachableStore::with_base(&crate::path_context::DaemonHostPath::new(temp.path()));
    let object_id = store
        .lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "mock-ref")
        .expect("workspace binding should exist");
    assert_eq!(object_id, set_id.as_str());
    let set = store.registry().sets.get(&set_id).expect("set should exist");
    assert_eq!(set.checkout, Some(QualifiedPath::from_host_path(&HostName::new("desktop"), PathBuf::from("/remote/feat"))));
}

#[tokio::test]
async fn create_workspace_for_checkout_selects_existing_workspace() {
    let checkout_path = PathBuf::from("/repo/wt-feat");
    let existing_workspace = Workspace { name: "feat".to_string(), correlation_keys: vec![], attachable_set_id: None };
    let ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:42".to_string(), existing_workspace)]));

    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    let runner = runner_ok();

    // Pre-populate the attachable store with a set for the checkout and a
    // workspace binding so the binding-based lookup finds it.
    let attachable_store = crate::attachable::shared_in_memory_attachable_store();
    {
        let mut store = attachable_store.lock().expect("lock store");
        let host = local_host();
        let checkout = QualifiedPath::from_host_path(&host, PathBuf::from("/repo/wt-feat"));
        let set_id = store.ensure_terminal_set(Some(host), Some(checkout));
        store.replace_binding(ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: "cmux".into(),
            object_kind: BindingObjectKind::AttachableSet,
            object_id: set_id.to_string(),
            external_ref: "workspace:42".into(),
        });
    }

    let result = run_build_plan_to_completion_with(
        CommandAction::CreateWorkspaceForCheckout { checkout_path, label: "feat".into() },
        registry,
        data,
        runner,
        repo_root(),
        config_base(),
        attachable_store,
    )
    .await;

    // Binding-based lookup finds the existing workspace and selects it
    // instead of creating a new one.
    assert_ok(result);
    let calls = ws_mgr.calls.lock().await;
    assert!(calls.iter().any(|c| c.starts_with("select_workspace")), "should select existing workspace, got: {calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("create_workspace")), "should NOT create workspace when binding exists, got: {calls:?}");
}

#[tokio::test]
async fn checkout_action_creates_workspace_after_checkout() {
    // Fresh checkout has no binding in the store, so a new workspace is
    // always created (binding-based lookup returns None).
    let ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:99".to_string(), Workspace {
        name: "feat-x".to_string(),
        correlation_keys: vec![],
        attachable_set_id: None,
    })]));

    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);
    let attachable_store = crate::attachable::shared_in_memory_attachable_store();

    let result = run_build_plan_to_completion_with(
        fresh_checkout_action("feat-x"),
        registry,
        empty_data(),
        runner,
        repo_root(),
        config_base(),
        attachable_store,
    )
    .await;

    assert_checkout_created_branch(result, "feat-x");
    let calls = ws_mgr.calls.lock().await;
    assert!(calls.iter().any(|c| c.starts_with("create_workspace")), "should create workspace, got: {calls:?}");
}

#[tokio::test]
async fn create_workspace_from_prepared_terminal_uses_local_fallback_for_remote_only_repo() {
    let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(&DaemonHostPath::new(temp.path()));
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let result = run_build_plan_to_completion_with(
        CommandAction::CreateWorkspaceFromPreparedTerminal {
            target_host: HostName::new("desktop"),
            branch: "feat".into(),
            checkout_path: PathBuf::from("/remote/feat"),
            attachable_set_id: None,
            commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash -l".into())] }],
        },
        registry,
        empty_data(),
        runner,
        ExecutionEnvironmentPath::new("<remote>/desktop/home/dev/repo"),
        DaemonHostPath::new(temp.path()),
        attachable_store,
    )
    .await;

    assert_ok(result);
    let created = workspace_manager.created_configs.lock().await;
    assert_eq!(created.len(), 1);
    assert!(!created[0].working_directory.as_path().to_string_lossy().starts_with("<remote>/"));
    assert!(created[0].working_directory.as_path().exists(), "fallback working directory should exist");
    let resolved = &created[0].attach_commands;
    assert!(resolved[0].1.contains("${SHELL:-/bin/sh} -l -c"), "expected login shell wrapper, got: {}", resolved[0].1);
}

#[tokio::test]
async fn teleport_session_creates_workspace_even_when_one_exists() {
    // Teleport must always create a new workspace because the attach command
    // is session-specific. Reusing an existing workspace would attach to
    // whatever session was there before, not the requested one.
    let checkout_path = PathBuf::from("/repo/wt-feat");
    let existing_workspace = Workspace { name: "feat".to_string(), correlation_keys: vec![], attachable_set_id: None };
    let ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:77".to_string(), existing_workspace)]));

    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    registry.workspace_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::TeleportSession {
            session_id: "sess-1".to_string(),
            branch: Some("feat".to_string()),
            checkout_key: Some(checkout_path),
        },
        registry,
        data,
        runner,
    )
    .await;

    assert_ok(result);
    let calls = ws_mgr.calls.lock().await;
    assert!(calls.iter().any(|c| c.starts_with("create_workspace")), "teleport should always create a new workspace, got: {calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("select_workspace")), "teleport should NOT select existing workspace, got: {calls:?}");
}

#[tokio::test]
async fn teleport_session_persists_workspace_binding() {
    let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(&DaemonHostPath::new(temp.path()));

    let result = run_build_plan_to_completion_with(
        CommandAction::TeleportSession {
            session_id: "sess-1".into(),
            branch: Some("feat".into()),
            checkout_key: Some(PathBuf::from("/repo/wt-feat")),
        },
        registry,
        data,
        runner,
        repo_root(),
        DaemonHostPath::new(temp.path()),
        attachable_store,
    )
    .await;

    assert_ok(result);
    let store = AttachableStore::with_base(&crate::path_context::DaemonHostPath::new(temp.path()));
    let object_id = store
        .lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "mock-ref")
        .expect("workspace binding should exist");
    let set = store.registry().sets.values().find(|set| set.id.as_str() == object_id).expect("set should exist");
    assert_eq!(set.checkout, Some(QualifiedPath::from_host_path(&local_host(), PathBuf::from("/repo/wt-feat"))));
}
// -----------------------------------------------------------------------
// Tests: SelectWorkspace
// -----------------------------------------------------------------------

#[tokio::test]
async fn select_workspace_no_manager() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::SelectWorkspace { ws_ref: "my-ws".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn select_workspace_success() {
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::SelectWorkspace { ws_ref: "my-ws".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn select_workspace_failure() {
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("select failed")));
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::SelectWorkspace { ws_ref: "bad-ws".to_string() }, registry, empty_data(), runner).await;

    assert_error_eq(result, "select failed");
}

// -----------------------------------------------------------------------
// Tests: CreateCheckout
// -----------------------------------------------------------------------

#[tokio::test]
async fn create_checkout_no_manager() {
    let registry = empty_registry();
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_build_plan_to_completion(fresh_checkout_action("feat-x"), registry, empty_data(), runner).await;

    assert_error_contains(result, "No checkout manager available");
}

#[tokio::test]
async fn create_checkout_success() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_build_plan_to_completion(fresh_checkout_action("feat-x"), registry, empty_data(), runner).await;

    assert_checkout_created_branch(result, "feat-x");
}

#[tokio::test]
async fn create_checkout_with_issue_ids_writes_git_config() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
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
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("branch already exists")));
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_build_plan_to_completion(fresh_checkout_action("feat-x"), registry, empty_data(), runner).await;

    assert_error_eq(result, "branch already exists");
}

#[tokio::test]
async fn create_checkout_success_ws_manager_fails_still_returns_created() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws failed")));
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
    data.checkouts.insert(hp("/repo/wt-old"), TestCheckout::new("old").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(remove_checkout_action("old"), registry, data, runner).await;

    assert_error_contains(result, "No checkout manager available");
}

#[tokio::test]
async fn remove_checkout_success() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-old"), TestCheckout::new("old").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(remove_checkout_action("old"), registry, data, runner).await;

    assert_checkout_removed_branch(result, "old");
}

#[tokio::test]
async fn remove_checkout_failure() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("cannot remove trunk")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-main"), TestCheckout::new("main").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(remove_checkout_action("main"), registry, data, runner).await;

    assert_error_eq(result, "cannot remove trunk");
}

#[tokio::test]
async fn remove_checkout_resolves_for_remote_host() {
    let remote = HostName::new("remote-box");
    let remote_hp = QualifiedPath::from_host_path(&remote, PathBuf::from("/repo/wt-feat"));
    let mut data = empty_data();
    data.checkouts.insert(remote_hp, TestCheckout::new("feat").build());

    let config_base = config_base();
    let plan = build_plan(
        command_with_host("remote-box", remove_checkout_action("feat")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(empty_registry()),
        Arc::new(data),
        config_base.clone(),
        test_attachable_store(&config_base),
        None,
        local_host(),
    )
    .await;

    let plan = plan.expect("build_plan should succeed for remote checkout");
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(HostName::new("remote-box")));
    assert!(
        matches!(&plan.steps[0].action, StepAction::RemoveCheckout { branch, .. } if branch == "feat"),
        "step should be RemoveCheckout for branch feat"
    );
}

#[tokio::test]
async fn remove_checkout_disambiguates_by_target_host() {
    // Same branch on two hosts — command.host should disambiguate
    let local = hp("/repo/wt-feat");
    let remote = QualifiedPath::from_host_path(&HostName::new("remote-box"), PathBuf::from("/repo/wt-feat"));
    let mut data = empty_data();
    data.checkouts.insert(local, TestCheckout::new("feat").build());
    data.checkouts.insert(remote, TestCheckout::new("feat").build());

    let config_base = config_base();
    let plan = build_plan(
        command_with_host("remote-box", remove_checkout_action("feat")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(empty_registry()),
        Arc::new(data),
        config_base.clone(),
        test_attachable_store(&config_base),
        None,
        local_host(),
    )
    .await;

    let plan = plan.expect("build_plan should not be ambiguous when command.host disambiguates");
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(HostName::new("remote-box")));
}

// -----------------------------------------------------------------------
// Tests: RemoveCheckout — terminal cleanup
// -----------------------------------------------------------------------

struct MockTerminalPool {
    killed: tokio::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl TerminalPool for MockTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<crate::providers::terminal::TerminalSession>, String> {
        Ok(vec![])
    }
    async fn ensure_session(
        &self,
        _session_name: &str,
        _cmd: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &crate::providers::terminal::TerminalEnvVars,
    ) -> Result<(), String> {
        Ok(())
    }
    fn attach_args(
        &self,
        session_name: &str,
        _cmd: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &crate::providers::terminal::TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        Ok(vec![flotilla_protocol::arg::Arg::Literal(format!("attach:{session_name}"))])
    }
    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        self.killed.lock().await.push(session_name.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn remove_checkout_succeeds_with_terminal_pool() {
    let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });

    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.terminal_pools.insert("shpool", desc("shpool"), Arc::clone(&mock_pool) as Arc<dyn TerminalPool>);
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-x"), TestCheckout::new("feat-x").build());

    let runner = runner_ok();
    let result = run_build_plan_to_completion(remove_checkout_action("feat-x"), registry, data, runner).await;

    assert_checkout_removed_branch(result, "feat-x");
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
    registry.issue_trackers.insert("github", desc("github"), Arc::new(MockIssueTracker));
    let runner = runner_ok();

    let result = run_build_plan_to_completion(CommandAction::OpenIssue { id: "10".to_string() }, registry, empty_data(), runner).await;

    assert_ok(result);
}

// -----------------------------------------------------------------------
// Tests: LinkIssuesToChangeRequest
// -----------------------------------------------------------------------

#[tokio::test]
async fn link_issues_success_with_existing_body() {
    let registry = empty_registry();
    // First call: gh pr view returns existing body
    // Second call: gh pr edit succeeds
    let runner = MockRunner::new(vec![Ok("Existing PR body".to_string()), Ok(String::new())]);

    let result = run_build_plan_to_completion(
        CommandAction::LinkIssuesToChangeRequest {
            change_request_id: "55".to_string(),
            issue_ids: vec!["10".to_string(), "20".to_string()],
        },
        registry,
        empty_data(),
        runner,
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn link_issues_success_with_empty_body() {
    let registry = empty_registry();
    let runner = MockRunner::new(vec![
        Ok("  \n".to_string()), // empty/whitespace body
        Ok(String::new()),      // edit succeeds
    ]);

    let result = run_build_plan_to_completion(
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
        registry,
        empty_data(),
        runner,
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn link_issues_view_fails() {
    let registry = empty_registry();
    let runner = MockRunner::new(vec![Err("gh not found".to_string())]);

    let result = run_build_plan_to_completion(
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
        registry,
        empty_data(),
        runner,
    )
    .await;

    assert_error_eq(result, "gh not found");
}

#[tokio::test]
async fn link_issues_edit_fails() {
    let registry = empty_registry();
    let runner = MockRunner::new(vec![Ok("body text".to_string()), Err("permission denied".to_string())]);

    let result = run_build_plan_to_completion(
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
        registry,
        empty_data(),
        runner,
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
    registry.issue_trackers.insert("github", desc("github"), Arc::new(MockIssueTracker));
    let mut data = empty_data();
    data.issues.insert("42".to_string(), TestIssue::new("Add login feature").build());
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
    let mut data = empty_data();
    data.issues.insert("42".to_string(), TestIssue::new("Add login").build());
    let runner = runner_ok();

    let result =
        run_build_plan_to_completion(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, registry, data, runner)
            .await;

    assert_branch_name_generated(result, "issue-42", &[("issues", "42")]);
}

#[tokio::test]
async fn generate_branch_name_no_ai_provider_uses_fallback() {
    let registry = empty_registry();
    let mut data = empty_data();
    data.issues.insert("7".to_string(), TestIssue::new("Fix bug").build());
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
    registry.issue_trackers.insert("github", desc("github"), Arc::new(MockIssueTracker));
    let mut data = empty_data();
    data.issues.insert("1".to_string(), TestIssue::new("Login feature").build());
    data.issues.insert("2".to_string(), TestIssue::new("Signup feature").build());
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
async fn generate_branch_name_unknown_issue_key() {
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

    // No issues found, so empty fallback
    assert_branch_name_generated(result, "", &[]);
}

// -----------------------------------------------------------------------
// Tests: TeleportSession
// -----------------------------------------------------------------------

#[tokio::test]
async fn teleport_session_with_checkout_key() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert(
        "claude",
        desc("claude"),
        Arc::new(MockCloudAgent::with_attach("claude --teleport")), // base; mock appends session_id
    );
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
        registry,
        data,
        runner,
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn teleport_session_uses_provider_specific_attach_command() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::with_attach("claude --teleport")));
    registry.cloud_agents.insert("cursor", desc("cursor"), Arc::new(MockCloudAgent::with_attach("agent --resume")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("cursor", "sess-1").build());
    let runner = runner_ok();

    let attach = resolve_attach_command("sess-1", &registry, &data).await.expect("resolve attach command");
    assert_eq!(attach, "agent --resume sess-1");

    let result = run_build_plan_to_completion(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
        registry,
        data,
        runner,
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn teleport_session_with_branch_creates_checkout() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat", "/repo/wt-feat")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: None },
        registry,
        data,
        runner,
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn teleport_session_no_path_no_branch() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: None, checkout_key: None },
        registry,
        data,
        runner,
    )
    .await;

    assert_error_contains(result, "checkout path not resolved by prior step");
}

#[tokio::test]
async fn teleport_session_ws_manager_fails() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws failed")));
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
        registry,
        data,
        runner,
    )
    .await;

    assert_error_eq(result, "ws failed");
}

#[tokio::test]
async fn teleport_session_uses_session_as_name_when_no_branch() {
    // When checkout_key is present but branch is None, uses "session" as name.
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let result = run_build_plan_to_completion(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: None, checkout_key: Some(path) },
        registry,
        data,
        runner,
    )
    .await;

    assert_ok(result);
}

// -----------------------------------------------------------------------
// Tests: Daemon-level commands rejected
// -----------------------------------------------------------------------

#[tokio::test]
async fn daemon_level_commands_return_error() {
    let daemon_commands = vec![
        CommandAction::TrackRepoPath { path: PathBuf::from("/repo") },
        CommandAction::UntrackRepo { repo: RepoSelector::Path(PathBuf::from("/repo")) },
        CommandAction::Refresh { repo: None },
        CommandAction::SetIssueViewport { repo: RepoSelector::Path(PathBuf::from("/repo")), visible_count: 10 },
        CommandAction::FetchMoreIssues { repo: RepoSelector::Path(PathBuf::from("/repo")), desired_count: 20 },
        CommandAction::SearchIssues { repo: RepoSelector::Path(PathBuf::from("/repo")), query: "bug".to_string() },
        CommandAction::ClearIssueSearch { repo: RepoSelector::Path(PathBuf::from("/repo")) },
    ];

    for cmd in daemon_commands {
        let result = run_build_plan(cmd, empty_registry(), empty_data(), runner_ok()).await;
        match result {
            Err(value) => assert_error_contains(value, "daemon-level command"),
            Ok(_) => panic!("expected Err for daemon-level command"),
        }
    }
}

// -----------------------------------------------------------------------
// Tests: workspace_config helper
// -----------------------------------------------------------------------

#[test]
fn workspace_config_builds_correct_struct() {
    let config = workspace_config(Path::new("/nonexistent-repo"), "my-branch", Path::new("/repo/wt"), "claude", config_base().as_path());

    assert_eq!(config.name, "my-branch");
    assert_eq!(config.working_directory, ExecutionEnvironmentPath::new("/repo/wt"));
    assert_eq!(config.template_vars.get("main_command"), Some(&"claude".to_string()));
    assert!(config.template_yaml.is_none(), "no template file should exist at test paths");
}

// -----------------------------------------------------------------------
// Helper to run build_plan with Arc-wrapped arguments
// -----------------------------------------------------------------------

async fn run_build_plan(
    action: CommandAction,
    registry: ProviderRegistry,
    providers_data: ProviderData,
    _runner: MockRunner,
) -> Result<crate::step::StepPlan, CommandValue> {
    let config_base = config_base();
    build_plan(
        local_command(action),
        RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root(),
        },
        Arc::new(registry),
        Arc::new(providers_data),
        config_base.clone(),
        test_attachable_store(&config_base),
        None,
        local_host(),
    )
    .await
}

async fn run_build_plan_to_completion(
    action: CommandAction,
    registry: ProviderRegistry,
    providers_data: ProviderData,
    runner: MockRunner,
) -> CommandValue {
    let config_base = config_base();
    let attachable_store = test_attachable_store(&config_base);
    run_build_plan_to_completion_with(action, registry, providers_data, runner, repo_root(), config_base, attachable_store).await
}

async fn run_build_plan_to_completion_with(
    action: CommandAction,
    registry: ProviderRegistry,
    providers_data: ProviderData,
    runner: MockRunner,
    root: ExecutionEnvironmentPath,
    config_base: DaemonHostPath,
    attachable_store: SharedAttachableStore,
) -> CommandValue {
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::step::run_step_plan;

    let local_host = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root };
    let registry = Arc::new(registry);
    let providers_data = Arc::new(providers_data);
    let runner: Arc<dyn CommandRunner> = Arc::new(runner);

    let plan = build_plan(
        local_command(action),
        repo.clone(),
        Arc::clone(&registry),
        Arc::clone(&providers_data),
        config_base.clone(),
        attachable_store.clone(),
        None,
        local_host.clone(),
    )
    .await;

    match plan {
        Err(result) => result,
        Ok(step_plan) => {
            let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
            let resolver = ExecutorStepResolver {
                repo,
                registry,
                providers_data,
                runner,
                config_base,
                attachable_store,
                daemon_socket_path: None,
                local_host: local_host.clone(),
                environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
                environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
            };
            run_step_plan(step_plan, 1, local_host, repo_identity(), repo_root(), cancel, tx, &resolver).await
        }
    }
}

#[tokio::test]
async fn remove_checkout_cascades_attachable_set_deletion() {
    let config_base = config_base();
    let attachable_store = crate::attachable::shared_in_memory_attachable_store();
    let host = local_host();

    // Pre-populate the store with a set and members
    {
        let mut store = attachable_store.lock().expect("lock store");
        let checkout_path = QualifiedPath::from_host_path(&host, "/repo/wt-feat-x");
        let set_id = store.ensure_terminal_set(Some(host.clone()), Some(checkout_path));
        store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat-x/shell/0",
            crate::attachable::TerminalPurpose { checkout: "feat-x".into(), role: "shell".into(), index: 0 },
            "bash",
            crate::path_context::ExecutionEnvironmentPath::new("/repo/wt-feat-x"),
            TerminalStatus::Running,
        );
    }

    let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.terminal_pools.insert("shpool", desc("shpool"), Arc::clone(&mock_pool) as Arc<dyn TerminalPool>);
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-x"), TestCheckout::new("feat-x").build());

    let runner = runner_ok();
    let result = run_build_plan_to_completion_with(
        remove_checkout_action("feat-x"),
        registry,
        data,
        runner,
        repo_root(),
        config_base,
        attachable_store.clone(),
    )
    .await;

    assert_checkout_removed_branch(result, "feat-x");

    // Verify set and members were removed from store
    {
        let store = attachable_store.lock().expect("lock store");
        assert!(store.registry().sets.is_empty(), "set should be removed");
        assert!(store.registry().attachables.is_empty(), "attachables should be removed");
        assert!(store.registry().bindings.is_empty(), "bindings should be removed");
    }

    // Verify terminal was killed via cascade (session name is the AttachableId, not the old flotilla/... format)
    let killed = mock_pool.killed.lock().await;
    assert_eq!(killed.len(), 1, "cascade should kill the terminal");
}

// -----------------------------------------------------------------------
// Tests: build_plan
// -----------------------------------------------------------------------

#[tokio::test]
async fn build_plan_create_checkout_returns_steps() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let data = empty_data();
    let runner = runner_ok();

    let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, data, runner).await;

    match plan {
        Ok(step_plan) => {
            assert_eq!(step_plan.steps.len(), 3, "checkout + prepare + attach steps");
            assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
            assert_eq!(step_plan.steps[1].description, "Prepare workspace for feat-x");
            assert_eq!(step_plan.steps[2].description, "Attach workspace");
        }
        Err(_) => panic!("expected Ok, got Err"),
    }
}

#[tokio::test]
async fn build_plan_create_checkout_uses_command_host_for_checkout_steps() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let data = empty_data();

    let plan = build_plan(
        command_with_host(remote_host().as_str(), fresh_checkout_action("feat-x")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        local_host(),
    )
    .await
    .expect("build plan");

    assert_eq!(plan.steps.len(), 3);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(remote_host()));
    assert_eq!(plan.steps[1].host, StepExecutionContext::Host(remote_host()));
    assert_eq!(plan.steps[2].host, StepExecutionContext::Host(local_host()));
}

#[tokio::test]
async fn build_plan_remote_checkout_with_issue_links_keeps_workspace_local() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));

    let plan = build_plan(
        command_with_host(remote_host().as_str(), CommandAction::Checkout {
            repo: repo_selector(),
            target: CheckoutTarget::FreshBranch("feat-x".to_string()),
            issue_ids: vec![("github".into(), "123".into())],
        }),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(empty_data()),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        local_host(),
    )
    .await
    .expect("build plan");

    assert_eq!(plan.steps.len(), 4);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(remote_host()));
    assert_eq!(plan.steps[1].host, StepExecutionContext::Host(remote_host()));
    assert_eq!(plan.steps[2].description, format!("Prepare workspace for feat-x@{}", remote_host()));
    assert_eq!(plan.steps[2].host, StepExecutionContext::Host(remote_host()));
    assert_eq!(plan.steps[3].description, "Attach workspace");
    assert_eq!(plan.steps[3].host, StepExecutionContext::Host(local_host()));
}

#[tokio::test]
async fn build_plan_create_checkout_treats_local_host_as_local() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let data = empty_data();
    let local = local_host();

    let plan = build_plan(
        command_with_host(local.as_str(), fresh_checkout_action("feat-x")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        local.clone(),
    )
    .await
    .expect("build plan");

    assert_eq!(plan.steps.len(), 3);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(local_host()));
    assert_eq!(plan.steps[1].host, StepExecutionContext::Host(local_host()));
    assert_eq!(plan.steps[2].host, StepExecutionContext::Host(local_host()));
}

#[tokio::test]
async fn build_plan_create_checkout_skips_existing() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    // Pre-populate with an existing checkout for the branch
    data.checkouts.insert(hp("/repo/wt-feat-x"), TestCheckout::new("feat-x").build());
    let runner = runner_ok();

    let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, data, runner).await;

    match plan {
        Ok(step_plan) => {
            assert_eq!(step_plan.steps.len(), 3, "checkout + prepare + attach steps");
            assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
            assert_eq!(step_plan.steps[1].description, "Prepare workspace for feat-x");
            assert_eq!(step_plan.steps[2].description, "Attach workspace");
        }
        Err(_) => panic!("expected Ok, got Err"),
    }
}

#[tokio::test]
async fn checkout_plan_includes_workspace_step() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));

    let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, empty_data(), runner_ok()).await;

    match plan {
        Ok(step_plan) => {
            assert_eq!(step_plan.steps.len(), 3, "expected checkout + prepare + attach steps");
            assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
            assert_eq!(step_plan.steps[1].description, "Prepare workspace for feat-x");
            assert_eq!(step_plan.steps[2].description, "Attach workspace");
        }
        Err(_) => panic!("expected Ok"),
    }
}

#[tokio::test]
async fn build_plan_prepare_terminal_uses_command_host_for_terminal_step() {
    let registry = empty_registry();
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());

    let plan = build_plan(
        command_with_host(remote_host().as_str(), CommandAction::PrepareTerminalForCheckout { checkout_path: path, commands: vec![] }),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        local_host(),
    )
    .await
    .expect("build plan");

    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(remote_host()));
}

#[tokio::test]
async fn build_plan_create_workspace_for_checkout_uses_prepare_and_attach_steps_locally() {
    let registry = empty_registry();
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());

    let plan = build_plan(
        local_command(CommandAction::CreateWorkspaceForCheckout { checkout_path: path.clone(), label: "feat".into() }),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        local_host(),
    )
    .await
    .expect("build plan");

    assert_eq!(plan.steps.len(), 2);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(local_host()));
    assert!(matches!(
        plan.steps[0].action,
        StepAction::PrepareWorkspace { ref checkout_path, ref label }
            if checkout_path == &Some(ExecutionEnvironmentPath::new(path.clone())) && label == "feat"
    ));
    assert_eq!(plan.steps[1].host, StepExecutionContext::Host(local_host()));
    assert!(matches!(plan.steps[1].action, StepAction::AttachWorkspace));
}

#[tokio::test]
async fn build_plan_create_workspace_for_checkout_uses_remote_prepare_and_local_attach() {
    let registry = empty_registry();
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(QualifiedPath::from_host_path(&remote_host(), path.clone()), TestCheckout::new("feat").build());

    let plan = build_plan(
        command_with_host(remote_host().as_str(), CommandAction::CreateWorkspaceForCheckout {
            checkout_path: path.clone(),
            label: "feat".into(),
        }),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        local_host(),
    )
    .await
    .expect("build plan");

    assert_eq!(plan.steps.len(), 2);
    assert_eq!(plan.steps[0].host, StepExecutionContext::Host(remote_host()));
    assert!(matches!(
        plan.steps[0].action,
        StepAction::PrepareWorkspace { ref checkout_path, ref label }
            if checkout_path == &Some(ExecutionEnvironmentPath::new(path.clone())) && label == &format!("feat@{}", remote_host())
    ));
    assert_eq!(plan.steps[1].host, StepExecutionContext::Host(local_host()));
    assert!(matches!(plan.steps[1].action, StepAction::AttachWorkspace));
}

#[tokio::test]
async fn checkout_plan_end_to_end_creates_workspace() {
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::step::run_step_plan;

    let ws_mgr = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = ProviderRegistry::new();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);
    let registry = Arc::new(registry);
    let runner: Arc<dyn CommandRunner> = Arc::new(MockRunner::new(vec![Err("missing".into()), Err("missing".into())]));
    let providers_data = Arc::new(empty_data());
    let cb = config_base();
    let attachable = test_attachable_store(&cb);
    let lh = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };

    let plan = build_plan(
        local_command(fresh_checkout_action("feat-x")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::clone(&registry),
        Arc::clone(&providers_data),
        cb.clone(),
        attachable.clone(),
        None,
        lh.clone(),
    )
    .await;

    let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
    let resolver = ExecutorStepResolver {
        repo,
        registry,
        providers_data,
        runner,
        config_base: cb,
        attachable_store: attachable,
        daemon_socket_path: None,
        local_host: lh.clone(),
        environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    let result = match plan {
        Ok(step_plan) => run_step_plan(step_plan, 1, lh, repo_identity(), repo_root(), cancel, tx, &resolver).await,
        _ => panic!("expected steps"),
    };

    assert!(matches!(result, CommandValue::CheckoutCreated { .. }));

    let calls = ws_mgr.calls.lock().await;
    assert!(
        calls.iter().any(|c| c.starts_with("create_workspace") || c.starts_with("select_workspace")),
        "should create or select workspace from prior outcome: {calls:?}"
    );
}

#[tokio::test]
async fn checkout_plan_creates_workspace_for_preexisting_checkout() {
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::step::run_step_plan;

    let ws_mgr = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = ProviderRegistry::new();
    // No checkout manager needed — checkout already exists
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);
    let registry = Arc::new(registry);
    // validate_checkout_target needs 2 responses: local ref check (Ok), remote ref check
    let runner: Arc<dyn CommandRunner> = Arc::new(MockRunner::new(vec![Ok("".into()), Err("missing".into())]));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-x"), TestCheckout::new("feat-x").build());
    let providers_data = Arc::new(data);
    let cb = config_base();
    let attachable = test_attachable_store(&cb);
    let lh = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };

    let plan = build_plan(
        local_command(existing_branch_checkout_action("feat-x")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::clone(&registry),
        Arc::clone(&providers_data),
        cb.clone(),
        attachable.clone(),
        None,
        lh.clone(),
    )
    .await;

    let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
    let resolver = ExecutorStepResolver {
        repo,
        registry,
        providers_data,
        runner,
        config_base: cb,
        attachable_store: attachable,
        daemon_socket_path: None,
        local_host: lh.clone(),
        environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    let result = match plan {
        Ok(step_plan) => run_step_plan(step_plan, 1, lh, repo_identity(), repo_root(), cancel, tx, &resolver).await,
        _ => panic!("expected steps"),
    };

    assert!(
        matches!(result, CommandValue::CheckoutCreated { ref branch, .. } if branch == "feat-x"),
        "should return CheckoutCreated for pre-existing checkout, got: {result:?}"
    );
    let calls = ws_mgr.calls.lock().await;
    assert!(
        calls.iter().any(|c| c.starts_with("select_workspace") || c.starts_with("create_workspace")),
        "should select or create workspace for pre-existing checkout: {calls:?}"
    );
}

#[tokio::test]
async fn checkout_plan_preserves_checkout_created_when_workspace_step_fails() {
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::step::run_step_plan;

    let ws_mgr = Arc::new(MockWorkspaceManager::failing("ws failed"));
    let mut registry = ProviderRegistry::new();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);
    let registry = Arc::new(registry);
    let runner: Arc<dyn CommandRunner> = Arc::new(MockRunner::new(vec![Err("missing".into()), Err("missing".into())]));
    let providers_data = Arc::new(empty_data());
    let cb = config_base();
    let attachable = test_attachable_store(&cb);
    let lh = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };

    let plan = build_plan(
        local_command(fresh_checkout_action("feat-x")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::clone(&registry),
        Arc::clone(&providers_data),
        cb.clone(),
        attachable.clone(),
        None,
        lh.clone(),
    )
    .await;

    let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
    let resolver = ExecutorStepResolver {
        repo,
        registry,
        providers_data,
        runner,
        config_base: cb,
        attachable_store: attachable,
        daemon_socket_path: None,
        local_host: lh.clone(),
        environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    let result = match plan {
        Ok(step_plan) => run_step_plan(step_plan, 1, lh, repo_identity(), repo_root(), cancel, tx, &resolver).await,
        _ => panic!("expected steps"),
    };

    assert_eq!(result, CommandValue::CheckoutCreated { branch: "feat-x".into(), path: PathBuf::from("/repo/wt-feat-x") });
}

#[tokio::test]
async fn build_plan_teleport_session_returns_steps() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat").build());
    data.sessions.insert("sess-1".to_string(), TestSession::new("test session").with_session_ref("claude", "sess-1").build());
    let runner = runner_ok();

    let plan = run_build_plan(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
        registry,
        data,
        runner,
    )
    .await;

    match plan {
        Ok(step_plan) => {
            // 3 steps: resolve attach, ensure checkout, create workspace
            assert_eq!(step_plan.steps.len(), 3, "expected 3 steps, got {}", step_plan.steps.len());
        }
        Err(_) => panic!("expected Ok, got Err"),
    }
}

#[tokio::test]
async fn build_plan_remove_checkout_returns_steps() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-old"), TestCheckout::new("old").build());
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
    let mut data = empty_data();
    data.issues.insert("42".to_string(), TestIssue::new("Add login feature").build());
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
    let mut data = empty_data();
    data.issues.insert("42".to_string(), TestIssue::new("Add login feature").build());
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

#[tokio::test]
async fn build_plan_with_environment_prepends_lifecycle_steps() {
    let registry = empty_registry();
    let data = empty_data();

    let cmd = Command {
        host: Some(remote_host()),
        provisioning_target: Some(flotilla_protocol::ProvisioningTarget::NewEnvironment {
            host: remote_host(),
            provider: "docker".to_string(),
        }),
        context_repo: Some(repo_selector()),
        action: CommandAction::Checkout {
            repo: repo_selector(),
            target: CheckoutTarget::FreshBranch("feature-x".to_string()),
            issue_ids: vec![],
        },
    };

    let plan = build_plan(
        cmd,
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        HostName::new("laptop"),
    )
    .await
    .expect("build_plan should succeed");

    // Verify 7 steps (ReadEnvironmentSpec prepended)
    assert_eq!(plan.steps.len(), 7);

    // Verify step actions in order
    assert!(matches!(plan.steps[0].action, StepAction::ReadEnvironmentSpec));
    assert!(matches!(plan.steps[1].action, StepAction::EnsureEnvironmentImage { .. }));
    assert!(matches!(plan.steps[2].action, StepAction::CreateEnvironment { .. }));
    assert!(matches!(plan.steps[3].action, StepAction::DiscoverEnvironmentProviders { .. }));
    assert!(matches!(plan.steps[4].action, StepAction::CreateCheckout { .. }));
    assert!(matches!(plan.steps[5].action, StepAction::PrepareWorkspace { .. }));
    assert!(matches!(plan.steps[6].action, StepAction::AttachWorkspace));

    // Verify host assignments — steps 0-3: Host(feta)
    assert_eq!(*plan.steps[0].host.host_name(), remote_host());
    assert_eq!(*plan.steps[1].host.host_name(), remote_host());
    assert_eq!(*plan.steps[2].host.host_name(), remote_host());
    assert_eq!(*plan.steps[3].host.host_name(), remote_host());
    assert!(matches!(&plan.steps[0].host, StepExecutionContext::Host(_)));
    assert!(matches!(&plan.steps[1].host, StepExecutionContext::Host(_)));
    assert!(matches!(&plan.steps[2].host, StepExecutionContext::Host(_)));
    assert!(matches!(&plan.steps[3].host, StepExecutionContext::Host(_)));

    // Steps 4-5: Environment(feta, env_id)
    assert!(matches!(&plan.steps[4].host, StepExecutionContext::Environment(h, _) if *h == remote_host()));
    assert!(matches!(&plan.steps[5].host, StepExecutionContext::Environment(h, _) if *h == remote_host()));

    // Step 6: Host(laptop) — attach on local
    assert_eq!(*plan.steps[6].host.host_name(), HostName::new("laptop"));
    assert!(matches!(&plan.steps[6].host, StepExecutionContext::Host(_)));

    // Verify workspace label includes remote host suffix
    if let StepAction::PrepareWorkspace { ref label, .. } = plan.steps[5].action {
        assert_eq!(label, "feature-x@test-remote");
    } else {
        panic!("step 5 should be PrepareWorkspace");
    }
}

#[tokio::test]
async fn build_plan_with_environment_local_host_omits_suffix() {
    let registry = empty_registry();
    let data = empty_data();

    let cmd = Command {
        host: Some(HostName::new("laptop")),
        provisioning_target: Some(flotilla_protocol::ProvisioningTarget::NewEnvironment {
            host: HostName::new("laptop"),
            provider: "docker".to_string(),
        }),
        context_repo: Some(repo_selector()),
        action: CommandAction::Checkout { repo: repo_selector(), target: CheckoutTarget::Branch("main".to_string()), issue_ids: vec![] },
    };

    let plan = build_plan(
        cmd,
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        HostName::new("laptop"),
    )
    .await
    .expect("build_plan should succeed");

    assert_eq!(plan.steps.len(), 7);

    // When target_host == local_host, workspace label should be just the branch
    if let StepAction::PrepareWorkspace { ref label, .. } = plan.steps[5].action {
        assert_eq!(label, "main");
    } else {
        panic!("step 5 should be PrepareWorkspace");
    }

    // Checkout step should use ExistingBranch intent
    if let StepAction::CreateCheckout { ref branch, create_branch, intent, .. } = plan.steps[4].action {
        assert_eq!(branch, "main");
        assert!(!create_branch);
        assert_eq!(intent, CheckoutIntent::ExistingBranch);
    } else {
        panic!("step 4 should be CreateCheckout");
    }
}

#[tokio::test]
async fn build_plan_with_existing_environment_returns_4_steps() {
    let registry = empty_registry();
    let data = empty_data();

    let cmd = Command {
        host: Some(remote_host()),
        provisioning_target: Some(flotilla_protocol::ProvisioningTarget::ExistingEnvironment {
            host: remote_host(),
            env_id: flotilla_protocol::EnvironmentId::new("env-abc"),
        }),
        context_repo: Some(repo_selector()),
        action: CommandAction::Checkout {
            repo: repo_selector(),
            target: CheckoutTarget::FreshBranch("feature-x".to_string()),
            issue_ids: vec![],
        },
    };

    let plan = build_plan(
        cmd,
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        HostName::new("laptop"),
    )
    .await
    .expect("build_plan should succeed");

    assert_eq!(plan.steps.len(), 4, "existing-environment checkout should have 4 steps");

    assert!(matches!(plan.steps[0].action, StepAction::DiscoverEnvironmentProviders { .. }));
    assert!(matches!(plan.steps[1].action, StepAction::CreateCheckout { .. }));
    assert!(matches!(plan.steps[2].action, StepAction::PrepareWorkspace { .. }));
    assert!(matches!(plan.steps[3].action, StepAction::AttachWorkspace));

    // Steps 0 executes on Host(feta)
    assert_eq!(*plan.steps[0].host.host_name(), remote_host());
    assert!(matches!(&plan.steps[0].host, StepExecutionContext::Host(_)));

    // Steps 1-2 execute in Environment(feta, env_id)
    assert!(matches!(&plan.steps[1].host, StepExecutionContext::Environment(h, _) if *h == remote_host()));
    assert!(matches!(&plan.steps[2].host, StepExecutionContext::Environment(h, _) if *h == remote_host()));

    // Step 3 attaches on the local host
    assert_eq!(*plan.steps[3].host.host_name(), HostName::new("laptop"));
    assert!(matches!(&plan.steps[3].host, StepExecutionContext::Host(_)));
}

#[tokio::test]
async fn build_plan_with_host_target_returns_standard_checkout_plan() {
    let registry = empty_registry();
    let data = empty_data();

    // ProvisioningTarget::Host should fall through to the standard checkout plan
    let cmd = Command {
        host: Some(remote_host()),
        provisioning_target: Some(flotilla_protocol::ProvisioningTarget::Host { host: remote_host() }),
        context_repo: Some(repo_selector()),
        action: CommandAction::Checkout {
            repo: repo_selector(),
            target: CheckoutTarget::FreshBranch("feature-x".to_string()),
            issue_ids: vec![],
        },
    };

    let plan = build_plan(
        cmd,
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        HostName::new("laptop"),
    )
    .await
    .expect("build_plan should succeed");

    // Standard checkout plan: CreateCheckout + PrepareWorkspace + AttachWorkspace
    assert_eq!(plan.steps.len(), 3, "host-target checkout should have 3 steps");

    assert!(matches!(plan.steps[0].action, StepAction::CreateCheckout { .. }));
    assert!(matches!(plan.steps[1].action, StepAction::PrepareWorkspace { .. }));
    assert!(matches!(plan.steps[2].action, StepAction::AttachWorkspace));

    // CreateCheckout and PrepareWorkspace run on the target host
    assert_eq!(*plan.steps[0].host.host_name(), remote_host());
    assert_eq!(*plan.steps[1].host.host_name(), remote_host());
    // AttachWorkspace runs on the local host
    assert_eq!(*plan.steps[2].host.host_name(), HostName::new("laptop"));
}

#[tokio::test]
async fn build_plan_with_no_provisioning_target_returns_standard_checkout_plan() {
    let registry = empty_registry();
    let data = empty_data();

    // No provisioning_target should also produce the standard checkout plan
    let cmd = Command {
        host: Some(remote_host()),
        provisioning_target: None,
        context_repo: Some(repo_selector()),
        action: CommandAction::Checkout {
            repo: repo_selector(),
            target: CheckoutTarget::FreshBranch("feature-x".to_string()),
            issue_ids: vec![],
        },
    };

    let plan = build_plan(
        cmd,
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::new(registry),
        Arc::new(data),
        config_base(),
        test_attachable_store(&config_base()),
        None,
        HostName::new("laptop"),
    )
    .await
    .expect("build_plan should succeed");

    assert_eq!(plan.steps.len(), 3, "no-target checkout should have 3 steps");

    assert!(matches!(plan.steps[0].action, StepAction::CreateCheckout { .. }));
    assert!(matches!(plan.steps[1].action, StepAction::PrepareWorkspace { .. }));
    assert!(matches!(plan.steps[2].action, StepAction::AttachWorkspace));
}

// -----------------------------------------------------------------------
// Tests: resolve_checkout_branch
// -----------------------------------------------------------------------

#[test]
fn resolve_checkout_branch_path_found() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat-branch").build());
    let local_host = local_host();

    let result = resolve_checkout_branch(&CheckoutSelector::Path(PathBuf::from("/repo/wt-feat")), &data, &local_host);

    assert_eq!(result.expect("path lookup should succeed"), "feat-branch");
}

#[test]
fn resolve_checkout_branch_path_not_found() {
    let data = empty_data();
    let local_host = local_host();

    let result = resolve_checkout_branch(&CheckoutSelector::Path(PathBuf::from("/nonexistent")), &data, &local_host);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("checkout not found"));
}

#[test]
fn resolve_checkout_branch_query_exact_match() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat-login").build());
    let local_host = local_host();

    let result = resolve_checkout_branch(&CheckoutSelector::Query("feat-login".to_string()), &data, &local_host);

    assert_eq!(result.expect("exact query should match"), "feat-login");
}

#[test]
fn resolve_checkout_branch_query_substring_match() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat-login-page").build());
    let local_host = local_host();

    let result = resolve_checkout_branch(&CheckoutSelector::Query("login".to_string()), &data, &local_host);

    assert_eq!(result.expect("substring query should match"), "feat-login-page");
}

#[test]
fn resolve_checkout_branch_query_not_found() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), TestCheckout::new("feat-login").build());
    let local_host = local_host();

    let result = resolve_checkout_branch(&CheckoutSelector::Query("nonexistent".to_string()), &data, &local_host);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("checkout not found"));
}

#[test]
fn resolve_checkout_branch_query_ambiguous() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-a"), TestCheckout::new("feat-a").build());
    data.checkouts.insert(hp("/repo/wt-feat-b"), TestCheckout::new("feat-b").build());
    let local_host = local_host();

    let result = resolve_checkout_branch(&CheckoutSelector::Query("feat".to_string()), &data, &local_host);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("ambiguous"));
}

// -----------------------------------------------------------------------
// Tests: resolve_workspace_commands via TerminalPreparationService
// -----------------------------------------------------------------------

#[tokio::test]
async fn resolve_workspace_commands_no_template_uses_default() {
    let mock_pool: Arc<dyn TerminalPool> = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let store = crate::attachable::shared_in_memory_attachable_store();
    let tm = crate::terminal_manager::TerminalManager::new(mock_pool, store, local_host());
    let mut config = WorkspaceConfig {
        name: "test-branch".to_string(),
        working_directory: ExecutionEnvironmentPath::new("/repo/wt"),
        template_vars: [("main_command".to_string(), "claude".to_string())].into_iter().collect(),
        template_yaml: None,
        resolved_commands: None,
    };

    let host = local_host();
    let service = super::terminals::TerminalPreparationService::new(&tm, None, &host);
    service.resolve_workspace_commands(&mut config).await;

    // Default template has one "main" terminal entry
    assert!(config.resolved_commands.is_some());
    let commands = config.resolved_commands.expect("default template should produce resolved commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "main");
}

#[tokio::test]
async fn resolve_workspace_commands_skips_non_terminal_content() {
    let mock_pool: Arc<dyn TerminalPool> = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let store = crate::attachable::shared_in_memory_attachable_store();
    let tm = crate::terminal_manager::TerminalManager::new(mock_pool, store, local_host());
    let yaml = r#"
content:
  - role: docs
    type: webview
    command: "http://localhost:3000"
"#;
    let mut config = WorkspaceConfig {
        name: "test-branch".to_string(),
        working_directory: ExecutionEnvironmentPath::new("/repo/wt"),
        template_vars: std::collections::HashMap::new(),
        template_yaml: Some(yaml.to_string()),
        resolved_commands: None,
    };

    let host = local_host();
    let service = super::terminals::TerminalPreparationService::new(&tm, None, &host);
    service.resolve_workspace_commands(&mut config).await;

    // All content entries were non-terminal, so resolved_commands stays None
    assert!(config.resolved_commands.is_none());
}

#[tokio::test]
async fn prepare_terminal_commands_wraps_requested_commands_via_terminal_manager() {
    let mock_pool: Arc<dyn TerminalPool> = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let store = crate::attachable::shared_in_memory_attachable_store();
    let tm = crate::terminal_manager::TerminalManager::new(mock_pool, store, local_host());

    let host = local_host();
    let service = super::terminals::TerminalPreparationService::new(&tm, None, &host);
    let requested = vec![PreparedTerminalCommand { role: "main".into(), command: "claude".into() }, PreparedTerminalCommand {
        role: "main".into(),
        command: "bash".into(),
    }];

    let result = service
        .prepare_terminal_commands("feat", Path::new("/repo/wt"), &requested, || panic!("workspace config should not be built"))
        .await
        .expect("prepare requested terminal commands");

    // Both commands should be resolved through the terminal manager
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].role, "main");
    assert_eq!(result[1].role, "main");
    // Args should contain structured Arg from attach_args(), not Literal-wrapped strings
    let flat = flotilla_protocol::arg::flatten(&result[0].args, 0);
    assert!(flat.starts_with("attach:"), "expected attach: prefix, got: {flat}");
}

#[tokio::test]
async fn resolve_workspace_commands_invalid_template_uses_default() {
    let mock_pool: Arc<dyn TerminalPool> = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let store = crate::attachable::shared_in_memory_attachable_store();
    let tm = crate::terminal_manager::TerminalManager::new(mock_pool, store, local_host());
    let mut config = WorkspaceConfig {
        name: "test-branch".to_string(),
        working_directory: ExecutionEnvironmentPath::new("/repo/wt"),
        template_vars: [("main_command".to_string(), "claude".to_string())].into_iter().collect(),
        template_yaml: Some("content: [".to_string()),
        resolved_commands: None,
    };

    let host = local_host();
    let service = super::terminals::TerminalPreparationService::new(&tm, None, &host);
    service.resolve_workspace_commands(&mut config).await;

    let commands = config.resolved_commands.expect("invalid template should fall back to default template");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "main");
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
// Tests: validate_checkout_target
// -----------------------------------------------------------------------

#[tokio::test]
async fn validate_fresh_branch_succeeds_when_neither_exists() {
    // local check -> Err (not found), remote check -> Err (not found)
    let runner = MockRunner::new(vec![Err("not found".to_string()), Err("not found".to_string())]);

    let result = validate_checkout_target(repo_root().as_path(), "new-branch", CheckoutIntent::FreshBranch, &runner).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn validate_fresh_branch_fails_when_local_exists() {
    // local check -> Ok (found), remote check -> Err (not found)
    let runner = MockRunner::new(vec![Ok(String::new()), Err("not found".to_string())]);

    let result = validate_checkout_target(repo_root().as_path(), "existing", CheckoutIntent::FreshBranch, &runner).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("already exists"));
}

#[tokio::test]
async fn validate_fresh_branch_fails_when_remote_exists() {
    // local check -> Err (not found), remote check -> Ok (found)
    let runner = MockRunner::new(vec![Err("not found".to_string()), Ok(String::new())]);

    let result = validate_checkout_target(repo_root().as_path(), "remote-only", CheckoutIntent::FreshBranch, &runner).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("already exists"));
}

#[tokio::test]
async fn validate_existing_branch_succeeds_when_local_exists() {
    // local check -> Ok (found), remote check -> Err (not found)
    let runner = MockRunner::new(vec![Ok(String::new()), Err("not found".to_string())]);

    let result = validate_checkout_target(repo_root().as_path(), "local-branch", CheckoutIntent::ExistingBranch, &runner).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn validate_existing_branch_succeeds_when_remote_exists() {
    // local check -> Err (not found), remote check -> Ok (found)
    let runner = MockRunner::new(vec![Err("not found".to_string()), Ok(String::new())]);

    let result = validate_checkout_target(repo_root().as_path(), "remote-branch", CheckoutIntent::ExistingBranch, &runner).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn validate_existing_branch_fails_when_neither_exists() {
    // local check -> Err (not found), remote check -> Err (not found)
    let runner = MockRunner::new(vec![Err("not found".to_string()), Err("not found".to_string())]);

    let result = validate_checkout_target(repo_root().as_path(), "ghost-branch", CheckoutIntent::ExistingBranch, &runner).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("branch not found"));
}

// -----------------------------------------------------------------------
// Tests: ExecutorStepResolver
// -----------------------------------------------------------------------

#[tokio::test]
async fn executor_step_resolver_prepare_workspace_produces_prepared_workspace() {
    let config_base = config_base();
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(empty_registry()),
        providers_data: Arc::new(empty_data()),
        runner: Arc::new(runner_ok()),
        config_base: config_base.clone(),
        attachable_store: test_attachable_store(&config_base),
        daemon_socket_path: None,
        local_host: local_host(),
        environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    let prior =
        vec![StepOutcome::CompletedWith(CommandValue::CheckoutCreated { branch: "feat".into(), path: PathBuf::from("/repo/wt-feat") })];
    let action = StepAction::PrepareWorkspace { label: "feat".into(), checkout_path: None };
    let context = StepExecutionContext::Host(local_host());
    let outcome = resolver.resolve("create workspace", &context, action, &prior).await;
    match outcome {
        Ok(StepOutcome::Produced(CommandValue::PreparedWorkspace(prepared))) => {
            assert_eq!(prepared.label, "feat");
            assert_eq!(prepared.target_host, local_host());
            assert_eq!(prepared.checkout_path, PathBuf::from("/repo/wt-feat"));
            assert!(!prepared.prepared_commands.is_empty(), "default workspace template should produce commands");
        }
        other => panic!("expected PreparedWorkspace outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn executor_step_resolver_prepare_workspace_skips_when_no_checkout_path() {
    let config_base = config_base();
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(empty_registry()),
        providers_data: Arc::new(empty_data()),
        runner: Arc::new(runner_ok()),
        config_base: config_base.clone(),
        attachable_store: test_attachable_store(&config_base),
        daemon_socket_path: None,
        local_host: local_host(),
        environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    let action = StepAction::PrepareWorkspace { label: "feat".into(), checkout_path: None };
    let context = StepExecutionContext::Host(local_host());
    let outcome = resolver.resolve("create workspace", &context, action, &[]).await;
    assert!(matches!(outcome, Ok(StepOutcome::Skipped)), "should skip when no prior CheckoutCreated outcome: {outcome:?}");
}

// -----------------------------------------------------------------------
// Tests: Environment lifecycle actions
// -----------------------------------------------------------------------

use flotilla_protocol::{EnvironmentId, EnvironmentSpec, EnvironmentStatus, ImageId};

use crate::providers::environment::{CreateOpts, EnvironmentHandle, EnvironmentProvider, ProvisionedEnvironment};

struct MockEnvironmentProvider {
    ensure_image_results: tokio::sync::Mutex<Vec<Result<ImageId, String>>>,
    create_results: tokio::sync::Mutex<Vec<Result<EnvironmentHandle, String>>>,
}

#[async_trait]
impl EnvironmentProvider for MockEnvironmentProvider {
    async fn ensure_image(&self, _spec: &EnvironmentSpec, _repo_root: &std::path::Path) -> Result<ImageId, String> {
        self.ensure_image_results.lock().await.remove(0)
    }
    async fn create(&self, _id: EnvironmentId, _image: &ImageId, _opts: CreateOpts) -> Result<EnvironmentHandle, String> {
        self.create_results.lock().await.remove(0)
    }
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Ok(vec![])
    }
}

struct MockProvisionedEnvironment {
    id: EnvironmentId,
    image: ImageId,
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
    async fn status(&self) -> Result<EnvironmentStatus, String> {
        Ok(EnvironmentStatus::Running)
    }
    async fn env_vars(&self) -> Result<std::collections::HashMap<String, String>, String> {
        let mut vars = std::collections::HashMap::new();
        vars.insert("PATH".to_string(), "/usr/bin".to_string());
        Ok(vars)
    }
    fn runner(&self, host_runner: Arc<dyn CommandRunner>) -> Arc<dyn CommandRunner> {
        host_runner
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

#[tokio::test]
async fn executor_step_resolver_ensure_environment_image() {
    let config_base = config_base();
    let provider = Arc::new(MockEnvironmentProvider {
        ensure_image_results: tokio::sync::Mutex::new(vec![Ok(ImageId::new("flotilla:test-abc123"))]),
        create_results: tokio::sync::Mutex::new(vec![]),
    });
    let registry = registry_with_env_provider(provider);
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(registry),
        providers_data: Arc::new(empty_data()),
        runner: Arc::new(runner_ok()),
        config_base: config_base.clone(),
        attachable_store: test_attachable_store(&config_base),
        daemon_socket_path: None,
        local_host: local_host(),
        environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    // Spec is now read from the prior ReadEnvironmentSpec step outcome
    let spec = EnvironmentSpec { image: flotilla_protocol::ImageSource::Registry("test:latest".into()), token_env_vars: vec![] };
    let prior = vec![StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec })];
    let action = StepAction::EnsureEnvironmentImage { provider: "docker".into() };
    let context = StepExecutionContext::Host(local_host());
    let outcome = resolver.resolve("ensure image", &context, action, &prior).await;
    match outcome {
        Ok(StepOutcome::Produced(CommandValue::ImageEnsured { image })) => {
            assert_eq!(image, ImageId::new("flotilla:test-abc123"));
        }
        other => panic!("expected ImageEnsured outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn executor_step_resolver_ensure_environment_image_error_when_no_provider() {
    let config_base = config_base();
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(empty_registry()),
        providers_data: Arc::new(empty_data()),
        runner: Arc::new(runner_ok()),
        config_base: config_base.clone(),
        attachable_store: test_attachable_store(&config_base),
        daemon_socket_path: None,
        local_host: local_host(),
        environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    // Spec is now read from the prior ReadEnvironmentSpec step outcome
    let spec = EnvironmentSpec { image: flotilla_protocol::ImageSource::Registry("test:latest".into()), token_env_vars: vec![] };
    let prior = vec![StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec })];
    let action = StepAction::EnsureEnvironmentImage { provider: "docker".into() };
    let context = StepExecutionContext::Host(local_host());
    let outcome = resolver.resolve("ensure image", &context, action, &prior).await;
    assert!(outcome.is_err(), "should fail when no environment provider available");
    assert!(outcome.unwrap_err().contains("environment provider not available"));
}

#[tokio::test]
async fn executor_step_resolver_create_environment() {
    let config_base = config_base();
    let env_id = EnvironmentId::new("env-test-1");
    let image_id = ImageId::new("flotilla:test-abc123");

    let mock_env: EnvironmentHandle = Arc::new(MockProvisionedEnvironment { id: env_id.clone(), image: image_id.clone() });

    let provider = Arc::new(MockEnvironmentProvider {
        ensure_image_results: tokio::sync::Mutex::new(vec![]),
        create_results: tokio::sync::Mutex::new(vec![Ok(mock_env)]),
    });
    let registry = registry_with_env_provider(provider);
    // resolve_reference_repo calls `git rev-parse --git-common-dir`
    let runner = Arc::new(MockRunner::new(vec![Ok("/tmp/test-repo/.git".into())]));
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(registry),
        providers_data: Arc::new(empty_data()),
        runner,
        config_base: config_base.clone(),
        attachable_store: test_attachable_store(&config_base),
        daemon_socket_path: Some(DaemonHostPath::new("/tmp/flotilla.sock")),
        local_host: local_host(),
        environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    // Prior step must have produced the image
    let prior = vec![StepOutcome::Produced(CommandValue::ImageEnsured { image: image_id.clone() })];
    let action = StepAction::CreateEnvironment { env_id: env_id.clone(), provider: "docker".into(), image: None };
    let context = StepExecutionContext::Host(local_host());
    let outcome = resolver.resolve("create env", &context, action, &prior).await;
    match outcome {
        Ok(StepOutcome::Produced(CommandValue::EnvironmentCreated { env_id: created_id })) => {
            assert_eq!(created_id, env_id);
        }
        other => panic!("expected EnvironmentCreated outcome, got {other:?}"),
    }

    // Verify handle was stored
    let handles = resolver.environment_handles.lock().expect("lock");
    assert!(handles.contains_key(&env_id), "environment handle should be stored");
}

#[tokio::test]
async fn executor_step_resolver_destroy_environment() {
    let config_base = config_base();
    let env_id = EnvironmentId::new("env-destroy-1");
    let image_id = ImageId::new("flotilla:test");

    let mock_env: EnvironmentHandle = Arc::new(MockProvisionedEnvironment { id: env_id.clone(), image: image_id });

    // Pre-populate the handle
    let mut handles_map = std::collections::HashMap::new();
    handles_map.insert(env_id.clone(), mock_env);

    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(empty_registry()),
        providers_data: Arc::new(empty_data()),
        runner: Arc::new(runner_ok()),
        config_base: config_base.clone(),
        attachable_store: test_attachable_store(&config_base),
        daemon_socket_path: None,
        local_host: local_host(),
        environment_handles: std::sync::Mutex::new(handles_map),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    let action = StepAction::DestroyEnvironment { env_id: env_id.clone() };
    let context = StepExecutionContext::Host(local_host());
    let outcome = resolver.resolve("destroy env", &context, action, &[]).await;
    assert!(matches!(outcome, Ok(StepOutcome::Completed)), "destroy should complete: {outcome:?}");

    // Verify handle was removed
    let handles = resolver.environment_handles.lock().expect("lock");
    assert!(!handles.contains_key(&env_id), "environment handle should be removed after destroy");
}

#[tokio::test]
async fn executor_step_resolver_destroy_environment_not_found() {
    let config_base = config_base();
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(empty_registry()),
        providers_data: Arc::new(empty_data()),
        runner: Arc::new(runner_ok()),
        config_base: config_base.clone(),
        attachable_store: test_attachable_store(&config_base),
        daemon_socket_path: None,
        local_host: local_host(),
        environment_handles: std::sync::Mutex::new(std::collections::HashMap::new()),
        environment_registries: std::sync::Mutex::new(std::collections::HashMap::new()),
    };

    let action = StepAction::DestroyEnvironment { env_id: EnvironmentId::new("nonexistent") };
    let context = StepExecutionContext::Host(local_host());
    let outcome = resolver.resolve("destroy env", &context, action, &[]).await;
    assert!(outcome.is_err(), "should fail when handle not found");
    assert!(outcome.unwrap_err().contains("environment handle not found"));
}
