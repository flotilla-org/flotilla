use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::{
    sync::{watch, Notify},
    task::JoinHandle,
};

use crate::{
    attachable::{terminal_session_binding_ref, BindingObjectKind, SharedAttachableStore},
    data::{self, CorrelationResult, RefreshError},
    provider_data::ProviderData,
    providers::{correlation::CorrelatedGroup, registry::ProviderRegistry, types::RepoCriteria},
};

/// Result of a single background refresh cycle.
#[derive(Debug, Clone)]
pub struct RefreshSnapshot {
    pub providers: Arc<ProviderData>,
    pub work_items: Vec<CorrelationResult>,
    pub correlation_groups: Vec<CorrelatedGroup>,
    pub errors: Vec<RefreshError>,
    pub provider_health: HashMap<(&'static str, String), bool>,
}

impl Default for RefreshSnapshot {
    fn default() -> Self {
        Self {
            providers: Arc::new(ProviderData::default()),
            work_items: Vec::new(),
            correlation_groups: Vec::new(),
            errors: Vec::new(),
            provider_health: HashMap::new(),
        }
    }
}

pub struct RepoRefreshHandle {
    pub refresh_trigger: Arc<Notify>,
    pub snapshot_rx: watch::Receiver<Arc<RefreshSnapshot>>,
    _task_handle: JoinHandle<()>,
}

impl RepoRefreshHandle {
    pub fn spawn(
        repo_root: PathBuf,
        registry: Arc<ProviderRegistry>,
        criteria: RepoCriteria,
        attachable_store: SharedAttachableStore,
        agent_state_store: crate::agents::SharedAgentStateStore,
        interval: Duration,
    ) -> Self {
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(RefreshSnapshot::default()));
        let refresh_trigger = Arc::new(Notify::new());
        let trigger = refresh_trigger.clone();

        let task_handle = tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = timer.tick() => {}
                    _ = trigger.notified() => {}
                }

                // Fetch all provider data
                let mut provider_data = ProviderData::default();
                let errors =
                    refresh_providers(&mut provider_data, &repo_root, &registry, &criteria, &attachable_store, &agent_state_store).await;
                let provider_health = compute_provider_health(&registry, &errors);

                // Correlate
                let providers = Arc::new(provider_data);
                let (work_items, correlation_groups) = data::correlate(&providers);

                let snapshot = Arc::new(RefreshSnapshot { providers, work_items, correlation_groups, errors, provider_health });

                // Publish — receivers will see has_changed().
                // Break if receiver is dropped (handle dropped without Drop running).
                if snapshot_tx.send(snapshot).is_err() {
                    break;
                }
            }
        });

        Self { refresh_trigger, snapshot_rx, _task_handle: task_handle }
    }

    /// Create a dormant refresh handle that never polls providers.
    ///
    /// Used for virtual (remote-only) repos where provider data arrives
    /// via PeerData messages rather than local filesystem polling.
    pub fn idle() -> Self {
        let (_snapshot_tx, snapshot_rx) = watch::channel(Arc::new(RefreshSnapshot::default()));
        let refresh_trigger = Arc::new(Notify::new());

        // Spawn a task that just parks forever — it will be aborted on Drop.
        let task_handle = tokio::spawn(std::future::pending::<()>());

        Self { refresh_trigger, snapshot_rx, _task_handle: task_handle }
    }

    pub fn trigger_refresh(&self) {
        self.refresh_trigger.notify_one();
    }
}

impl Drop for RepoRefreshHandle {
    fn drop(&mut self) {
        self._task_handle.abort();
    }
}

/// Collect results from parallel provider requests, separating successes from errors.
async fn collect_named_results<T, Fut>(requests: Vec<(String, Fut)>) -> (Vec<T>, Vec<(String, String)>)
where
    Fut: Future<Output = Result<Vec<T>, String>>,
{
    let results = futures::future::join_all(requests.into_iter().map(|(name, fut)| async move { (name, fut.await) })).await;

    let mut entries = Vec::new();
    let mut errs = Vec::new();
    for (name, result) in results {
        match result {
            Ok(mut items) => entries.append(&mut items),
            Err(e) => errs.push((name, e)),
        }
    }
    (entries, errs)
}

fn provider_has_error(errors: &[RefreshError], provider: &str, categories: &[&str]) -> bool {
    errors.iter().any(|e| categories.contains(&e.category) && e.provider == provider)
}

fn insert_category_health<I>(
    health: &mut HashMap<(&'static str, String), bool>,
    errors: &[RefreshError],
    health_category: &'static str,
    provider_names: I,
    error_categories: &[&str],
) where
    I: IntoIterator<Item = String>,
{
    for name in provider_names {
        let has_error = provider_has_error(errors, &name, error_categories);
        health.insert((health_category, name), !has_error);
    }
}

/// Fetch all provider data into the given ProviderData struct.
async fn refresh_providers(
    pd: &mut ProviderData,
    repo_root: &Path,
    registry: &ProviderRegistry,
    criteria: &RepoCriteria,
    attachable_store: &SharedAttachableStore,
    agent_state_store: &crate::agents::SharedAgentStateStore,
) -> Vec<RefreshError> {
    let mut errors = Vec::new();

    let checkouts_fut = async {
        if let Some((desc, cm)) = registry.checkout_managers.preferred_with_desc() {
            let name = desc.display_name.clone();
            match cm.list_checkouts(repo_root).await {
                Ok(entries) => (entries, vec![]),
                Err(e) => (vec![], vec![(name, e)]),
            }
        } else {
            (vec![], vec![])
        }
    };

    let cr_fut = collect_named_results(
        registry.change_requests.iter().map(|(desc, cr)| (desc.display_name.clone(), cr.list_change_requests(repo_root, 20))).collect(),
    );

    let sessions_fut = collect_named_results(
        registry.cloud_agents.iter().map(|(desc, ca)| (desc.display_name.clone(), ca.list_sessions(criteria))).collect(),
    );

    let branches_fut = collect_named_results(
        registry.vcs.iter().map(|(desc, vcs)| (desc.display_name.clone(), vcs.list_remote_branches(repo_root))).collect(),
    );

    let merged_fut = collect_named_results(
        registry.change_requests.iter().map(|(desc, cr)| (desc.display_name.clone(), cr.list_merged_branch_names(repo_root, 50))).collect(),
    );

    let ws_fut = async {
        if let Some((desc, ws_mgr)) = registry.workspace_managers.preferred_with_desc() {
            let name = desc.display_name.clone();
            match ws_mgr.list_workspaces().await {
                Ok(entries) => (entries, vec![]),
                Err(e) => (vec![], vec![(name, e)]),
            }
        } else {
            (vec![], vec![])
        }
    };

    let tp_fut = async {
        if let Some((desc, tp)) = registry.terminal_pools.preferred_with_desc() {
            let name = desc.display_name.clone();
            match tp.list_terminals().await {
                Ok(entries) => (entries, vec![]),
                Err(e) => (vec![], vec![(name, e)]),
            }
        } else {
            (vec![], vec![])
        }
    };

    let (
        (checkouts, checkout_errors),
        (crs, cr_errors),
        (sessions, session_errors),
        (branches, branch_errors),
        (merged, merged_errors),
        (workspaces, ws_errors),
        (managed_terminals, tp_errors),
    ) = tokio::join!(checkouts_fut, cr_fut, sessions_fut, branches_fut, merged_fut, ws_fut, tp_fut);

    fn collect_errors(errors: &mut Vec<RefreshError>, category: &'static str, provider_errors: Vec<(String, String)>) {
        for (provider, message) in provider_errors {
            errors.push(RefreshError { category, provider, message });
        }
    }

    let local_host = flotilla_protocol::HostName::local();
    pd.checkouts = checkouts.into_iter().map(|(path, co)| (flotilla_protocol::HostPath::new(local_host.clone(), path), co)).collect();
    collect_errors(&mut errors, "checkouts", checkout_errors);

    pd.change_requests = crs.into_iter().collect();
    collect_errors(&mut errors, "PRs", cr_errors);

    pd.sessions = sessions.into_iter().collect();
    collect_errors(&mut errors, "sessions", session_errors);

    pd.workspaces = workspaces.into_iter().collect();
    collect_errors(&mut errors, "workspaces", ws_errors);

    pd.managed_terminals = managed_terminals.into_iter().map(|t| (t.id.to_string(), t)).collect();
    collect_errors(&mut errors, "terminals", tp_errors);
    project_attachable_data(pd, registry, attachable_store);
    project_agent_data(pd, agent_state_store);
    {
        use flotilla_protocol::delta::{Branch, BranchStatus};
        let remote = branches;
        collect_errors(&mut errors, "branches", branch_errors);
        let merged_names = merged;
        collect_errors(&mut errors, "merged", merged_errors);
        for name in remote {
            pd.branches.insert(name, Branch { status: BranchStatus::Remote });
        }
        for name in merged_names {
            pd.branches.insert(name, Branch { status: BranchStatus::Merged });
        }
    }

    errors
}

fn project_attachable_data(pd: &mut ProviderData, registry: &ProviderRegistry, attachable_store: &SharedAttachableStore) {
    let terminal_provider = registry.terminal_pools.preferred_with_desc().map(|(desc, _)| desc.implementation.clone());
    let workspace_provider = registry.workspace_managers.preferred_with_desc().map(|(desc, _)| desc.implementation.clone());
    let Ok(store) = attachable_store.lock() else {
        tracing::warn!("attachable store lock poisoned while projecting provider data");
        return;
    };

    let mut referenced_sets = std::collections::HashSet::new();

    if let Some(provider_name) = terminal_provider.as_deref() {
        for terminal in pd.managed_terminals.values_mut() {
            let session_name = terminal_session_binding_ref(&terminal.id);
            let Some(attachable_id) = store.lookup_binding("terminal_pool", provider_name, BindingObjectKind::Attachable, &session_name)
            else {
                continue;
            };
            let attachable_id = flotilla_protocol::AttachableId::new(attachable_id.to_string());
            terminal.attachable_id = Some(attachable_id.clone());
            if let Some(attachable) = store.registry().attachables.get(&attachable_id) {
                terminal.attachable_set_id = Some(attachable.set_id.clone());
                referenced_sets.insert(attachable.set_id.clone());
            }
        }
    }

    if let Some(provider_name) = workspace_provider.as_deref() {
        for (ws_ref, workspace) in &mut pd.workspaces {
            let Some(set_id) = store.lookup_binding("workspace_manager", provider_name, BindingObjectKind::AttachableSet, ws_ref.as_str())
            else {
                continue;
            };
            let set_id = flotilla_protocol::AttachableSetId::new(set_id.to_string());
            workspace.attachable_set_id = Some(set_id.clone());
            referenced_sets.insert(set_id);
        }
    }

    let mut referenced_set_ids: Vec<_> = referenced_sets.into_iter().collect();
    referenced_set_ids.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    pd.attachable_sets =
        referenced_set_ids.into_iter().filter_map(|set_id| store.registry().sets.get(&set_id).cloned().map(|set| (set_id, set))).collect();
}

fn project_agent_data(pd: &mut ProviderData, agent_state_store: &crate::agents::SharedAgentStateStore) {
    let Ok(store) = agent_state_store.lock() else {
        tracing::warn!("agent state store lock poisoned while projecting agent data");
        return;
    };
    for (attachable_id, entry) in store.list_agents() {
        // Only include agents whose terminal is in this repo's managed_terminals.
        // Without this check, every agent would appear in every tracked repo.
        let matching_terminal = pd.managed_terminals.values().find(|t| t.attachable_id.as_ref() == Some(&attachable_id));
        let Some(terminal) = matching_terminal else {
            continue;
        };
        let correlation_keys = terminal
            .attachable_set_id
            .clone()
            .map(|set_id| vec![flotilla_protocol::CorrelationKey::AttachableSet(set_id)])
            .unwrap_or_default();

        pd.agents.insert(attachable_id.to_string(), flotilla_protocol::Agent {
            harness: entry.harness,
            status: entry.status,
            model: entry.model,
            context: flotilla_protocol::AgentContext::Local { attachable_id },
            correlation_keys,
            provider_name: "cli-agent".to_string(),
            provider_display_name: "CLI Agent".to_string(),
            item_noun: "agent".to_string(),
        });
    }
}

fn compute_provider_health(registry: &ProviderRegistry, errors: &[RefreshError]) -> HashMap<(&'static str, String), bool> {
    use crate::providers::discovery::ProviderCategory;

    let mut health = HashMap::new();

    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::CloudAgent.slug(),
        registry.cloud_agents.display_names().map(|s| s.to_string()),
        &["sessions"],
    );
    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::ChangeRequest.slug(),
        registry.change_requests.display_names().map(|s| s.to_string()),
        &["PRs", "merged"],
    );
    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::CheckoutManager.slug(),
        registry.checkout_managers.display_names().map(|s| s.to_string()),
        &["checkouts"],
    );
    insert_category_health(&mut health, errors, ProviderCategory::Vcs.slug(), registry.vcs.display_names().map(|s| s.to_string()), &[
        "branches",
    ]);
    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::WorkspaceManager.slug(),
        registry.workspace_managers.display_names().map(|s| s.to_string()),
        &["workspaces"],
    );
    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::TerminalPool.slug(),
        registry.terminal_pools.display_names().map(|s| s.to_string()),
        &["terminals"],
    );

    health
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::PathBuf, sync::Arc};

    use async_trait::async_trait;

    use super::*;
    use crate::providers::{
        change_request::ChangeRequestTracker,
        coding_agent::CloudAgentService,
        discovery::{ProviderCategory, ProviderDescriptor},
        terminal::TerminalPool,
        types::*,
        vcs::{CheckoutManager, Vcs},
        workspace::WorkspaceManager,
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
        async fn list_checkouts(&self, _repo_root: &Path) -> Result<Vec<(PathBuf, Checkout)>, String> {
            self.result.clone()
        }

        async fn create_checkout(&self, _repo_root: &Path, _branch: &str, _create_branch: bool) -> Result<(PathBuf, Checkout), String> {
            Err("not implemented".to_string())
        }

        async fn remove_checkout(&self, _repo_root: &Path, _branch: &str) -> Result<(), String> {
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
        fn resolve_repo_root(&self, _path: &Path) -> Option<PathBuf> {
            None
        }

        async fn list_local_branches(&self, _repo_root: &Path) -> Result<Vec<BranchInfo>, String> {
            Ok(vec![])
        }

        async fn list_remote_branches(&self, _repo_root: &Path) -> Result<Vec<String>, String> {
            self.result.clone()
        }

        async fn commit_log(&self, _repo_root: &Path, _branch: &str, _limit: usize) -> Result<Vec<CommitInfo>, String> {
            Ok(vec![])
        }

        async fn ahead_behind(&self, _repo_root: &Path, _branch: &str, _reference: &str) -> Result<AheadBehind, String> {
            Ok(AheadBehind { ahead: 0, behind: 0 })
        }

        async fn working_tree_status(&self, _repo_root: &Path, _checkout_path: &Path) -> Result<WorkingTreeStatus, String> {
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
    impl WorkspaceManager for MockWorkspaceManager {
        async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
            self.result.clone()
        }

        async fn create_workspace(&self, _config: &WorkspaceConfig) -> Result<(String, Workspace), String> {
            Err("not implemented".to_string())
        }

        async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
            Ok(())
        }
    }

    struct MockTerminalPool {
        result: Result<Vec<flotilla_protocol::ManagedTerminal>, String>,
    }

    impl MockTerminalPool {
        fn ok(terminals: Vec<flotilla_protocol::ManagedTerminal>) -> Self {
            Self { result: Ok(terminals) }
        }
    }

    #[async_trait]
    impl TerminalPool for MockTerminalPool {
        async fn list_terminals(&self) -> Result<Vec<flotilla_protocol::ManagedTerminal>, String> {
            self.result.clone()
        }

        async fn ensure_running(&self, _id: &flotilla_protocol::ManagedTerminalId, _command: &str, _cwd: &Path) -> Result<(), String> {
            Ok(())
        }

        async fn attach_command(
            &self,
            _id: &flotilla_protocol::ManagedTerminalId,
            _command: &str,
            _cwd: &Path,
            _env_vars: &crate::providers::terminal::TerminalEnvVars,
        ) -> Result<String, String> {
            Ok("mock attach".into())
        }

        async fn kill_terminal(&self, _id: &flotilla_protocol::ManagedTerminalId) -> Result<(), String> {
            Ok(())
        }
    }

    fn repo_root() -> PathBuf {
        PathBuf::from("/tmp/test-repo")
    }

    fn criteria() -> RepoCriteria {
        RepoCriteria::default()
    }

    fn make_checkout(branch: &str) -> Checkout {
        Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![CorrelationKey::Branch(branch.to_string())],
            association_keys: vec![],
        }
    }

    fn make_change_request(title: &str, branch: &str) -> ChangeRequest {
        ChangeRequest {
            title: title.to_string(),
            branch: branch.to_string(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![CorrelationKey::Branch(branch.to_string())],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }

    fn make_session(title: &str, session_id: &str) -> CloudAgentSession {
        CloudAgentSession {
            title: title.to_string(),
            status: SessionStatus::Running,
            model: None,
            updated_at: None,
            correlation_keys: vec![CorrelationKey::SessionRef("mock".to_string(), session_id.to_string())],
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        }
    }

    fn make_workspace(name: &str) -> Workspace {
        Workspace { name: name.to_string(), directories: vec![], correlation_keys: vec![], attachable_set_id: None }
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
            Arc::new(MockCheckoutManager::ok(vec![(PathBuf::from("/tmp/wt/feat-a"), make_checkout("feat-a"))])),
        );
        registry.change_requests.insert(
            "github",
            desc("github"),
            Arc::new(MockChangeRequestTracker::ok(vec![("42".to_string(), make_change_request("Add feature", "feat-a"))], vec![
                "shared".to_string()
            ])),
        );
        registry.cloud_agents.insert(
            "claude",
            desc("claude"),
            Arc::new(MockCloudAgent::ok(vec![("sess-1".to_string(), make_session("Debug", "sess-1"))])),
        );
        registry.vcs.insert("git", desc("git"), Arc::new(MockVcs::ok(vec!["remote-only".to_string(), "shared".to_string()])));
        registry.workspace_managers.insert(
            "cmux",
            desc("cmux"),
            Arc::new(MockWorkspaceManager::ok(vec![("ws-1".to_string(), make_workspace("dev"))])),
        );

        let mut pd = ProviderData::default();
        let errors =
            refresh_providers(&mut pd, &repo_root(), &registry, &criteria(), &test_attachable_store(), &test_agent_state_store()).await;

        assert!(errors.is_empty());
        assert_eq!(pd.checkouts.len(), 1);
        assert_eq!(pd.change_requests.len(), 1);
        assert_eq!(pd.sessions.len(), 1);
        assert_eq!(pd.workspaces.len(), 1);
        assert_eq!(pd.branches.len(), 2);
        assert_eq!(pd.branches.get("remote-only").unwrap().status, BranchStatus::Remote);
        assert_eq!(pd.branches.get("shared").unwrap().status, BranchStatus::Merged);
    }

    #[test]
    fn project_attachable_data_populates_sets_and_ids() {
        let mut registry = ProviderRegistry::new();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::ok(vec![])));
        registry.terminal_pools.insert("shpool", desc("shpool"), Arc::new(MockTerminalPool::ok(vec![])));

        let store_dir = tempfile::tempdir().expect("tempdir");
        let attachable_store = crate::attachable::shared_file_backed_attachable_store(store_dir.path());
        let set_id = {
            let mut store = attachable_store.lock().expect("lock store");
            let set_id = store.ensure_terminal_set(
                Some(flotilla_protocol::HostName::local()),
                Some(flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), PathBuf::from("/tmp/wt-feat"))),
            );
            let _attachable_id = store.ensure_terminal_attachable(
                &set_id,
                "terminal_pool",
                "shpool",
                "flotilla/feat/dev/0",
                crate::attachable::TerminalPurpose { checkout: "feat".into(), role: "dev".into(), index: 0 },
                "bash",
                PathBuf::from("/tmp/wt-feat"),
                flotilla_protocol::TerminalStatus::Running,
            );
            store.replace_binding(crate::attachable::ProviderBinding {
                provider_category: "workspace_manager".into(),
                provider_name: "cmux".into(),
                object_kind: crate::attachable::BindingObjectKind::AttachableSet,
                object_id: set_id.to_string(),
                external_ref: "ws-1".into(),
            });
            set_id
        };

        let mut pd = ProviderData::default();
        pd.workspaces.insert("ws-1".into(), make_workspace("dev"));
        pd.managed_terminals.insert("feat/dev/0".into(), flotilla_protocol::ManagedTerminal {
            id: flotilla_protocol::ManagedTerminalId { checkout: "feat".into(), role: "dev".into(), index: 0 },
            role: "dev".into(),
            command: "bash".into(),
            working_directory: PathBuf::from("/tmp/wt-feat"),
            status: flotilla_protocol::TerminalStatus::Running,
            attachable_id: None,
            attachable_set_id: None,
        });

        project_attachable_data(&mut pd, &registry, &attachable_store);

        assert_eq!(pd.attachable_sets.len(), 1);
        assert!(pd.attachable_sets.contains_key(&set_id));
        assert_eq!(pd.workspaces.get("ws-1").and_then(|ws| ws.attachable_set_id.as_ref()), Some(&set_id));
        assert_eq!(pd.managed_terminals["feat/dev/0"].attachable_set_id.as_ref(), Some(&set_id));
        assert!(pd.managed_terminals["feat/dev/0"].attachable_id.is_some());
    }

    #[tokio::test]
    async fn refresh_reports_checkout_errors() {
        let mut registry = ProviderRegistry::new();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("checkout failed")));

        let mut pd = ProviderData::default();
        let errors =
            refresh_providers(&mut pd, &repo_root(), &registry, &criteria(), &test_attachable_store(), &test_agent_state_store()).await;

        assert!(errors.iter().any(|e| e.category == "checkouts"));
        assert!(pd.checkouts.is_empty());
    }

    #[tokio::test]
    async fn refresh_collects_multiple_errors_and_preserves_successful_providers() {
        let mut registry = ProviderRegistry::new();
        registry.checkout_managers.insert(
            "wt",
            desc("wt"),
            Arc::new(MockCheckoutManager::ok(vec![(PathBuf::from("/tmp/wt/feat-a"), make_checkout("feat-a"))])),
        );
        registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker::failing("pr fail", "merged fail")));
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::failing("sessions fail")));
        registry.vcs.insert("git", desc("git"), Arc::new(MockVcs::failing("branches fail")));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("workspaces fail")));

        let mut pd = ProviderData::default();
        let errors =
            refresh_providers(&mut pd, &repo_root(), &registry, &criteria(), &test_attachable_store(), &test_agent_state_store()).await;

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
}
