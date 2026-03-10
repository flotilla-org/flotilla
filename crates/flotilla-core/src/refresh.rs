use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::data::{self, CorrelationResult, RefreshError};
use crate::provider_data::ProviderData;
use crate::providers::correlation::CorrelatedGroup;
use crate::providers::registry::ProviderRegistry;
use crate::providers::types::RepoCriteria;

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
                    refresh_providers(&mut provider_data, &repo_root, &registry, &criteria).await;
                let provider_health = compute_provider_health(&registry, &errors);

                // Correlate
                let providers = Arc::new(provider_data);
                let (work_items, correlation_groups) = data::correlate(&providers);

                let snapshot = Arc::new(RefreshSnapshot {
                    providers,
                    work_items,
                    correlation_groups,
                    errors,
                    provider_health,
                });

                // Publish — receivers will see has_changed().
                // Break if receiver is dropped (handle dropped without Drop running).
                if snapshot_tx.send(snapshot).is_err() {
                    break;
                }
            }
        });

        Self {
            refresh_trigger,
            snapshot_rx,
            _task_handle: task_handle,
        }
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

/// Fetch all provider data into the given ProviderData struct.
async fn refresh_providers(
    pd: &mut ProviderData,
    repo_root: &Path,
    registry: &ProviderRegistry,
    criteria: &RepoCriteria,
) -> Vec<RefreshError> {
    let mut errors = Vec::new();

    let checkouts_fut = async {
        if let Some(cm) = registry.checkout_managers.values().next() {
            cm.list_checkouts(repo_root).await
        } else {
            Ok(vec![])
        }
    };

    let cr_fut = async {
        if let Some(cr) = registry.code_review.values().next() {
            cr.list_change_requests(repo_root, 20).await
        } else {
            Ok(vec![])
        }
    };

    let sessions_fut = async {
        if registry.cloud_agents.is_empty() {
            return (vec![], vec![]);
        }
        let results = futures::future::join_all(registry.cloud_agents.iter().map(|(name, ca)| {
            let provider = name.clone();
            async move { (provider, ca.list_sessions(criteria).await) }
        }))
        .await;

        let mut sessions = Vec::new();
        let mut session_errors = Vec::new();
        for (provider, result) in results {
            match result {
                Ok(mut entries) => sessions.append(&mut entries),
                Err(e) => session_errors.push(format!("{provider}: {e}")),
            }
        }
        (sessions, session_errors)
    };

    let branches_fut = async {
        if let Some(vcs) = registry.vcs.values().next() {
            vcs.list_remote_branches(repo_root).await
        } else {
            Ok(vec![])
        }
    };

    let merged_fut = async {
        if let Some(cr) = registry.code_review.values().next() {
            cr.list_merged_branch_names(repo_root, 50).await
        } else {
            Ok(vec![])
        }
    };

    let ws_fut = async {
        if let Some((_, ws_mgr)) = &registry.workspace_manager {
            ws_mgr.list_workspaces().await
        } else {
            Ok(vec![])
        }
    };

    let tp_fut = async {
        if let Some((_, tp)) = &registry.terminal_pool {
            tp.list_terminals().await
        } else {
            Ok(vec![])
        }
    };

    let (checkouts, crs, sessions_bundle, branches, merged, workspaces, managed_terminals) = tokio::join!(
        checkouts_fut,
        cr_fut,
        sessions_fut,
        branches_fut,
        merged_fut,
        ws_fut,
        tp_fut
    );

    pd.checkouts = checkouts
        .unwrap_or_else(|e| {
            errors.push(RefreshError {
                category: "checkouts",
                provider: String::new(),
                message: e,
            });
            Vec::new()
        })
        .into_iter()
        .collect();
    pd.change_requests = crs
        .unwrap_or_else(|e| {
            errors.push(RefreshError {
                category: "PRs",
                provider: String::new(),
                message: e,
            });
            Vec::new()
        })
        .into_iter()
        .collect();
    pd.workspaces = workspaces
        .unwrap_or_else(|e| {
            errors.push(RefreshError {
                category: "workspaces",
                provider: String::new(),
                message: e,
            });
            Vec::new()
        })
        .into_iter()
        .collect();
    pd.managed_terminals = managed_terminals
        .unwrap_or_else(|e| {
            errors.push(RefreshError {
                category: "terminals",
                provider: String::new(),
                message: e,
            });
            Vec::new()
        })
        .into_iter()
        .map(|t| (t.id.to_string(), t))
        .collect();
    let (sessions, session_errors) = sessions_bundle;
    if !session_errors.is_empty() {
        errors.push(RefreshError {
            category: "sessions",
            provider: String::new(),
            message: session_errors.join("; "),
        });
    }
    pd.sessions = sessions.into_iter().collect();
    {
        use flotilla_protocol::delta::{Branch, BranchStatus};
        let remote = branches.unwrap_or_else(|e| {
            errors.push(RefreshError {
                category: "branches",
                provider: String::new(),
                message: e,
            });
            Vec::new()
        });
        let merged_names = merged.unwrap_or_else(|e| {
            errors.push(RefreshError {
                category: "merged",
                provider: String::new(),
                message: e,
            });
            Vec::new()
        });
        for name in remote {
            pd.branches.insert(
                name,
                Branch {
                    status: BranchStatus::Remote,
                },
            );
        }
        for name in merged_names {
            pd.branches.insert(
                name,
                Branch {
                    status: BranchStatus::Merged,
                },
            );
        }
    }

    errors
}

fn compute_provider_health(
    registry: &ProviderRegistry,
    errors: &[RefreshError],
) -> HashMap<(&'static str, String), bool> {
    let mut health = HashMap::new();
    if registry.cloud_agents.values().next().is_some() {
        health.insert(
            ("cloud_agent", String::new()),
            !errors.iter().any(|e| e.category == "sessions"),
        );
    }
    if registry.code_review.values().next().is_some() {
        health.insert(
            ("code_review", String::new()),
            !errors
                .iter()
                .any(|e| e.category == "PRs" || e.category == "merged"),
        );
    }
    if registry.terminal_pool.is_some() {
        health.insert(
            ("terminal_pool", String::new()),
            !errors.iter().any(|e| e.category == "terminals"),
        );
    }
    health
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::providers::code_review::CodeReview;
    use crate::providers::coding_agent::CloudAgentService;
    use crate::providers::types::*;
    use crate::providers::vcs::{CheckoutManager, Vcs};
    use crate::providers::workspace::WorkspaceManager;

    struct MockCheckoutManager {
        result: Result<Vec<(PathBuf, Checkout)>, String>,
    }

    impl MockCheckoutManager {
        fn ok(checkouts: Vec<(PathBuf, Checkout)>) -> Self {
            Self {
                result: Ok(checkouts),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                result: Err(msg.to_string()),
            }
        }
    }

    #[async_trait]
    impl CheckoutManager for MockCheckoutManager {
        fn display_name(&self) -> &str {
            "mock-checkout"
        }

        async fn list_checkouts(
            &self,
            _repo_root: &Path,
        ) -> Result<Vec<(PathBuf, Checkout)>, String> {
            self.result.clone()
        }

        async fn create_checkout(
            &self,
            _repo_root: &Path,
            _branch: &str,
            _create_branch: bool,
        ) -> Result<(PathBuf, Checkout), String> {
            Err("not implemented".to_string())
        }

        async fn remove_checkout(&self, _repo_root: &Path, _branch: &str) -> Result<(), String> {
            Err("not implemented".to_string())
        }
    }

    struct MockCodeReview {
        change_requests_result: Result<Vec<(String, ChangeRequest)>, String>,
        merged_result: Result<Vec<String>, String>,
    }

    impl MockCodeReview {
        fn ok(change_requests: Vec<(String, ChangeRequest)>, merged_branches: Vec<String>) -> Self {
            Self {
                change_requests_result: Ok(change_requests),
                merged_result: Ok(merged_branches),
            }
        }

        fn failing(change_requests_msg: &str, merged_msg: &str) -> Self {
            Self {
                change_requests_result: Err(change_requests_msg.to_string()),
                merged_result: Err(merged_msg.to_string()),
            }
        }
    }

    #[async_trait]
    impl CodeReview for MockCodeReview {
        fn display_name(&self) -> &str {
            "mock-cr"
        }

        async fn list_change_requests(
            &self,
            _repo_root: &Path,
            _limit: usize,
        ) -> Result<Vec<(String, ChangeRequest)>, String> {
            self.change_requests_result.clone()
        }

        async fn get_change_request(
            &self,
            _repo_root: &Path,
            _id: &str,
        ) -> Result<(String, ChangeRequest), String> {
            Err("not implemented".to_string())
        }

        async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
            Ok(())
        }

        async fn list_merged_branch_names(
            &self,
            _repo_root: &Path,
            _limit: usize,
        ) -> Result<Vec<String>, String> {
            self.merged_result.clone()
        }
    }

    struct MockCloudAgent {
        result: Result<Vec<(String, CloudAgentSession)>, String>,
    }

    impl MockCloudAgent {
        fn ok(sessions: Vec<(String, CloudAgentSession)>) -> Self {
            Self {
                result: Ok(sessions),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                result: Err(msg.to_string()),
            }
        }
    }

    #[async_trait]
    impl CloudAgentService for MockCloudAgent {
        fn display_name(&self) -> &str {
            "mock-agent"
        }

        async fn list_sessions(
            &self,
            _criteria: &RepoCriteria,
        ) -> Result<Vec<(String, CloudAgentSession)>, String> {
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
            Self {
                result: Ok(branches),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                result: Err(msg.to_string()),
            }
        }
    }

    #[async_trait]
    impl Vcs for MockVcs {
        fn display_name(&self) -> &str {
            "mock-vcs"
        }

        fn resolve_repo_root(&self, _path: &Path) -> Option<PathBuf> {
            None
        }

        async fn list_local_branches(&self, _repo_root: &Path) -> Result<Vec<BranchInfo>, String> {
            Ok(vec![])
        }

        async fn list_remote_branches(&self, _repo_root: &Path) -> Result<Vec<String>, String> {
            self.result.clone()
        }

        async fn commit_log(
            &self,
            _repo_root: &Path,
            _branch: &str,
            _limit: usize,
        ) -> Result<Vec<CommitInfo>, String> {
            Ok(vec![])
        }

        async fn ahead_behind(
            &self,
            _repo_root: &Path,
            _branch: &str,
            _reference: &str,
        ) -> Result<AheadBehind, String> {
            Ok(AheadBehind {
                ahead: 0,
                behind: 0,
            })
        }

        async fn working_tree_status(
            &self,
            _repo_root: &Path,
            _checkout_path: &Path,
        ) -> Result<WorkingTreeStatus, String> {
            Ok(WorkingTreeStatus::default())
        }
    }

    struct MockWorkspaceManager {
        result: Result<Vec<(String, Workspace)>, String>,
    }

    impl MockWorkspaceManager {
        fn ok(workspaces: Vec<(String, Workspace)>) -> Self {
            Self {
                result: Ok(workspaces),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                result: Err(msg.to_string()),
            }
        }
    }

    #[async_trait]
    impl WorkspaceManager for MockWorkspaceManager {
        fn display_name(&self) -> &str {
            "mock-ws"
        }

        async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
            self.result.clone()
        }

        async fn create_workspace(
            &self,
            _config: &WorkspaceConfig,
        ) -> Result<(String, Workspace), String> {
            Err("not implemented".to_string())
        }

        async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
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
            is_trunk: false,
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
        }
    }

    fn make_session(title: &str, session_id: &str) -> CloudAgentSession {
        CloudAgentSession {
            title: title.to_string(),
            status: SessionStatus::Running,
            model: None,
            updated_at: None,
            correlation_keys: vec![CorrelationKey::SessionRef(
                "mock".to_string(),
                session_id.to_string(),
            )],
        }
    }

    fn make_workspace(name: &str) -> Workspace {
        Workspace {
            name: name.to_string(),
            directories: vec![],
            correlation_keys: vec![],
        }
    }

    fn refresh_error(category: &'static str) -> RefreshError {
        RefreshError {
            category,
            provider: String::new(),
            message: format!("{category} failure"),
        }
    }

    async fn wait_for_snapshot(
        rx: &mut tokio::sync::watch::Receiver<Arc<RefreshSnapshot>>,
    ) -> Arc<RefreshSnapshot> {
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

    #[test]
    fn compute_provider_health_maps_error_categories() {
        let mut registry = ProviderRegistry::new();
        registry
            .cloud_agents
            .insert("claude".to_string(), Arc::new(MockCloudAgent::ok(vec![])));
        registry.code_review.insert(
            "github".to_string(),
            Arc::new(MockCodeReview::ok(vec![], vec![])),
        );

        let cases = vec![
            (vec![], true, true),
            (vec![refresh_error("sessions")], false, true),
            (vec![refresh_error("PRs")], true, false),
            (vec![refresh_error("merged")], true, false),
            (vec![refresh_error("checkouts")], true, true),
            (
                vec![refresh_error("sessions"), refresh_error("PRs")],
                false,
                false,
            ),
        ];

        for (errors, expected_coding, expected_review) in cases {
            let health = compute_provider_health(&registry, &errors);
            assert_eq!(
                health.get(&("cloud_agent", String::new())),
                Some(&expected_coding)
            );
            assert_eq!(
                health.get(&("code_review", String::new())),
                Some(&expected_review)
            );
        }
    }

    #[tokio::test]
    async fn refresh_empty_registry_produces_empty_data() {
        let mut pd = ProviderData::default();
        let errors =
            refresh_providers(&mut pd, &repo_root(), &ProviderRegistry::new(), &criteria()).await;

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
            "wt".to_string(),
            Arc::new(MockCheckoutManager::ok(vec![(
                PathBuf::from("/tmp/wt/feat-a"),
                make_checkout("feat-a"),
            )])),
        );
        registry.code_review.insert(
            "github".to_string(),
            Arc::new(MockCodeReview::ok(
                vec![(
                    "42".to_string(),
                    make_change_request("Add feature", "feat-a"),
                )],
                vec!["shared".to_string()],
            )),
        );
        registry.cloud_agents.insert(
            "claude".to_string(),
            Arc::new(MockCloudAgent::ok(vec![(
                "sess-1".to_string(),
                make_session("Debug", "sess-1"),
            )])),
        );
        registry.vcs.insert(
            "git".to_string(),
            Arc::new(MockVcs::ok(vec![
                "remote-only".to_string(),
                "shared".to_string(),
            ])),
        );
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::ok(vec![(
                "ws-1".to_string(),
                make_workspace("dev"),
            )])),
        ));

        let mut pd = ProviderData::default();
        let errors = refresh_providers(&mut pd, &repo_root(), &registry, &criteria()).await;

        assert!(errors.is_empty());
        assert_eq!(pd.checkouts.len(), 1);
        assert_eq!(pd.change_requests.len(), 1);
        assert_eq!(pd.sessions.len(), 1);
        assert_eq!(pd.workspaces.len(), 1);
        assert_eq!(pd.branches.len(), 2);
        assert_eq!(
            pd.branches.get("remote-only").unwrap().status,
            BranchStatus::Remote
        );
        assert_eq!(
            pd.branches.get("shared").unwrap().status,
            BranchStatus::Merged
        );
    }

    #[tokio::test]
    async fn refresh_reports_checkout_errors() {
        let mut registry = ProviderRegistry::new();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::failing("checkout failed")),
        );

        let mut pd = ProviderData::default();
        let errors = refresh_providers(&mut pd, &repo_root(), &registry, &criteria()).await;

        assert!(errors.iter().any(|e| e.category == "checkouts"));
        assert!(pd.checkouts.is_empty());
    }

    #[tokio::test]
    async fn refresh_collects_multiple_errors_and_preserves_successful_providers() {
        let mut registry = ProviderRegistry::new();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::ok(vec![(
                PathBuf::from("/tmp/wt/feat-a"),
                make_checkout("feat-a"),
            )])),
        );
        registry.code_review.insert(
            "github".to_string(),
            Arc::new(MockCodeReview::failing("pr fail", "merged fail")),
        );
        registry.cloud_agents.insert(
            "claude".to_string(),
            Arc::new(MockCloudAgent::failing("sessions fail")),
        );
        registry.vcs.insert(
            "git".to_string(),
            Arc::new(MockVcs::failing("branches fail")),
        );
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::failing("workspaces fail")),
        ));

        let mut pd = ProviderData::default();
        let errors = refresh_providers(&mut pd, &repo_root(), &registry, &criteria()).await;

        let categories: HashSet<&str> = errors.iter().map(|e| e.category).collect();
        for expected in ["PRs", "merged", "sessions", "branches", "workspaces"] {
            assert!(
                categories.contains(expected),
                "missing error category: {expected}"
            );
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
        registry.cloud_agents.insert(
            "claude".to_string(),
            Arc::new(MockCloudAgent::failing("agent offline")),
        );

        let handle = RepoRefreshHandle::spawn(
            repo_root(),
            Arc::new(registry),
            criteria(),
            Duration::from_secs(3600),
        );

        let mut rx = handle.snapshot_rx.clone();
        let snapshot = wait_for_snapshot(&mut rx).await;
        assert!(snapshot.errors.iter().any(|e| e.category == "sessions"));
        assert_eq!(
            snapshot
                .provider_health
                .get(&("cloud_agent", String::new())),
            Some(&false)
        );
    }

    #[tokio::test]
    async fn trigger_refresh_produces_another_snapshot() {
        let handle = RepoRefreshHandle::spawn(
            repo_root(),
            Arc::new(ProviderRegistry::new()),
            criteria(),
            Duration::from_secs(3600),
        );

        let mut rx = handle.snapshot_rx.clone();
        wait_for_snapshot(&mut rx).await;

        handle.trigger_refresh();
        let snapshot = wait_for_snapshot(&mut rx).await;
        assert!(snapshot.errors.is_empty());
    }
}
