use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use super::{
    build_plan,
    checkout::{resolve_checkout_branch, validate_checkout_target, write_branch_issue_links, CheckoutIntent},
    execute,
    session_actions::resolve_attach_command,
    terminals::{build_terminal_env_vars, escape_for_double_quotes, resolve_terminal_pool, wrap_remote_attach_commands},
    workspace_config, ExecutionPlan, ExecutorStepResolver, RepoExecutionContext,
};
use crate::{
    attachable::{AttachableStore, BindingObjectKind, SharedAttachableStore},
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
    },
    step::{StepAction, StepOutcome, StepResolver},
};

fn desc(name: &str) -> ProviderDescriptor {
    ProviderDescriptor::named(ProviderCategory::Vcs, name)
}
use async_trait::async_trait;
use flotilla_protocol::{
    CheckoutSelector, CheckoutTarget, Command, CommandAction, CommandResult, HostName, HostPath, ManagedTerminalId,
    PreparedTerminalCommand, RepoSelector,
};

fn hp(path: &str) -> HostPath {
    HostPath::new(HostName::local(), PathBuf::from(path))
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
    async fn list_checkouts(&self, _repo_root: &Path) -> Result<Vec<(PathBuf, Checkout)>, String> {
        Ok(vec![])
    }
    async fn create_checkout(&self, _repo_root: &Path, _branch: &str, _create_branch: bool) -> Result<(PathBuf, Checkout), String> {
        self.create_result.lock().await.take().expect("create_checkout called more than expected")
    }
    async fn remove_checkout(&self, _repo_root: &Path, _branch: &str) -> Result<(), String> {
        self.remove_result.lock().await.take().expect("remove_checkout called more than expected")
    }
}

/// A mock WorkspaceManager that records calls and returns configurable results.
struct MockWorkspaceManager {
    existing: Vec<(String, Workspace)>,
    create_result: tokio::sync::Mutex<Result<(), String>>,
    select_result: tokio::sync::Mutex<Result<(), String>>,
    created_configs: tokio::sync::Mutex<Vec<WorkspaceConfig>>,
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
    async fn create_workspace(&self, config: &WorkspaceConfig) -> Result<(String, Workspace), String> {
        self.created_configs.lock().await.push(config.clone());
        self.calls.lock().await.push(format!("create_workspace:{}", config.name));
        let result = self.create_result.lock().await;
        match &*result {
            Ok(()) => Ok(("mock-ref".to_string(), Workspace {
                name: config.name.clone(),
                directories: vec![],
                correlation_keys: vec![],
                attachable_set_id: None,
            })),
            Err(e) => Err(e.clone()),
        }
    }
    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        self.calls.lock().await.push(format!("select_workspace:{ws_ref}"));
        let result = self.select_result.lock().await;
        result.clone()
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

fn repo_root() -> PathBuf {
    PathBuf::from("/tmp/test-repo")
}

fn config_base() -> PathBuf {
    PathBuf::from("/tmp/test-config")
}

fn make_checkout(branch: &str, _path: &str) -> Checkout {
    Checkout {
        branch: branch.to_string(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
    }
}

fn make_session_for(provider: &str, id: &str) -> CloudAgentSession {
    CloudAgentSession {
        title: "test session".to_string(),
        status: SessionStatus::Running,
        model: None,
        updated_at: None,
        correlation_keys: vec![CorrelationKey::SessionRef(provider.to_string(), id.to_string())],
        provider_name: String::new(),
        provider_display_name: String::new(),
        item_noun: String::new(),
    }
}

fn make_issue(_id: &str, title: &str) -> Issue {
    Issue {
        title: title.to_string(),
        labels: vec![],
        association_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
    }
}

fn runner_ok() -> MockRunner {
    MockRunner::new(vec![])
}

fn repo_selector() -> RepoSelector {
    RepoSelector::Path(repo_root())
}

fn local_command(action: CommandAction) -> Command {
    Command { host: None, context_repo: None, action }
}

fn local_host() -> HostName {
    HostName::local()
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

fn remove_checkout_action(branch: &str, terminal_keys: Vec<ManagedTerminalId>) -> CommandAction {
    CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query(branch.to_string()), terminal_keys }
}

fn test_attachable_store(base: &Path) -> SharedAttachableStore {
    crate::attachable::shared_file_backed_attachable_store(base)
}

async fn run_execute(
    action: CommandAction,
    registry: &ProviderRegistry,
    providers_data: &ProviderData,
    runner: &MockRunner,
) -> CommandResult {
    let repo = RepoExecutionContext {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        root: repo_root(),
    };
    let config_base = config_base();
    let attachable_store = test_attachable_store(&config_base);
    execute(action, &repo, registry, providers_data, runner, &config_base, &attachable_store, None, &local_host()).await
}

fn assert_error_contains(result: CommandResult, expected_substring: &str) {
    match result {
        CommandResult::Error { message } => {
            assert!(message.contains(expected_substring), "expected error containing {expected_substring:?}, got {message:?}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

fn assert_error_eq(result: CommandResult, expected: &str) {
    match result {
        CommandResult::Error { message } => assert_eq!(message, expected),
        other => panic!("expected Error, got {:?}", other),
    }
}

fn assert_checkout_created_branch(result: CommandResult, expected_branch: &str) {
    match result {
        CommandResult::CheckoutCreated { branch, .. } => {
            assert_eq!(branch, expected_branch);
        }
        other => panic!("expected CheckoutCreated, got {:?}", other),
    }
}

fn assert_checkout_status_branch(result: CommandResult, expected_branch: &str) {
    match result {
        CommandResult::CheckoutStatus(info) => {
            assert_eq!(info.branch, expected_branch);
        }
        other => panic!("expected CheckoutStatus, got {:?}", other),
    }
}

fn assert_checkout_removed_branch(result: CommandResult, expected_branch: &str) {
    match result {
        CommandResult::CheckoutRemoved { branch } => {
            assert_eq!(branch, expected_branch);
        }
        other => panic!("expected CheckoutRemoved, got {:?}", other),
    }
}

fn assert_branch_name_generated(result: CommandResult, expected_name: &str, expected_issue_ids: &[(&str, &str)]) {
    match result {
        CommandResult::BranchNameGenerated { name, issue_ids } => {
            assert_eq!(name, expected_name);
            let expected_issue_ids: Vec<_> =
                expected_issue_ids.iter().map(|(provider, id)| (provider.to_string(), id.to_string())).collect();
            assert_eq!(issue_ids, expected_issue_ids);
        }
        other => panic!("expected BranchNameGenerated, got {:?}", other),
    }
}

fn assert_ok(result: CommandResult) {
    assert!(matches!(result, CommandResult::Ok));
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

    let result =
        run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() }, &registry, &data, &runner)
            .await;

    assert_error_contains(result, "checkout not found");
}

#[tokio::test]
async fn create_workspace_for_checkout_success_without_ws_manager() {
    let registry = empty_registry();
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    let path = PathBuf::from("/repo/wt-feat");
    let runner = runner_ok();

    let result =
        run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() }, &registry, &data, &runner)
            .await;

    assert_ok(result);
}

#[tokio::test]
async fn archive_session_uses_provider_from_session_ref() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::failing("wrong provider")));
    registry.cloud_agents.insert("cursor", desc("cursor"), Arc::new(MockCloudAgent::succeeding()));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), make_session_for("cursor", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn create_workspace_for_checkout_success_with_ws_manager() {
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    let path = PathBuf::from("/repo/wt-feat");
    let runner = runner_ok();

    let result =
        run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() }, &registry, &data, &runner)
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
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(temp.path());
    let repo = RepoExecutionContext {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        root: repo_root(),
    };

    let result = execute(
        CommandAction::CreateWorkspaceForCheckout { checkout_path: checkout_path.clone(), label: "feat".into() },
        &repo,
        &registry,
        &data,
        &runner,
        temp.path(),
        &attachable_store,
        None,
        &local_host(),
    )
    .await;

    assert_ok(result);
    let store = AttachableStore::with_base(temp.path());
    let object_id = store
        .lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "mock-ref")
        .expect("workspace binding should exist");
    let set = store.registry().sets.values().find(|set| set.id.as_str() == object_id).expect("set should exist");
    assert_eq!(set.checkout, Some(HostPath::new(local_host(), checkout_path)));
}

#[tokio::test]
async fn create_workspace_for_checkout_ws_manager_fails() {
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws creation failed")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    let path = PathBuf::from("/repo/wt-feat");
    let runner = runner_ok();

    let result =
        run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() }, &registry, &data, &runner)
            .await;

    assert_error_eq(result, "ws creation failed");
}
#[tokio::test]
async fn prepare_terminal_for_checkout_returns_terminal_commands() {
    let registry = empty_registry();
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    let runner = runner_ok();

    let result =
        run_execute(CommandAction::PrepareTerminalForCheckout { checkout_path: path.clone(), commands: vec![] }, &registry, &data, &runner)
            .await;

    match result {
        CommandResult::TerminalPrepared { repo_identity, target_host, branch, checkout_path, attachable_set_id, commands } => {
            assert_eq!(repo_identity, flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
            assert_eq!(target_host, HostName::local());
            assert_eq!(branch, "feat");
            assert_eq!(checkout_path, path);
            assert!(attachable_set_id.is_some(), "prepare should allocate an attachable set");
            assert_eq!(commands, vec![PreparedTerminalCommand { role: "main".into(), command: "claude".into() }]);
        }
        other => panic!("expected TerminalPrepared, got {other:?}"),
    }
}

#[tokio::test]
async fn prepare_terminal_for_checkout_includes_attachable_set_id_when_present() {
    let registry = empty_registry();
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(temp.path());
    {
        let mut store = attachable_store.lock().expect("store lock");
        let ensured_set_id = store.ensure_terminal_set(Some(local_host()), Some(HostPath::new(local_host(), path.clone())));
        store.save().expect("save attachable store");
        assert_eq!(
            store.registry().sets.get(&ensured_set_id).and_then(|set| set.checkout.clone()),
            Some(HostPath::new(local_host(), path.clone()))
        );
    }

    let repo = RepoExecutionContext {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        root: repo_root(),
    };

    let result = execute(
        CommandAction::PrepareTerminalForCheckout { checkout_path: path.clone(), commands: vec![] },
        &repo,
        &registry,
        &data,
        &runner,
        temp.path(),
        &attachable_store,
        None,
        &local_host(),
    )
    .await;

    match result {
        CommandResult::TerminalPrepared { attachable_set_id, .. } => {
            let set_id = attachable_set_id.expect("attachable set id");
            let store = AttachableStore::with_base(temp.path());
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
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(temp.path());
    let repo = RepoExecutionContext {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        root: repo_root(),
    };

    let result = execute(
        CommandAction::PrepareTerminalForCheckout { checkout_path: path.clone(), commands: vec![] },
        &repo,
        &registry,
        &data,
        &runner,
        temp.path(),
        &attachable_store,
        None,
        &local_host(),
    )
    .await;

    let set_id = match result {
        CommandResult::TerminalPrepared { attachable_set_id, .. } => attachable_set_id.expect("attachable set id"),
        other => panic!("expected TerminalPrepared, got {other:?}"),
    };

    let store = AttachableStore::with_base(temp.path());
    let set = store.registry().sets.get(&set_id).expect("set should exist");
    assert_eq!(set.checkout, Some(HostPath::new(local_host(), path)));
    assert!(temp.path().join("attachables").join("registry.json").exists(), "registry should be written");
}

#[tokio::test]
async fn create_workspace_from_prepared_terminal_wraps_remote_commands_in_ssh() {
    let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(temp.path());
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("create repo root");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let repo = RepoExecutionContext {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        root: repo_root.clone(),
    };
    let result = execute(
        CommandAction::CreateWorkspaceFromPreparedTerminal {
            target_host: HostName::new("desktop"),
            branch: "feat".into(),
            checkout_path: PathBuf::from("/remote/feat"),
            attachable_set_id: None,
            commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash -l".into() }],
        },
        &repo,
        &registry,
        &empty_data(),
        &runner,
        temp.path(),
        &attachable_store,
        None,
        &local_host(),
    )
    .await;

    assert_ok(result);
    let created = workspace_manager.created_configs.lock().await;
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].working_directory, repo_root);
    let resolved = created[0].resolved_commands.as_ref().expect("resolved commands");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].0, "main");
    assert!(resolved[0].1.contains("ssh -t"));
    assert!(resolved[0].1.contains("desktop.local"));
    assert!(resolved[0].1.contains("/remote/feat"));
    assert!(resolved[0].1.contains("bash -l"));
    assert!(resolved[0].1.contains("$SHELL -l -c"), "expected login shell wrapper, got: {}", resolved[0].1);
}

#[tokio::test]
async fn create_workspace_from_prepared_terminal_prefixes_name_with_host() {
    let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(temp.path());
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("create repo root");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let repo = RepoExecutionContext {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        root: repo_root,
    };
    let result = execute(
        CommandAction::CreateWorkspaceFromPreparedTerminal {
            target_host: HostName::new("desktop"),
            branch: "feat".into(),
            checkout_path: PathBuf::from("/remote/feat"),
            attachable_set_id: None,
            commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }],
        },
        &repo,
        &registry,
        &empty_data(),
        &runner,
        temp.path(),
        &attachable_store,
        None,
        &local_host(),
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
    let attachable_store = test_attachable_store(temp.path());
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("create repo root");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let repo = RepoExecutionContext {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        root: repo_root,
    };
    let set_id = flotilla_protocol::AttachableSetId::new("set-remote");
    let result = execute(
        CommandAction::CreateWorkspaceFromPreparedTerminal {
            target_host: HostName::new("desktop"),
            branch: "feat".into(),
            checkout_path: PathBuf::from("/remote/feat"),
            attachable_set_id: Some(set_id.clone()),
            commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }],
        },
        &repo,
        &registry,
        &empty_data(),
        &runner,
        temp.path(),
        &attachable_store,
        None,
        &local_host(),
    )
    .await;

    assert_ok(result);
    let store = AttachableStore::with_base(temp.path());
    let object_id = store
        .lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "mock-ref")
        .expect("workspace binding should exist");
    assert_eq!(object_id, set_id.as_str());
    let set = store.registry().sets.get(&set_id).expect("set should exist");
    assert_eq!(set.checkout, Some(HostPath::new(HostName::new("desktop"), PathBuf::from("/remote/feat"))));
}

#[tokio::test]
async fn create_workspace_for_checkout_selects_existing_workspace() {
    let checkout_path = PathBuf::from("/repo/wt-feat");
    let existing_workspace =
        Workspace { name: "feat".to_string(), directories: vec![checkout_path.clone()], correlation_keys: vec![], attachable_set_id: None };
    let ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:42".to_string(), existing_workspace)]));

    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    let runner = runner_ok();

    let result =
        run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path, label: "feat".into() }, &registry, &data, &runner).await;

    assert_ok(result);
    let calls = ws_mgr.calls.lock().await;
    assert!(calls.contains(&"list_workspaces".to_string()), "should call list_workspaces, got: {calls:?}");
    assert!(calls.contains(&"select_workspace:workspace:42".to_string()), "should select existing workspace, got: {calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("create_workspace")), "should NOT create workspace, got: {calls:?}");
}

#[tokio::test]
async fn checkout_action_does_not_create_workspace() {
    let checkout_path = PathBuf::from("/repo/wt-feat-x");
    let ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:99".to_string(), Workspace {
        name: "feat-x".to_string(),
        directories: vec![checkout_path.clone()],
        correlation_keys: vec![],
        attachable_set_id: None,
    })]));

    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

    assert_checkout_created_branch(result, "feat-x");
    let calls = ws_mgr.calls.lock().await;
    assert!(
        !calls.iter().any(|c| c.starts_with("list_workspaces") || c.starts_with("select_workspace") || c.starts_with("create_workspace")),
        "checkout should not touch workspaces, got: {calls:?}"
    );
}

#[tokio::test]
async fn create_workspace_from_prepared_terminal_uses_local_fallback_for_remote_only_repo() {
    let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(temp.path());
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let repo = RepoExecutionContext {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        root: PathBuf::from("<remote>/desktop/home/dev/repo"),
    };
    let result = execute(
        CommandAction::CreateWorkspaceFromPreparedTerminal {
            target_host: HostName::new("desktop"),
            branch: "feat".into(),
            checkout_path: PathBuf::from("/remote/feat"),
            attachable_set_id: None,
            commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash -l".into() }],
        },
        &repo,
        &registry,
        &empty_data(),
        &runner,
        temp.path(),
        &attachable_store,
        None,
        &local_host(),
    )
    .await;

    assert_ok(result);
    let created = workspace_manager.created_configs.lock().await;
    assert_eq!(created.len(), 1);
    assert!(!created[0].working_directory.to_string_lossy().starts_with("<remote>/"));
    assert!(created[0].working_directory.exists(), "fallback working directory should exist");
    let resolved = created[0].resolved_commands.as_ref().expect("resolved commands");
    assert!(resolved[0].1.contains("$SHELL -l -c"), "expected login shell wrapper, got: {}", resolved[0].1);
}

#[tokio::test]
async fn teleport_session_creates_workspace_even_when_one_exists() {
    // Teleport must always create a new workspace because the attach command
    // is session-specific. Reusing an existing workspace would attach to
    // whatever session was there before, not the requested one.
    let checkout_path = PathBuf::from("/repo/wt-feat");
    let existing_workspace =
        Workspace { name: "feat".to_string(), directories: vec![checkout_path.clone()], correlation_keys: vec![], attachable_set_id: None };
    let ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:77".to_string(), existing_workspace)]));

    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    registry.workspace_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(
        CommandAction::TeleportSession {
            session_id: "sess-1".to_string(),
            branch: Some("feat".to_string()),
            checkout_key: Some(checkout_path),
        },
        &registry,
        &data,
        &runner,
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
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    let runner = runner_ok();
    let temp = tempfile::tempdir().expect("tempdir");
    let attachable_store = test_attachable_store(temp.path());
    let repo = RepoExecutionContext {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        root: repo_root(),
    };

    let result = execute(
        CommandAction::TeleportSession {
            session_id: "sess-1".into(),
            branch: Some("feat".into()),
            checkout_key: Some(PathBuf::from("/repo/wt-feat")),
        },
        &repo,
        &registry,
        &data,
        &runner,
        temp.path(),
        &attachable_store,
        None,
        &local_host(),
    )
    .await;

    assert_ok(result);
    let store = AttachableStore::with_base(temp.path());
    let object_id = store
        .lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "mock-ref")
        .expect("workspace binding should exist");
    let set = store.registry().sets.values().find(|set| set.id.as_str() == object_id).expect("set should exist");
    assert_eq!(set.checkout, Some(HostPath::new(local_host(), PathBuf::from("/repo/wt-feat"))));
}
// -----------------------------------------------------------------------
// Tests: SelectWorkspace
// -----------------------------------------------------------------------

#[tokio::test]
async fn select_workspace_no_manager() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result = run_execute(CommandAction::SelectWorkspace { ws_ref: "my-ws".to_string() }, &registry, &empty_data(), &runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn select_workspace_success() {
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let runner = runner_ok();

    let result = run_execute(CommandAction::SelectWorkspace { ws_ref: "my-ws".to_string() }, &registry, &empty_data(), &runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn select_workspace_failure() {
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("select failed")));
    let runner = runner_ok();

    let result = run_execute(CommandAction::SelectWorkspace { ws_ref: "bad-ws".to_string() }, &registry, &empty_data(), &runner).await;

    assert_error_eq(result, "select failed");
}

// -----------------------------------------------------------------------
// Tests: CreateCheckout
// -----------------------------------------------------------------------

#[tokio::test]
async fn create_checkout_no_manager() {
    let registry = empty_registry();
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

    assert_error_contains(result, "No checkout manager available");
}

#[tokio::test]
async fn create_checkout_success() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

    assert_checkout_created_branch(result, "feat-x");
}

#[tokio::test]
async fn create_checkout_with_issue_ids_writes_git_config() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    // Two validation probes (branch absent locally/remotely), then the git config write.
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string()), Ok(String::new())]);

    let result = run_execute(
        CommandAction::Checkout {
            repo: repo_selector(),
            target: CheckoutTarget::FreshBranch("feat-x".to_string()),
            issue_ids: vec![("github".to_string(), "42".to_string())],
        },
        &registry,
        &empty_data(),
        &runner,
    )
    .await;

    assert_checkout_created_branch(result, "feat-x");
}

#[tokio::test]
async fn create_checkout_failure() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("branch already exists")));
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

    assert_error_eq(result, "branch already exists");
}

#[tokio::test]
async fn create_checkout_success_ws_manager_fails_still_returns_created() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws failed")));
    let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

    let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

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
    data.checkouts.insert(hp("/repo/wt-old"), make_checkout("old", "/repo/wt-old"));
    let runner = runner_ok();

    let result = run_execute(remove_checkout_action("old", vec![]), &registry, &data, &runner).await;

    assert_error_contains(result, "No checkout manager available");
}

#[tokio::test]
async fn remove_checkout_success() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-old"), make_checkout("old", "/repo/wt-old"));
    let runner = runner_ok();

    let result = run_execute(remove_checkout_action("old", vec![]), &registry, &data, &runner).await;

    assert_checkout_removed_branch(result, "old");
}

#[tokio::test]
async fn remove_checkout_failure() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("cannot remove trunk")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-main"), make_checkout("main", "/repo/wt-main"));
    let runner = runner_ok();

    let result = run_execute(remove_checkout_action("main", vec![]), &registry, &data, &runner).await;

    assert_error_eq(result, "cannot remove trunk");
}

// -----------------------------------------------------------------------
// Tests: RemoveCheckout — terminal cleanup
// -----------------------------------------------------------------------

struct MockTerminalPool {
    killed: tokio::sync::Mutex<Vec<ManagedTerminalId>>,
}

#[async_trait]
impl TerminalPool for MockTerminalPool {
    async fn list_terminals(&self) -> Result<Vec<flotilla_protocol::ManagedTerminal>, String> {
        Ok(vec![])
    }
    async fn ensure_running(&self, _id: &ManagedTerminalId, _cmd: &str, _cwd: &Path) -> Result<(), String> {
        Ok(())
    }
    async fn attach_command(
        &self,
        _id: &ManagedTerminalId,
        _cmd: &str,
        _cwd: &Path,
        _env_vars: &crate::providers::terminal::TerminalEnvVars,
    ) -> Result<String, String> {
        Ok(String::new())
    }
    async fn kill_terminal(&self, id: &ManagedTerminalId) -> Result<(), String> {
        self.killed.lock().await.push(id.clone());
        Ok(())
    }
}

#[tokio::test]
async fn remove_checkout_kills_correlated_terminals() {
    let terminal_id = ManagedTerminalId { checkout: "feat-x".into(), role: "shell".into(), index: 0 };
    let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });

    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.terminal_pools.insert("shpool", desc("shpool"), Arc::clone(&mock_pool) as Arc<dyn TerminalPool>);
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));

    let runner = runner_ok();
    let result = run_execute(remove_checkout_action("feat-x", vec![terminal_id.clone()]), &registry, &data, &runner).await;

    assert_checkout_removed_branch(result, "feat-x");
    let killed = mock_pool.killed.lock().await;
    assert_eq!(killed.len(), 1);
    assert_eq!(killed[0], terminal_id);
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

    let result = run_execute(
        CommandAction::FetchCheckoutStatus {
            branch: "feat".to_string(),
            checkout_path: Some(PathBuf::from("/repo/wt")),
            change_request_id: Some("42".to_string()),
        },
        &registry,
        &empty_data(),
        &runner,
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

    let result = run_execute(
        CommandAction::FetchCheckoutStatus {
            branch: "feat".to_string(),
            checkout_path: Some(PathBuf::from("/repo/wt")),
            change_request_id: None,
        },
        &registry,
        &empty_data(),
        &runner,
    )
    .await;

    match result {
        CommandResult::CheckoutStatus(info) => {
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

    let result = run_execute(CommandAction::OpenChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn open_change_request_with_provider() {
    let mut registry = empty_registry();
    registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker));
    let runner = runner_ok();

    let result = run_execute(CommandAction::OpenChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

    assert_ok(result);
}

// -----------------------------------------------------------------------
// Tests: CloseChangeRequest
// -----------------------------------------------------------------------

#[tokio::test]
async fn close_change_request_no_provider() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result = run_execute(CommandAction::CloseChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn close_change_request_with_provider() {
    let mut registry = empty_registry();
    registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker));
    let runner = runner_ok();

    let result = run_execute(CommandAction::CloseChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

    assert_ok(result);
}

// -----------------------------------------------------------------------
// Tests: OpenIssue
// -----------------------------------------------------------------------

#[tokio::test]
async fn open_issue_no_provider() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result = run_execute(CommandAction::OpenIssue { id: "10".to_string() }, &registry, &empty_data(), &runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn open_issue_with_provider() {
    let mut registry = empty_registry();
    registry.issue_trackers.insert("github", desc("github"), Arc::new(MockIssueTracker));
    let runner = runner_ok();

    let result = run_execute(CommandAction::OpenIssue { id: "10".to_string() }, &registry, &empty_data(), &runner).await;

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

    let result = run_execute(
        CommandAction::LinkIssuesToChangeRequest {
            change_request_id: "55".to_string(),
            issue_ids: vec!["10".to_string(), "20".to_string()],
        },
        &registry,
        &empty_data(),
        &runner,
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

    let result = run_execute(
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
        &registry,
        &empty_data(),
        &runner,
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn link_issues_view_fails() {
    let registry = empty_registry();
    let runner = MockRunner::new(vec![Err("gh not found".to_string())]);

    let result = run_execute(
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
        &registry,
        &empty_data(),
        &runner,
    )
    .await;

    assert_error_eq(result, "gh not found");
}

#[tokio::test]
async fn link_issues_edit_fails() {
    let registry = empty_registry();
    let runner = MockRunner::new(vec![Ok("body text".to_string()), Err("permission denied".to_string())]);

    let result = run_execute(
        CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
        &registry,
        &empty_data(),
        &runner,
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

    let result =
        run_execute(CommandAction::ArchiveSession { session_id: "nonexistent".to_string() }, &registry, &empty_data(), &runner).await;

    assert_error_contains(result, "session not found");
}

#[tokio::test]
async fn archive_session_no_agent_provider() {
    let registry = empty_registry();
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

    assert_error_contains(result, "No coding agent provider: claude");
}

#[tokio::test]
async fn archive_session_success() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

    assert_ok(result);
}

#[tokio::test]
async fn archive_session_agent_fails() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::failing("archive failed")));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

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
    data.issues.insert("42".to_string(), make_issue("42", "Add login feature"));
    let runner = runner_ok();

    let result = run_execute(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, &registry, &data, &runner).await;

    assert_branch_name_generated(result, "feat/add-login", &[("github", "42")]);
}

#[tokio::test]
async fn generate_branch_name_ai_failure_uses_fallback() {
    let mut registry = empty_registry();
    registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::failing("API error")));
    let mut data = empty_data();
    data.issues.insert("42".to_string(), make_issue("42", "Add login"));
    let runner = runner_ok();

    let result = run_execute(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, &registry, &data, &runner).await;

    assert_branch_name_generated(result, "issue-42", &[("issues", "42")]);
}

#[tokio::test]
async fn generate_branch_name_no_ai_provider_uses_fallback() {
    let registry = empty_registry();
    let mut data = empty_data();
    data.issues.insert("7".to_string(), make_issue("7", "Fix bug"));
    let runner = runner_ok();

    let result = run_execute(CommandAction::GenerateBranchName { issue_keys: vec!["7".to_string()] }, &registry, &data, &runner).await;

    // No issue tracker registered, defaults to "issues"
    assert_branch_name_generated(result, "issue-7", &[("issues", "7")]);
}

#[tokio::test]
async fn generate_branch_name_multiple_issues() {
    let mut registry = empty_registry();
    registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::succeeding("feat/login-and-signup")));
    registry.issue_trackers.insert("github", desc("github"), Arc::new(MockIssueTracker));
    let mut data = empty_data();
    data.issues.insert("1".to_string(), make_issue("1", "Login feature"));
    data.issues.insert("2".to_string(), make_issue("2", "Signup feature"));
    let runner = runner_ok();

    let result =
        run_execute(CommandAction::GenerateBranchName { issue_keys: vec!["1".to_string(), "2".to_string()] }, &registry, &data, &runner)
            .await;

    assert_branch_name_generated(result, "feat/login-and-signup", &[("github", "1"), ("github", "2")]);
}

#[tokio::test]
async fn generate_branch_name_unknown_issue_key() {
    let registry = empty_registry();
    let data = empty_data();
    let runner = runner_ok();

    let result =
        run_execute(CommandAction::GenerateBranchName { issue_keys: vec!["nonexistent".to_string()] }, &registry, &data, &runner).await;

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
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
        &registry,
        &data,
        &runner,
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
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    data.sessions.insert("sess-1".to_string(), make_session_for("cursor", "sess-1"));
    let runner = runner_ok();

    let attach = resolve_attach_command("sess-1", &registry, &data).await.expect("resolve attach command");
    assert_eq!(attach, "agent --resume sess-1");

    let result = run_execute(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
        &registry,
        &data,
        &runner,
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
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: None },
        &registry,
        &data,
        &runner,
    )
    .await;

    assert_ok(result);
}

#[tokio::test]
async fn teleport_session_no_path_no_branch() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: None, checkout_key: None },
        &registry,
        &data,
        &runner,
    )
    .await;

    assert_error_contains(result, "Could not determine checkout path");
}

#[tokio::test]
async fn teleport_session_ws_manager_fails() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws failed")));
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
        &registry,
        &data,
        &runner,
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
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let result = run_execute(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: None, checkout_key: Some(path) },
        &registry,
        &data,
        &runner,
    )
    .await;

    assert_ok(result);
}

// -----------------------------------------------------------------------
// Tests: Daemon-level commands rejected
// -----------------------------------------------------------------------

#[tokio::test]
async fn daemon_level_commands_return_error() {
    let registry = empty_registry();
    let data = empty_data();
    let runner = runner_ok();

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
        let result = run_execute(cmd, &registry, &data, &runner).await;
        assert_error_contains(result, "daemon-level command");
    }
}

// -----------------------------------------------------------------------
// Tests: workspace_config helper
// -----------------------------------------------------------------------

#[test]
fn workspace_config_builds_correct_struct() {
    let config = workspace_config(Path::new("/nonexistent-repo"), "my-branch", Path::new("/repo/wt"), "claude", &config_base());

    assert_eq!(config.name, "my-branch");
    assert_eq!(config.working_directory, PathBuf::from("/repo/wt"));
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
    runner: MockRunner,
) -> ExecutionPlan {
    let config_base = config_base();
    build_plan(
        local_command(action),
        RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root(),
        },
        Arc::new(registry),
        Arc::new(providers_data),
        Arc::new(runner),
        config_base.clone(),
        test_attachable_store(&config_base),
        None,
        local_host(),
        None,
    )
    .await
}

async fn run_build_plan_to_completion(
    action: CommandAction,
    registry: ProviderRegistry,
    providers_data: ProviderData,
    runner: MockRunner,
) -> CommandResult {
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::step::run_step_plan;

    let config_base = config_base();
    let attachable_store = test_attachable_store(&config_base);
    let local_host = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };
    let registry = Arc::new(registry);

    let plan = build_plan(
        local_command(action),
        repo.clone(),
        Arc::clone(&registry),
        Arc::new(providers_data),
        Arc::new(runner),
        config_base.clone(),
        attachable_store.clone(),
        None,
        local_host.clone(),
        None,
    )
    .await;

    match plan {
        ExecutionPlan::Immediate(result) => result,
        ExecutionPlan::Steps(step_plan) => {
            let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
            let resolver = ExecutorStepResolver {
                repo,
                registry,
                config_base,
                attachable_store,
                daemon_socket_path: None,
                local_host: local_host.clone(),
            };
            run_step_plan(step_plan, 1, local_host, repo_identity(), repo_root(), cancel, tx, Some(&resolver)).await
        }
    }
}

// -----------------------------------------------------------------------
// Tests: paired characterization for execute and build_plan paths
// -----------------------------------------------------------------------

#[tokio::test]
async fn checkout_create_plan_and_execute_return_same_checkout_created_result() {
    let expected_path = PathBuf::from("/repo/wt-feat-x");
    let execute_runner = MockRunner::new(vec![Err("not found".into()), Err("not found".into())]);
    let plan_runner = MockRunner::new(vec![Err("not found".into()), Err("not found".into())]);

    let mut execute_registry = empty_registry();
    execute_registry.checkout_managers.insert(
        "wt",
        desc("wt"),
        Arc::new(MockCheckoutManager::succeeding("feat-x", expected_path.to_str().expect("utf8 path"))),
    );

    let execute_result = run_execute(fresh_checkout_action("feat-x"), &execute_registry, &empty_data(), &execute_runner).await;

    match execute_result {
        CommandResult::CheckoutCreated { branch, path } => {
            assert_eq!(branch, "feat-x");
            assert_eq!(path, expected_path);
        }
        other => panic!("expected CheckoutCreated from execute, got {other:?}"),
    }

    let mut plan_registry = empty_registry();
    plan_registry.checkout_managers.insert(
        "wt",
        desc("wt"),
        Arc::new(MockCheckoutManager::succeeding("feat-x", expected_path.to_str().expect("utf8 path"))),
    );

    let plan_result = run_build_plan_to_completion(fresh_checkout_action("feat-x"), plan_registry, empty_data(), plan_runner).await;

    match plan_result {
        CommandResult::CheckoutCreated { branch, path } => {
            assert_eq!(branch, "feat-x");
            assert_eq!(path, expected_path);
        }
        other => panic!("expected CheckoutCreated from build_plan completion, got {other:?}"),
    }
}

#[tokio::test]
async fn remove_checkout_plan_and_execute_both_kill_correlated_terminals() {
    let terminal_id = ManagedTerminalId { checkout: "feat-x".into(), role: "shell".into(), index: 0 };

    let execute_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let mut execute_registry = empty_registry();
    execute_registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    execute_registry.terminal_pools.insert("shpool", desc("shpool"), Arc::clone(&execute_pool) as Arc<dyn TerminalPool>);
    let mut execute_data = empty_data();
    execute_data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));

    let execute_result =
        run_execute(remove_checkout_action("feat-x", vec![terminal_id.clone()]), &execute_registry, &execute_data, &runner_ok()).await;

    assert_checkout_removed_branch(execute_result, "feat-x");
    let execute_killed = execute_pool.killed.lock().await;
    assert_eq!(execute_killed.as_slice(), std::slice::from_ref(&terminal_id));
    drop(execute_killed);

    let plan_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let mut plan_registry = empty_registry();
    plan_registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    plan_registry.terminal_pools.insert("shpool", desc("shpool"), Arc::clone(&plan_pool) as Arc<dyn TerminalPool>);
    let mut plan_data = empty_data();
    plan_data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));

    let plan_result =
        run_build_plan_to_completion(remove_checkout_action("feat-x", vec![terminal_id.clone()]), plan_registry, plan_data, runner_ok())
            .await;

    assert_ok(plan_result);
    let plan_killed = plan_pool.killed.lock().await;
    assert_eq!(plan_killed.as_slice(), &[terminal_id]);
}

#[tokio::test]
async fn teleport_plan_and_execute_both_create_new_workspace_even_when_one_exists() {
    let checkout_path = PathBuf::from("/repo/wt-feat");
    let existing_workspace =
        Workspace { name: "feat".to_string(), directories: vec![checkout_path.clone()], correlation_keys: vec![], attachable_set_id: None };

    let execute_ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:77".to_string(), existing_workspace.clone())]));
    let mut execute_registry = empty_registry();
    execute_registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    execute_registry.workspace_managers.insert("cmux", desc("cmux"), execute_ws_mgr.clone());
    let mut execute_data = empty_data();
    execute_data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    execute_data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));

    let execute_result = run_execute(
        CommandAction::TeleportSession {
            session_id: "sess-1".to_string(),
            branch: Some("feat".to_string()),
            checkout_key: Some(checkout_path.clone()),
        },
        &execute_registry,
        &execute_data,
        &runner_ok(),
    )
    .await;

    assert_ok(execute_result);
    let execute_calls = execute_ws_mgr.calls.lock().await;
    assert!(
        execute_calls.iter().any(|call| call.starts_with("create_workspace")),
        "execute teleport should create a new workspace, got: {execute_calls:?}"
    );
    assert!(
        !execute_calls.iter().any(|call| call.starts_with("select_workspace")),
        "execute teleport should not reuse an existing workspace, got: {execute_calls:?}"
    );
    drop(execute_calls);

    let plan_ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:77".to_string(), existing_workspace)]));
    let mut plan_registry = empty_registry();
    plan_registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    plan_registry.workspace_managers.insert("cmux", desc("cmux"), plan_ws_mgr.clone());
    let mut plan_data = empty_data();
    plan_data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    plan_data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));

    let plan_result = run_build_plan_to_completion(
        CommandAction::TeleportSession {
            session_id: "sess-1".to_string(),
            branch: Some("feat".to_string()),
            checkout_key: Some(checkout_path),
        },
        plan_registry,
        plan_data,
        runner_ok(),
    )
    .await;

    assert_ok(plan_result);
    let plan_calls = plan_ws_mgr.calls.lock().await;
    assert!(
        plan_calls.iter().any(|call| call.starts_with("create_workspace")),
        "planned teleport should create a new workspace, got: {plan_calls:?}"
    );
    assert!(
        !plan_calls.iter().any(|call| call.starts_with("select_workspace")),
        "planned teleport should not reuse an existing workspace, got: {plan_calls:?}"
    );
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
        ExecutionPlan::Steps(step_plan) => {
            assert_eq!(step_plan.steps.len(), 2, "checkout + workspace steps");
            assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
            assert_eq!(step_plan.steps[1].description, "Create workspace");
        }
        ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
    }
}

#[tokio::test]
async fn build_plan_create_checkout_skips_existing() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    // Pre-populate with an existing checkout for the branch
    data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));
    let runner = runner_ok();

    let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, data, runner).await;

    match plan {
        ExecutionPlan::Steps(step_plan) => {
            assert_eq!(step_plan.steps.len(), 2, "checkout + workspace steps");
            assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
            assert_eq!(step_plan.steps[1].description, "Create workspace");
        }
        ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
    }
}

#[tokio::test]
async fn checkout_plan_includes_workspace_step() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));

    let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, empty_data(), runner_ok()).await;

    match plan {
        ExecutionPlan::Steps(step_plan) => {
            assert_eq!(step_plan.steps.len(), 2, "expected checkout + workspace steps");
            assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
            assert_eq!(step_plan.steps[1].description, "Create workspace");
        }
        ExecutionPlan::Immediate(_) => panic!("expected Steps"),
    }
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
    let runner = Arc::new(MockRunner::new(vec![Err("missing".into()), Err("missing".into())]));
    let cb = config_base();
    let attachable = test_attachable_store(&cb);
    let lh = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };

    let plan = build_plan(
        local_command(fresh_checkout_action("feat-x")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::clone(&registry),
        Arc::new(empty_data()),
        runner,
        cb.clone(),
        attachable.clone(),
        None,
        lh.clone(),
        None,
    )
    .await;

    let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
    let resolver = ExecutorStepResolver {
        repo,
        registry,
        config_base: cb,
        attachable_store: attachable,
        daemon_socket_path: None,
        local_host: lh.clone(),
    };

    let result = match plan {
        ExecutionPlan::Steps(step_plan) => run_step_plan(step_plan, 1, lh, repo_identity(), repo_root(), cancel, tx, Some(&resolver)).await,
        _ => panic!("expected steps"),
    };

    assert!(matches!(result, CommandResult::CheckoutCreated { .. }));

    let calls = ws_mgr.calls.lock().await;
    assert!(calls.iter().any(|c| c.starts_with("create_workspace")), "should create workspace from prior outcome: {calls:?}");
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
    let runner = Arc::new(MockRunner::new(vec![Ok("".into()), Err("missing".into())]));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));
    let cb = config_base();
    let attachable = test_attachable_store(&cb);
    let lh = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };

    let plan = build_plan(
        local_command(existing_branch_checkout_action("feat-x")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::clone(&registry),
        Arc::new(data),
        runner,
        cb.clone(),
        attachable.clone(),
        None,
        lh.clone(),
        None,
    )
    .await;

    let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
    let resolver = ExecutorStepResolver {
        repo,
        registry,
        config_base: cb,
        attachable_store: attachable,
        daemon_socket_path: None,
        local_host: lh.clone(),
    };

    let result = match plan {
        ExecutionPlan::Steps(step_plan) => run_step_plan(step_plan, 1, lh, repo_identity(), repo_root(), cancel, tx, Some(&resolver)).await,
        _ => panic!("expected steps"),
    };

    assert!(
        matches!(result, CommandResult::CheckoutCreated { ref branch, .. } if branch == "feat-x"),
        "should return CheckoutCreated for pre-existing checkout, got: {result:?}"
    );
    let calls = ws_mgr.calls.lock().await;
    assert!(calls.iter().any(|c| c.starts_with("create_workspace")), "should create workspace for pre-existing checkout: {calls:?}");
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
    let runner = Arc::new(MockRunner::new(vec![Err("missing".into()), Err("missing".into())]));
    let cb = config_base();
    let attachable = test_attachable_store(&cb);
    let lh = local_host();
    let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };

    let plan = build_plan(
        local_command(fresh_checkout_action("feat-x")),
        RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        Arc::clone(&registry),
        Arc::new(empty_data()),
        runner,
        cb.clone(),
        attachable.clone(),
        None,
        lh.clone(),
        None,
    )
    .await;

    let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
    let resolver = ExecutorStepResolver {
        repo,
        registry,
        config_base: cb,
        attachable_store: attachable,
        daemon_socket_path: None,
        local_host: lh.clone(),
    };

    let result = match plan {
        ExecutionPlan::Steps(step_plan) => run_step_plan(step_plan, 1, lh, repo_identity(), repo_root(), cancel, tx, Some(&resolver)).await,
        _ => panic!("expected steps"),
    };

    assert_eq!(result, CommandResult::CheckoutCreated { branch: "feat-x".into(), path: PathBuf::from("/repo/wt-feat-x") });
}

#[tokio::test]
async fn build_plan_teleport_session_returns_steps() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
    let mut data = empty_data();
    let path = PathBuf::from("/repo/wt-feat");
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let plan = run_build_plan(
        CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
        registry,
        data,
        runner,
    )
    .await;

    match plan {
        ExecutionPlan::Steps(step_plan) => {
            // 3 steps: resolve attach, ensure checkout, create workspace
            assert_eq!(step_plan.steps.len(), 3, "expected 3 steps, got {}", step_plan.steps.len());
        }
        ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
    }
}

#[tokio::test]
async fn build_plan_remove_checkout_returns_steps() {
    let mut registry = empty_registry();
    registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old")));
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-old"), make_checkout("old", "/repo/wt-old"));
    let runner = runner_ok();

    let plan = run_build_plan(remove_checkout_action("old", vec![]), registry, data, runner).await;

    match plan {
        ExecutionPlan::Steps(step_plan) => {
            // At least 1 step: remove checkout
            assert!(!step_plan.steps.is_empty(), "expected at least 1 step");
        }
        ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
    }
}

#[tokio::test]
async fn build_plan_archive_session_returns_steps() {
    let mut registry = empty_registry();
    registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
    let mut data = empty_data();
    data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
    let runner = runner_ok();

    let plan = run_build_plan(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, registry, data, runner).await;

    match plan {
        ExecutionPlan::Steps(step_plan) => {
            assert_eq!(step_plan.steps.len(), 1, "expected a single archive step");
            assert_eq!(step_plan.steps[0].description, "Archive session sess-1");
        }
        ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
    }
}

#[tokio::test]
async fn build_plan_generate_branch_name_returns_steps() {
    let mut registry = empty_registry();
    registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::succeeding("feat/add-login")));
    let mut data = empty_data();
    data.issues.insert("42".to_string(), make_issue("42", "Add login feature"));
    let runner = runner_ok();

    let plan = run_build_plan(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, registry, data, runner).await;

    match plan {
        ExecutionPlan::Steps(step_plan) => {
            assert_eq!(step_plan.steps.len(), 1, "expected a single branch-name step");
            assert_eq!(step_plan.steps[0].description, "Generate branch name");
        }
        ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
    }
}

#[tokio::test]
async fn build_plan_archive_session_missing_session_returns_immediate_error() {
    let registry = empty_registry();
    let runner = runner_ok();

    let plan = run_build_plan(CommandAction::ArchiveSession { session_id: "missing".to_string() }, registry, empty_data(), runner).await;

    match plan {
        ExecutionPlan::Immediate(CommandResult::Error { message }) => {
            assert!(message.contains("session not found"), "unexpected message: {message}");
        }
        ExecutionPlan::Immediate(other) => panic!("expected Error result, got {other:?}"),
        ExecutionPlan::Steps(_) => panic!("expected Immediate, got Steps"),
    }
}

#[tokio::test]
async fn build_plan_generate_branch_name_without_ai_returns_immediate_fallback() {
    let mut data = empty_data();
    data.issues.insert("42".to_string(), make_issue("42", "Add login feature"));
    let runner = runner_ok();

    let plan =
        run_build_plan(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, empty_registry(), data, runner).await;

    match plan {
        ExecutionPlan::Immediate(CommandResult::BranchNameGenerated { name, issue_ids }) => {
            assert_eq!(name, "issue-42");
            assert_eq!(issue_ids, vec![("issues".to_string(), "42".to_string())]);
        }
        ExecutionPlan::Immediate(other) => panic!("expected BranchNameGenerated, got {other:?}"),
        ExecutionPlan::Steps(_) => panic!("expected Immediate, got Steps"),
    }
}

#[tokio::test]
async fn build_plan_simple_command_returns_immediate() {
    let mut registry = empty_registry();
    registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker));
    let runner = runner_ok();

    let plan = run_build_plan(CommandAction::OpenChangeRequest { id: "42".to_string() }, registry, empty_data(), runner).await;

    match plan {
        ExecutionPlan::Immediate(result) => {
            assert_ok(result);
        }
        ExecutionPlan::Steps(_) => panic!("expected Immediate, got Steps"),
    }
}

// -----------------------------------------------------------------------
// Tests: resolve_checkout_branch
// -----------------------------------------------------------------------

#[test]
fn resolve_checkout_branch_path_found() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat-branch", "/repo/wt-feat"));
    let local_host = HostName::local();

    let result = resolve_checkout_branch(&CheckoutSelector::Path(PathBuf::from("/repo/wt-feat")), &data, &local_host);

    assert_eq!(result.expect("path lookup should succeed"), "feat-branch");
}

#[test]
fn resolve_checkout_branch_path_not_found() {
    let data = empty_data();
    let local_host = HostName::local();

    let result = resolve_checkout_branch(&CheckoutSelector::Path(PathBuf::from("/nonexistent")), &data, &local_host);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("checkout not found"));
}

#[test]
fn resolve_checkout_branch_query_exact_match() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat-login", "/repo/wt-feat"));
    let local_host = HostName::local();

    let result = resolve_checkout_branch(&CheckoutSelector::Query("feat-login".to_string()), &data, &local_host);

    assert_eq!(result.expect("exact query should match"), "feat-login");
}

#[test]
fn resolve_checkout_branch_query_substring_match() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat-login-page", "/repo/wt-feat"));
    let local_host = HostName::local();

    let result = resolve_checkout_branch(&CheckoutSelector::Query("login".to_string()), &data, &local_host);

    assert_eq!(result.expect("substring query should match"), "feat-login-page");
}

#[test]
fn resolve_checkout_branch_query_not_found() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat-login", "/repo/wt-feat"));
    let local_host = HostName::local();

    let result = resolve_checkout_branch(&CheckoutSelector::Query("nonexistent".to_string()), &data, &local_host);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("checkout not found"));
}

#[test]
fn resolve_checkout_branch_query_ambiguous() {
    let mut data = empty_data();
    data.checkouts.insert(hp("/repo/wt-feat-a"), make_checkout("feat-a", "/repo/wt-feat-a"));
    data.checkouts.insert(hp("/repo/wt-feat-b"), make_checkout("feat-b", "/repo/wt-feat-b"));
    let local_host = HostName::local();

    let result = resolve_checkout_branch(&CheckoutSelector::Query("feat".to_string()), &data, &local_host);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("ambiguous"));
}

// -----------------------------------------------------------------------
// Tests: resolve_terminal_pool
// -----------------------------------------------------------------------

#[tokio::test]
async fn resolve_terminal_pool_no_template_uses_default() {
    let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let mut config = WorkspaceConfig {
        name: "test-branch".to_string(),
        working_directory: PathBuf::from("/repo/wt"),
        template_vars: [("main_command".to_string(), "claude".to_string())].into_iter().collect(),
        template_yaml: None,
        resolved_commands: None,
    };

    resolve_terminal_pool(&mut config, mock_pool.as_ref(), &crate::attachable::shared_in_memory_attachable_store(), "shpool", None).await;

    // Default template has one "main" terminal entry
    assert!(config.resolved_commands.is_some());
    let commands = config.resolved_commands.expect("default template should produce resolved commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "main");
}

#[tokio::test]
async fn resolve_terminal_pool_skips_non_terminal_content() {
    let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
    let yaml = r#"
content:
  - role: docs
    type: webview
    command: "http://localhost:3000"
"#;
    let mut config = WorkspaceConfig {
        name: "test-branch".to_string(),
        working_directory: PathBuf::from("/repo/wt"),
        template_vars: std::collections::HashMap::new(),
        template_yaml: Some(yaml.to_string()),
        resolved_commands: None,
    };

    resolve_terminal_pool(&mut config, mock_pool.as_ref(), &crate::attachable::shared_in_memory_attachable_store(), "shpool", None).await;

    // All content entries were non-terminal, so resolved_commands stays None
    assert!(config.resolved_commands.is_none());
}

// -----------------------------------------------------------------------
// Tests: build_terminal_env_vars
// -----------------------------------------------------------------------

#[test]
fn build_terminal_env_vars_creates_binding_and_populates_both_vars() {
    let store = crate::attachable::shared_in_memory_attachable_store();
    let id = ManagedTerminalId { checkout: "feat".into(), role: "agent".into(), index: 0 };
    let cwd = std::path::Path::new("/repo/feat");
    let socket = std::path::PathBuf::from("/tmp/flotilla.sock");

    let vars = build_terminal_env_vars(&id, cwd, "claude", &store, "shpool", Some(&socket));

    assert_eq!(vars.len(), 2);
    assert_eq!(vars[0].0, "FLOTILLA_ATTACHABLE_ID");
    assert!(!vars[0].1.is_empty());
    assert_eq!(vars[1].0, "FLOTILLA_DAEMON_SOCKET");
    assert_eq!(vars[1].1, "/tmp/flotilla.sock");

    // Calling again returns the same attachable ID (idempotent)
    let vars2 = build_terminal_env_vars(&id, cwd, "claude", &store, "shpool", Some(&socket));
    assert_eq!(vars[0].1, vars2[0].1);
}

#[test]
fn build_terminal_env_vars_without_socket_only_has_attachable_id() {
    let store = crate::attachable::shared_in_memory_attachable_store();
    let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };
    let vars = build_terminal_env_vars(&id, std::path::Path::new("/repo"), "$SHELL", &store, "shpool", None);
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].0, "FLOTILLA_ATTACHABLE_ID");
}

// -----------------------------------------------------------------------
// Tests: write_branch_issue_links
// -----------------------------------------------------------------------

#[tokio::test]
async fn write_branch_issue_links_single_provider_multiple_issues() {
    let runner = MockRunner::new(vec![Ok(String::new())]);
    let issue_ids = vec![("github".to_string(), "10".to_string()), ("github".to_string(), "20".to_string())];

    write_branch_issue_links(&repo_root(), "feat-x", &issue_ids, &runner).await;

    assert_eq!(runner.remaining(), 0, "single provider should consume exactly 1 response");
}

#[tokio::test]
async fn write_branch_issue_links_multiple_providers() {
    let runner = MockRunner::new(vec![Ok(String::new()), Ok(String::new())]);
    let issue_ids = vec![("github".to_string(), "10".to_string()), ("jira".to_string(), "PROJ-5".to_string())];

    write_branch_issue_links(&repo_root(), "feat-x", &issue_ids, &runner).await;

    assert_eq!(runner.remaining(), 0, "two providers should consume exactly 2 responses");
}

#[tokio::test]
async fn write_branch_issue_links_git_error_tolerated() {
    let runner = MockRunner::new(vec![Err("git config failed".to_string())]);
    let issue_ids = vec![("github".to_string(), "10".to_string())];

    write_branch_issue_links(&repo_root(), "feat-x", &issue_ids, &runner).await;

    assert_eq!(runner.remaining(), 0, "should still consume the response even on error");
}

#[tokio::test]
async fn write_branch_issue_links_empty_is_noop() {
    let runner = MockRunner::new(vec![]);

    write_branch_issue_links(&repo_root(), "feat-x", &[], &runner).await;

    assert_eq!(runner.remaining(), 0, "empty issue_ids should make zero calls");
}

// -----------------------------------------------------------------------
// Tests: validate_checkout_target
// -----------------------------------------------------------------------

#[tokio::test]
async fn validate_fresh_branch_succeeds_when_neither_exists() {
    // local check -> Err (not found), remote check -> Err (not found)
    let runner = MockRunner::new(vec![Err("not found".to_string()), Err("not found".to_string())]);

    let result = validate_checkout_target(&repo_root(), "new-branch", CheckoutIntent::FreshBranch, &runner).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn validate_fresh_branch_fails_when_local_exists() {
    // local check -> Ok (found), remote check -> Err (not found)
    let runner = MockRunner::new(vec![Ok(String::new()), Err("not found".to_string())]);

    let result = validate_checkout_target(&repo_root(), "existing", CheckoutIntent::FreshBranch, &runner).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("already exists"));
}

#[tokio::test]
async fn validate_fresh_branch_fails_when_remote_exists() {
    // local check -> Err (not found), remote check -> Ok (found)
    let runner = MockRunner::new(vec![Err("not found".to_string()), Ok(String::new())]);

    let result = validate_checkout_target(&repo_root(), "remote-only", CheckoutIntent::FreshBranch, &runner).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("already exists"));
}

#[tokio::test]
async fn validate_existing_branch_succeeds_when_local_exists() {
    // local check -> Ok (found), remote check -> Err (not found)
    let runner = MockRunner::new(vec![Ok(String::new()), Err("not found".to_string())]);

    let result = validate_checkout_target(&repo_root(), "local-branch", CheckoutIntent::ExistingBranch, &runner).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn validate_existing_branch_succeeds_when_remote_exists() {
    // local check -> Err (not found), remote check -> Ok (found)
    let runner = MockRunner::new(vec![Err("not found".to_string()), Ok(String::new())]);

    let result = validate_checkout_target(&repo_root(), "remote-branch", CheckoutIntent::ExistingBranch, &runner).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn validate_existing_branch_fails_when_neither_exists() {
    // local check -> Err (not found), remote check -> Err (not found)
    let runner = MockRunner::new(vec![Err("not found".to_string()), Err("not found".to_string())]);

    let result = validate_checkout_target(&repo_root(), "ghost-branch", CheckoutIntent::ExistingBranch, &runner).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("branch not found"));
}

#[test]
fn wrap_remote_attach_commands_uses_login_shell() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let commands = vec![PreparedTerminalCommand { role: "main".into(), command: "claude".into() }];
    let result =
        wrap_remote_attach_commands(&HostName::new("desktop"), &PathBuf::from("/home/dev/project"), &commands, temp.path()).unwrap();

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].role, "main");
    assert!(result[0].command.contains("$SHELL -l -c"), "expected login shell wrapper, got: {}", result[0].command);
    assert!(result[0].command.contains("ssh -t"), "expected ssh -t, got: {}", result[0].command);
    assert!(result[0].command.contains("desktop.local"), "expected host, got: {}", result[0].command);
    assert!(result[0].command.contains("/home/dev/project"), "expected remote dir, got: {}", result[0].command);
    assert!(result[0].command.contains("claude"), "expected command, got: {}", result[0].command);
}

#[test]
fn escape_for_double_quotes_handles_special_chars() {
    assert_eq!(escape_for_double_quotes("hello"), "hello");
    assert_eq!(escape_for_double_quotes(r#"say "hi""#), r#"say \"hi\""#);
    assert_eq!(escape_for_double_quotes("$HOME"), r"\$HOME");
    assert_eq!(escape_for_double_quotes("a`cmd`b"), r"a\`cmd\`b");
    assert_eq!(escape_for_double_quotes(r"back\slash"), r"back\\slash");
    assert_eq!(escape_for_double_quotes(""), "");
    assert_eq!(
        escape_for_double_quotes("shpool --socket /tmp/s.sock attach flotilla/feat/main/0"),
        "shpool --socket /tmp/s.sock attach flotilla/feat/main/0"
    );
}

#[test]
fn wrap_remote_attach_commands_includes_multiplex_args() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let commands = vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }];
    let result =
        wrap_remote_attach_commands(&HostName::new("desktop"), &PathBuf::from("/home/dev/project"), &commands, temp.path()).unwrap();

    // Default is multiplex=true
    assert!(result[0].command.contains("ControlMaster=auto"), "expected ControlMaster, got: {}", result[0].command);
    assert!(result[0].command.contains("ControlPersist=60"), "expected ControlPersist, got: {}", result[0].command);
}

#[test]
fn wrap_remote_attach_commands_omits_multiplex_when_disabled() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[ssh]\nmultiplex = false\n\n[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
    )
    .expect("write hosts config");

    let commands = vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }];
    let result =
        wrap_remote_attach_commands(&HostName::new("desktop"), &PathBuf::from("/home/dev/project"), &commands, temp.path()).unwrap();

    assert!(!result[0].command.contains("ControlMaster"), "should not have ControlMaster when disabled, got: {}", result[0].command);
}

#[test]
fn wrap_remote_attach_commands_per_host_multiplex_override() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("hosts.toml"),
        "[ssh]\nmultiplex = true\n\n[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\nssh_multiplex = false\n",
    )
    .expect("write hosts config");

    let commands = vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }];
    let result =
        wrap_remote_attach_commands(&HostName::new("desktop"), &PathBuf::from("/home/dev/project"), &commands, temp.path()).unwrap();

    assert!(!result[0].command.contains("ControlMaster"), "per-host override should disable multiplex, got: {}", result[0].command);
}

// -----------------------------------------------------------------------
// Tests: ExecutorStepResolver
// -----------------------------------------------------------------------

#[tokio::test]
async fn executor_step_resolver_creates_workspace() {
    let ws_mgr = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);

    let config_base = config_base();
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(registry),
        config_base: config_base.clone(),
        attachable_store: test_attachable_store(&config_base),
        daemon_socket_path: None,
        local_host: local_host(),
    };

    let prior =
        vec![StepOutcome::CompletedWith(CommandResult::CheckoutCreated { branch: "feat".into(), path: PathBuf::from("/repo/wt-feat") })];
    let action = StepAction::CreateWorkspaceForCheckout { label: "feat".into() };
    let outcome = resolver.resolve("create workspace", action, &prior).await;
    assert!(outcome.is_ok(), "resolve should succeed: {outcome:?}");

    let calls = ws_mgr.calls.lock().await;
    assert!(calls.iter().any(|c| c.starts_with("create_workspace")), "should call create_workspace, got: {calls:?}");
}

#[tokio::test]
async fn executor_step_resolver_skips_when_no_checkout_path() {
    let ws_mgr = Arc::new(MockWorkspaceManager::succeeding());
    let mut registry = empty_registry();
    registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);

    let config_base = config_base();
    let resolver = ExecutorStepResolver {
        repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
        registry: Arc::new(registry),
        config_base: config_base.clone(),
        attachable_store: test_attachable_store(&config_base),
        daemon_socket_path: None,
        local_host: local_host(),
    };

    let action = StepAction::CreateWorkspaceForCheckout { label: "feat".into() };
    let outcome = resolver.resolve("create workspace", action, &[]).await;
    assert!(matches!(outcome, Ok(StepOutcome::Skipped)), "should skip when no prior CheckoutCreated outcome: {outcome:?}");

    let calls = ws_mgr.calls.lock().await;
    assert!(calls.is_empty(), "should not call workspace manager when no checkout path in prior outcomes, got: {calls:?}");
}
