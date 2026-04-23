use std::{collections::VecDeque, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use crossterm::event::KeyCode;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{
        discovery::test_support::{fake_discovery_with_provider_set, FakeDiscoveryProviders},
        issue_query::{IssueQuery, IssueQueryService, IssueResultPage},
    },
};
use flotilla_protocol::{
    provider_data::Issue, qualified_path::HostId, Change, EnvironmentId, EnvironmentInfo, EnvironmentStatus, HostSummary, ImageId, NodeId,
    NodeInfo, ProvisioningTarget, RepoSelector, WorkItemIdentity,
};
use tempfile::tempdir;
use test_support::*;
use tokio::sync::{Mutex as TokioMutex, Notify};

use super::*;

fn insert_host(
    model: &mut TuiModel,
    host_name: &str,
    environment_id: EnvironmentId,
    node_id: NodeId,
    display_name: &str,
    is_local: bool,
    status: PeerStatus,
) {
    let host_name = HostName::new(host_name);
    model.hosts.insert(environment_id.clone(), TuiHostState {
        environment_id: environment_id.clone(),
        host_name: host_name.clone(),
        is_local,
        status,
        summary: HostSummary {
            environment_id,
            host_name: Some(host_name.clone()),
            node: NodeInfo::new(node_id, display_name),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });
}

fn insert_local_host(model: &mut TuiModel, name: &str) {
    let host_name = HostName::new(name);
    let environment_id = EnvironmentId::host(HostId::new(format!("{name}-local-env")));
    insert_host(model, host_name.as_str(), environment_id, NodeId::new(format!("node-{name}-local")), name, true, PeerStatus::Connected);
}

fn insert_peer_host(model: &mut TuiModel, name: &str, status: PeerStatus) {
    let environment_id = EnvironmentId::host(HostId::new(format!("{name}-peer-env")));
    insert_host(model, name, environment_id, NodeId::new(format!("node-{name}-peer")), name, false, status);
}

#[derive(Clone)]
struct QueryStep {
    expected_page: u32,
    gate: Option<Arc<Notify>>,
    result: IssueResultPage,
}

struct ScriptedIssueQueryService {
    steps: TokioMutex<VecDeque<QueryStep>>,
    requests: TokioMutex<Vec<u32>>,
}

impl ScriptedIssueQueryService {
    fn new(steps: Vec<QueryStep>) -> Self {
        Self { steps: TokioMutex::new(steps.into()), requests: TokioMutex::new(Vec::new()) }
    }

    async fn requests(&self) -> Vec<u32> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl IssueQueryService for ScriptedIssueQueryService {
    async fn query(&self, _repo: &Path, _params: &IssueQuery, page: u32, _count: usize) -> Result<IssueResultPage, String> {
        self.requests.lock().await.push(page);
        let step = self.steps.lock().await.pop_front().expect("unexpected issue query");
        assert_eq!(step.expected_page, page, "unexpected page requested");
        if let Some(gate) = step.gate {
            gate.notified().await;
        }
        Ok(step.result)
    }

    async fn fetch_by_ids(&self, _repo: &Path, _ids: &[String]) -> Result<Vec<(String, Issue)>, String> {
        Ok(vec![])
    }

    async fn open_in_browser(&self, _repo: &Path, _id: &str) -> Result<(), String> {
        Ok(())
    }
}

fn issue_row(id: usize) -> (String, Issue) {
    (id.to_string(), Issue {
        title: format!("Issue {id}"),
        labels: vec![],
        association_keys: vec![],
        provider_name: "fake".into(),
        provider_display_name: "Fake".into(),
    })
}

fn issue_page(range: std::ops::RangeInclusive<usize>, has_more: bool) -> IssueResultPage {
    IssueResultPage { items: range.map(issue_row).collect(), total: None, has_more }
}

fn daemon_test_config_store(config_dir: PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&config_dir).expect("create daemon config dir");
    std::fs::write(config_dir.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    Arc::new(ConfigStore::with_base(config_dir))
}

async fn app_with_issue_query_service(service: Arc<dyn IssueQueryService>) -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>, App) {
    let temp = tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_issue_query_service(service));
    let daemon =
        InProcessDaemon::new(vec![repo.clone()], daemon_test_config_store(temp.path().join("daemon-config")), discovery, HostName::local())
            .await;
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh repo");

    let daemon_handle: Arc<dyn DaemonHandle> = daemon.clone();
    let repos = daemon_handle.list_repos().await.expect("list repos");
    let app = App::new(daemon_handle, repos, Arc::new(ConfigStore::with_base(temp.path().join("tui-config"))), Theme::classic());
    (temp, repo, daemon, app)
}

// -- CommandQueue --

#[test]
fn command_queue_push_and_take_fifo() {
    let mut q = CommandQueue::default();
    q.push(Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::Refresh { repo: None } });
    q.push(Command {
        node_id: None,
        provisioning_target: None,
        context_repo: Some(RepoSelector::Path(PathBuf::from("/repo"))),
        action: CommandAction::OpenChangeRequest { id: "1".into() },
    });
    assert!(matches!(q.take_next(), Some((Command { action: CommandAction::Refresh { .. }, .. }, _))));
    assert!(matches!(q.take_next(), Some((Command { action: CommandAction::OpenChangeRequest { .. }, .. }, _))));
}

#[test]
fn command_queue_empty_returns_none() {
    let mut q = CommandQueue::default();
    assert!(q.take_next().is_none());
}

// -- TuiModel::repo_name --

#[test]
fn repo_name_extracts_directory_name() {
    assert_eq!(TuiModel::repo_name(Path::new("/home/user/project")), "project");
}

#[test]
fn repo_name_root_path() {
    let name = TuiModel::repo_name(Path::new("/"));
    assert_eq!(name, "/");
}

// -- TuiModel::from_repo_info --

#[test]
fn from_repo_info_builds_correct_model() {
    let repos_info =
        vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default()), repo_info("/tmp/repo-b", "repo-b", RepoLabels::default())];
    let model = TuiModel::from_repo_info(repos_info);
    assert_eq!(model.repos.len(), 2);
    assert_eq!(model.repo_order.len(), 2);
    assert_eq!(model.active_repo, 0);
    assert!(model.repos.values().any(|repo| repo.path.as_path() == Path::new("/tmp/repo-a")));
    assert!(model.repos.values().any(|repo| repo.path.as_path() == Path::new("/tmp/repo-b")));
    assert!(model.status_message.is_none());
}

#[test]
fn from_repo_info_preserves_order() {
    let repos_info = vec![repo_info("/z", "z", RepoLabels::default()), repo_info("/a", "a", RepoLabels::default())];
    let model = TuiModel::from_repo_info(repos_info);
    assert_eq!(model.repos[&model.repo_order[0]].path, PathBuf::from("/z"));
    assert_eq!(model.repos[&model.repo_order[1]].path, PathBuf::from("/a"));
}

#[test]
fn from_repo_info_empty() {
    let model = TuiModel::from_repo_info(vec![]);
    assert!(model.repos.is_empty());
    assert!(model.repo_order.is_empty());
}

#[test]
fn app_new_loads_layout_from_config() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[ui.preview]\nlayout = \"below\"\n").unwrap();

    let daemon: Arc<dyn DaemonHandle> = Arc::new(test_support::StubDaemon::new());
    let config = Arc::new(ConfigStore::with_base(dir.path()));
    let app = App::new(daemon, vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())], config, Theme::classic());

    assert_eq!(app.ui.view_layout, RepoViewLayout::Below);
}

#[test]
fn persist_layout_writes_current_ui_state() {
    let dir = tempdir().unwrap();
    let daemon: Arc<dyn DaemonHandle> = Arc::new(test_support::StubDaemon::new());
    let config = Arc::new(ConfigStore::with_base(dir.path()));
    let mut app = App::new(daemon, vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())], config, Theme::classic());

    app.ui.view_layout = RepoViewLayout::Right;
    app.persist_layout();

    let reloaded = ConfigStore::with_base(dir.path());
    let cfg = reloaded.load_config();
    assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Right);
}

// -- format_error_status --

#[test]
fn format_error_status_no_errors() {
    assert!(format_error_status(&[], Path::new("/repo")).is_none());
}

#[test]
fn format_error_status_single_error() {
    let errors = vec![provider_error("change_request", "github", "rate limited")];
    let msg = format_error_status(&errors, Path::new("/tmp/my-repo")).unwrap();
    assert!(msg.contains("my-repo"));
    assert!(msg.contains("change_request"));
    assert!(msg.contains("rate limited"));
    assert!(msg.contains("(github)"));
}

#[test]
fn format_error_status_suppresses_issues_disabled() {
    let errors = vec![provider_error("issues", "github", "repo has disabled issues")];
    assert!(format_error_status(&errors, Path::new("/repo")).is_none());
}

#[test]
fn format_error_status_mixed_suppressed_and_real() {
    let errors = vec![provider_error("issues", "github", "repo has disabled issues"), provider_error("vcs", "git", "not a git repo")];
    let msg = format_error_status(&errors, Path::new("/repo")).unwrap();
    assert!(msg.contains("not a git repo"));
    assert!(!msg.contains("disabled issues"));
}

#[test]
fn format_error_status_empty_provider_no_suffix() {
    let errors = vec![provider_error("vcs", "", "error")];
    let msg = format_error_status(&errors, Path::new("/r")).unwrap();
    assert!(!msg.contains("()"));
}

#[test]
fn format_error_status_multiple_errors_joined() {
    let errors = vec![provider_error("vcs", "git", "err1"), provider_error("cr", "gh", "err2")];
    let msg = format_error_status(&errors, Path::new("/r")).unwrap();
    assert!(msg.contains("; "));
}

#[test]
fn apply_snapshot_updates_provider_data() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let snap = snapshot(&repo_path);
    app.apply_snapshot(snap);
    assert!(!app.model.repos[&repo].loading);
}

#[tokio::test]
async fn repeated_snapshots_do_not_queue_duplicate_initial_issue_pages() {
    let page_one_gate = Arc::new(Notify::new());
    let service = Arc::new(ScriptedIssueQueryService::new(vec![
        QueryStep { expected_page: 1, gate: Some(Arc::clone(&page_one_gate)), result: issue_page(1..=50, true) },
        QueryStep { expected_page: 1, gate: Some(Arc::clone(&page_one_gate)), result: issue_page(1..=50, true) },
    ]));
    let (_temp, repo, daemon, mut app) = app_with_issue_query_service(service.clone()).await;

    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("repo snapshot");
    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot.clone())));
    tokio::time::sleep(Duration::from_millis(10)).await;

    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot)));
    tokio::time::sleep(Duration::from_millis(10)).await;

    assert_eq!(service.requests().await, vec![1], "initial issue fetch should stay in-flight instead of queueing a duplicate page 1");

    page_one_gate.notify_waiters();
    tokio::time::sleep(Duration::from_millis(10)).await;
    app.drain_background_updates();
}

#[tokio::test]
async fn infinite_scroll_appends_the_next_issue_page_without_duplicates() {
    let service = Arc::new(ScriptedIssueQueryService::new(vec![
        QueryStep { expected_page: 1, gate: None, result: issue_page(1..=50, true) },
        QueryStep { expected_page: 2, gate: None, result: issue_page(51..=60, false) },
    ]));
    let (_temp, repo, daemon, mut app) = app_with_issue_query_service(service.clone()).await;

    let snapshot = daemon.get_state(&RepoSelector::Path(repo)).await.expect("repo snapshot");
    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot)));
    tokio::time::sleep(Duration::from_millis(10)).await;
    app.drain_background_updates();

    let repo_identity = app.model.active_repo_identity().clone();
    let page = app.screen.repo_pages.get_mut(&repo_identity).expect("repo page");
    page.reconcile_if_changed();
    page.table.select_flat_index(44);

    app.handle_key(key(KeyCode::Char('j')));
    tokio::time::sleep(Duration::from_millis(10)).await;
    app.drain_background_updates();

    let default = app.issue_views.get(&repo_identity).and_then(|view| view.default.as_ref()).expect("default issue view");
    let ids: Vec<&str> = default.items.iter().map(|(id, _)| id.as_str()).collect();
    let unique_ids: std::collections::HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(service.requests().await, vec![1, 2], "scrolling near the bottom should request exactly one next page");
    assert_eq!(ids.len(), 60, "the first two pages should be appended together");
    assert_eq!(unique_ids.len(), 60, "issue paging should not duplicate items");
    assert_eq!(ids.first().copied(), Some("1"));
    assert_eq!(ids.last().copied(), Some("60"));
}

#[test]
fn first_page_search_failure_clears_active_search_state() {
    use flotilla_protocol::issue_query::IssueQuery;

    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    if let Some(page) = app.screen.repo_pages.get_mut(&repo) {
        page.active_search_query = Some("beta".into());
    }

    let view = app.issue_views.entry(repo.clone()).or_default();
    view.search_query = Some("beta".into());
    view.search = Some(issue_view::IssuePagingState {
        params: IssueQuery { search: Some("beta".into()) },
        items: vec![],
        next_page: 1,
        total: None,
        has_more: true,
        fetch_pending: true,
    });

    app.issue_update_tx
        .send(issue_view::IssueQueryUpdate::QueryFailed {
            repo: repo.clone(),
            params: IssueQuery { search: Some("beta".into()) },
            requested_page: 1,
            message: "search failed".into(),
        })
        .expect("send");
    app.drain_background_updates();

    let page = app.screen.repo_pages.get(&repo).expect("repo page");
    assert!(page.active_search_query.is_none(), "visible search state should clear after first-page failure");

    let view = app.issue_views.get(&repo).expect("issue view");
    assert!(view.search_query.is_none(), "search query bookkeeping should clear after first-page failure");
    assert!(view.search.is_none(), "failed first page should not leave a pending search state behind");
}

#[test]
fn apply_snapshot_maps_provider_health_to_statuses() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let mut snap = snapshot(&repo_path);
    snap.provider_health.insert("vcs".into(), HashMap::from([("git".into(), true), ("wt".into(), false)]));
    app.apply_snapshot(snap);

    assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "git".into())], ProviderStatus::Ok,);
    assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "wt".into())], ProviderStatus::Error,);
}

#[test]
fn apply_snapshot_sets_error_status_message() {
    let mut app = stub_app();
    let repo_path = active_repo_path(&app);

    let mut snap = snapshot(&repo_path);
    snap.errors = vec![provider_error("cr", "gh", "fail")];
    app.apply_snapshot(snap);

    assert!(app.model.status_message.is_some());
    assert!(app.model.status_message.as_ref().unwrap().contains("fail"));
}

#[test]
fn dismissing_status_message_hides_only_that_message() {
    let mut app = stub_app();
    app.set_status_message(Some("rate limit exceeded".into()));

    let id = app.visible_status_items()[0].id;
    app.dismiss_status_item(id);

    assert!(app.visible_status_items().is_empty());
}

#[test]
fn new_status_message_reappears_after_dismissing_old_one() {
    let mut app = stub_app();
    app.set_status_message(Some("old error".into()));
    app.dismiss_status_item(0);

    app.set_status_message(Some("new error".into()));

    assert_eq!(app.visible_status_items(), vec![VisibleStatusItem { id: 0, text: "ERROR new error".into() }]);
}

#[test]
fn visible_status_items_use_shared_error_and_peer_labels() {
    let mut app = stub_app();
    app.set_status_message(Some("boom".into()));
    insert_peer_host(&mut app.model, "host-a", PeerStatus::Disconnected);

    assert_eq!(app.visible_status_items(), vec![VisibleStatusItem { id: 0, text: "ERROR boom".into() }, VisibleStatusItem {
        id: 1,
        text: "HOST DOWN host-a".into()
    },]);
}

#[test]
fn apply_snapshot_clears_status_on_no_errors() {
    let mut app = stub_app();
    let repo_path = active_repo_path(&app);
    app.set_status_message(Some("old error".into()));

    let snap = snapshot(&repo_path);
    app.apply_snapshot(snap);

    assert!(app.model.status_message.is_none());
}

#[test]
fn apply_snapshot_unknown_repo_is_noop() {
    let mut app = stub_app();
    let snap = snapshot(Path::new("/nonexistent"));
    app.apply_snapshot(snap);
}

#[test]
fn apply_snapshot_sets_unseen_changes_for_inactive_tab() {
    let mut app = stub_app_with_repos(2);
    let inactive_repo = app.model.repo_order[1].clone();
    let inactive_path = app.model.repos[&inactive_repo].path.clone();

    // First snapshot to establish baseline providers
    let snap1 = snapshot(&inactive_path);
    app.apply_snapshot(snap1);

    // Second snapshot with different providers
    let mut snap2 = snapshot(&inactive_path);
    snap2.seq = 2;
    snap2.work_items = vec![checkout_item("feat", "/wt", false)];
    let mut different_providers = ProviderData::default();
    different_providers.checkouts.insert(
        flotilla_protocol::HostPath::new(flotilla_protocol::HostName::new("test-host"), PathBuf::from("/wt")).into(),
        flotilla_protocol::Checkout {
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
    snap2.providers = different_providers;
    app.apply_snapshot(snap2);

    assert!(app.model.repos[&inactive_repo].has_unseen_changes);
}

// -- apply_delta --

#[test]
fn apply_delta_unknown_repo_is_noop() {
    let mut app = stub_app();
    let mut change = delta(Path::new("/nonexistent"), vec![]);
    change.seq = 1;
    change.prev_seq = 0;
    app.apply_delta(change);
}

#[test]
fn apply_delta_provider_health_added() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let change = delta(&repo_path, vec![flotilla_protocol::Change::ProviderHealth {
        category: "vcs".into(),
        provider: "git".into(),
        op: flotilla_protocol::EntryOp::Added(true),
    }]);
    app.apply_delta(change);

    assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "git".into())], ProviderStatus::Ok,);
    assert!(app.model.repos[&repo].provider_health["vcs"]["git"]);
}

#[test]
fn apply_delta_provider_health_removed() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    app.model.repos.get_mut(&repo).unwrap().provider_health.entry("vcs".into()).or_default().insert("git".into(), true);

    let change = delta(&repo_path, vec![flotilla_protocol::Change::ProviderHealth {
        category: "vcs".into(),
        provider: "git".into(),
        op: flotilla_protocol::EntryOp::Removed,
    }]);
    app.apply_delta(change);

    assert!(!app.model.repos[&repo].provider_health.contains_key("vcs"));
}

#[test]
fn apply_delta_errors_changed_updates_status() {
    let mut app = stub_app();
    let repo_path = active_repo_path(&app);

    let change = delta(&repo_path, vec![flotilla_protocol::Change::ErrorsChanged(vec![provider_error("cr", "gh", "broken")])]);
    app.apply_delta(change);

    assert!(app.model.status_message.as_ref().unwrap().contains("broken"));
}

#[test]
fn apply_delta_data_change_on_inactive_tab_sets_unseen() {
    let mut app = stub_app_with_repos(2);
    let inactive_repo = app.model.repo_order[1].clone();
    let inactive_path = app.model.repos[&inactive_repo].path.clone();

    let change = delta(&inactive_path, vec![flotilla_protocol::Change::Session {
        key: "s1".into(),
        op: flotilla_protocol::EntryOp::Added(flotilla_protocol::CloudAgentSession {
            title: "new session".into(),
            status: flotilla_protocol::SessionStatus::Running,
            model: None,
            updated_at: None,
            correlation_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        }),
    }]);
    app.apply_delta(change);

    assert!(app.model.repos[&inactive_repo].has_unseen_changes);
}

#[test]
fn apply_delta_health_only_change_does_not_set_unseen() {
    let mut app = stub_app_with_repos(2);
    let inactive_repo = app.model.repo_order[1].clone();
    let inactive_path = app.model.repos[&inactive_repo].path.clone();

    let change = delta(&inactive_path, vec![flotilla_protocol::Change::ProviderHealth {
        category: "vcs".into(),
        provider: "git".into(),
        op: flotilla_protocol::EntryOp::Added(true),
    }]);
    app.apply_delta(change);

    assert!(!app.model.repos[&inactive_repo].has_unseen_changes);
}

#[test]
fn apply_delta_work_item_changes_update_repo_data() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let mut snap = snapshot(&repo_path);
    snap.work_items = vec![issue_item("1")];
    app.apply_snapshot(snap);

    let mut updated_issue = issue_item("1");
    updated_issue.description = "Updated issue 1".into();
    let added_issue = issue_item("2");

    // Our branch sends full work_items with each delta, so set the expected list directly.
    let mut d = delta(&repo_path, vec![
        Change::WorkItem { identity: updated_issue.identity.clone(), op: flotilla_protocol::EntryOp::Updated(updated_issue.clone()) },
        Change::WorkItem { identity: added_issue.identity.clone(), op: flotilla_protocol::EntryOp::Added(added_issue.clone()) },
    ]);
    d.work_items = vec![updated_issue.clone(), added_issue.clone()];
    app.apply_delta(d);

    let data = app.repo_data[&repo].read();
    assert_eq!(data.work_items.len(), 2);
    assert!(data.work_items.iter().any(|item| item.identity == updated_issue.identity && item.description == "Updated issue 1"));
    assert!(data.work_items.iter().any(|item| item.identity == added_issue.identity));
}

#[test]
fn apply_delta_work_item_remove_updates_repo_data() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let mut snap = snapshot(&repo_path);
    snap.work_items = vec![issue_item("1"), issue_item("2")];
    app.apply_snapshot(snap);

    // Our branch sends full work_items with each delta, so set the expected list directly.
    let mut d = delta(&repo_path, vec![Change::WorkItem {
        identity: flotilla_protocol::WorkItemIdentity::Issue("1".into()),
        op: flotilla_protocol::EntryOp::Removed,
    }]);
    d.work_items = vec![issue_item("2")];
    app.apply_delta(d);

    let data = app.repo_data[&repo].read();
    assert_eq!(data.work_items.len(), 1);
    assert_eq!(data.work_items[0].identity, flotilla_protocol::WorkItemIdentity::Issue("2".into()));
}

// -- handle_repo_added / handle_repo_removed --

#[test]
fn handle_repo_added_adds_new_repo() {
    let mut app = stub_app();
    assert_eq!(app.model.repos.len(), 1);

    let info = repo_info("/tmp/new-repo", "new-repo", RepoLabels::default());
    app.handle_repo_added(info);

    assert_eq!(app.model.repos.len(), 2);
    assert!(app.model.repos.values().any(|repo| repo.path.as_path() == Path::new("/tmp/new-repo")));
    assert_eq!(app.model.repos[app.model.repo_order.last().unwrap()].path, PathBuf::from("/tmp/new-repo"));
    // Adding a repo should not switch to it (it may arrive asynchronously)
    assert_eq!(app.model.active_repo, 0);
}

#[test]
fn handle_repo_added_duplicate_is_noop() {
    let mut app = stub_app();
    let existing_path = app.model.repo_order[0].clone();
    let info = repo_info(app.model.repos[&existing_path].path.clone(), "dup", RepoLabels::default());
    app.handle_repo_added(info);
    assert_eq!(app.model.repos.len(), 1);
}

#[test]
fn handle_repo_removed_removes_repo() {
    let mut app = stub_app_with_repos(2);
    let path = app.model.repo_order[0].clone();

    app.handle_repo_removed(&path);

    assert_eq!(app.model.repos.len(), 1);
    assert!(!app.model.repos.contains_key(&path));
    assert!(!app.model.repo_order.contains(&path));
}

#[test]
fn handle_repo_removed_last_repo_sets_quit() {
    let mut app = stub_app();
    let path = app.model.repo_order[0].clone();

    app.handle_repo_removed(&path);

    assert!(app.should_quit);
}

#[test]
fn handle_repo_removed_adjusts_active_index() {
    let mut app = stub_app_with_repos(3);
    app.model.active_repo = 2;
    let last_path = app.model.repo_order[2].clone();

    app.handle_repo_removed(&last_path);

    assert_eq!(app.model.active_repo, 1);
}

#[test]
fn handle_repo_removed_syncs_layout_from_new_active_page() {
    let mut app = stub_app_with_repos(2);
    // Give the two repo pages different layouts.
    let repo0 = app.model.repo_order[0].clone();
    let repo1 = app.model.repo_order[1].clone();
    app.screen.repo_pages.get_mut(&repo0).expect("page 0").layout = RepoViewLayout::Zoom;
    app.screen.repo_pages.get_mut(&repo1).expect("page 1").layout = RepoViewLayout::Below;

    // Active repo is 1 (Below layout). Remove it.
    app.model.active_repo = 1;
    app.handle_repo_removed(&repo1);

    // Active repo should now be 0, and ui.view_layout should match its page.
    assert_eq!(app.model.active_repo, 0);
    assert_eq!(app.ui.view_layout, RepoViewLayout::Zoom);
}

// -- handle_daemon_event --

#[test]
fn handle_daemon_event_command_started_tracked() {
    let mut app = stub_app();
    let repo = app.model.active_repo_root().clone();

    app.handle_daemon_event(DaemonEvent::CommandStarted {
        command_id: 99,
        node_id: NodeId::new(HostName::local().as_str()),
        repo_identity: app.model.active_repo_identity().clone(),
        repo: Some(repo.clone()),
        description: "test cmd".into(),
    });

    assert!(app.in_flight.contains_key(&99));
    assert_eq!(app.in_flight[&99].description, "test cmd");
}

#[test]
fn step_failure_surfaces_error_in_status_message() {
    let mut app = stub_app();
    let repo_identity = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo_identity].path.clone();

    app.in_flight.insert(42, InFlightCommand {
        repo_identity: repo_identity.clone(),
        repo: repo_path.clone(),
        description: "Creating checkout...".into(),
    });

    app.handle_daemon_event(DaemonEvent::CommandStepUpdate {
        command_id: 42,
        node_id: NodeId::new(HostName::local().as_str()),
        repo_identity,
        repo: Some(repo_path),
        step_index: 0,
        step_count: 1,
        description: "Create checkout for branch my-branch".into(),
        status: StepStatus::Failed { message: "branch already exists: my-branch".into() },
    });

    let msg = app.model.status_message.as_deref().expect("status_message should be set");
    assert!(msg.contains("branch already exists"), "expected error detail in status message, got: {msg}");
}

#[test]
fn peer_disconnect_clears_selected_target_host() {
    let mut app = stub_app();
    app.ui.provisioning_target = ProvisioningTarget::Host { host: HostName::new("alpha") };
    insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);
    let peer_node_id = app.model.resolve_host(&HostName::new("alpha")).expect("alpha host").summary.node.node_id.clone();

    app.handle_daemon_event(DaemonEvent::PeerStatusChanged { node_id: peer_node_id, status: PeerConnectionState::Disconnected });

    assert_eq!(app.ui.provisioning_target, ProvisioningTarget::Host { host: HostName::local() });
    assert_eq!(app.model.resolve_host(&HostName::new("alpha")).unwrap().status, PeerStatus::Disconnected);
}

#[test]
fn host_removed_event_deletes_host_and_clears_selected_target_host() {
    let mut app = stub_app();
    app.ui.provisioning_target = ProvisioningTarget::Host { host: HostName::new("alpha") };
    insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);

    let environment_id = app.model.resolve_host(&HostName::new("alpha")).unwrap().environment_id.clone();
    app.handle_daemon_event(DaemonEvent::HostRemoved { environment_id, seq: 2 });

    assert_eq!(app.ui.provisioning_target, ProvisioningTarget::Host { host: HostName::local() });
    assert!(app.model.resolve_host(&HostName::new("alpha")).is_err());
}

// -- Convenience accessors --

#[test]
fn selected_work_item_none_when_no_selection() {
    let app = stub_app();
    assert!(app.selected_work_item().is_none());
}

#[test]
fn selected_work_item_returns_item() {
    let mut app = stub_app();
    setup_selectable_table(&mut app, vec![checkout_item("feat", "/wt", false)]);
    let item = app.selected_work_item();
    assert!(item.is_some());
    assert_eq!(item.unwrap().branch.as_deref(), Some("feat"));
}

// -- CloseConfirm flow (via widget stack) --

fn push_close_confirm_widget(app: &mut App, id: &str) {
    let widget = crate::widgets::close_confirm::CloseConfirmWidget::new(
        id.into(),
        "Test PR".into(),
        WorkItemIdentity::Session("test".into()),
        Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::CloseChangeRequest { id: id.into() },
        },
    );
    app.screen.modal_stack.push(Box::new(widget));
}

#[test]
fn close_confirm_y_dispatches_command() {
    let mut app = stub_app();
    push_close_confirm_widget(&mut app, "42");
    app.handle_key(key(KeyCode::Char('y')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let cmd = app.proto_commands.take_next();
    assert!(matches!(cmd, Some((Command { action: CommandAction::CloseChangeRequest { id }, .. }, _)) if id == "42"));
}

#[test]
fn close_confirm_enter_dispatches_command() {
    let mut app = stub_app();
    push_close_confirm_widget(&mut app, "42");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let cmd = app.proto_commands.take_next();
    assert!(matches!(cmd, Some((Command { action: CommandAction::CloseChangeRequest { id }, .. }, _)) if id == "42"));
}

#[test]
fn close_confirm_esc_cancels() {
    let mut app = stub_app();
    push_close_confirm_widget(&mut app, "42");
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn close_confirm_n_cancels() {
    let mut app = stub_app();
    push_close_confirm_widget(&mut app, "42");
    app.handle_key(key(KeyCode::Char('n')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
}

// -- CommandQueue with PendingActionContext --

#[test]
fn command_queue_push_with_context() {
    use crate::app::ui_state::PendingActionContext;

    let mut q = CommandQueue::default();
    let ctx = PendingActionContext {
        identity: WorkItemIdentity::Session("s1".into()),
        description: "Archive session".into(),
        repo_identity: RepoIdentity { authority: "local".into(), path: "/tmp/test-repo".into() },
    };
    q.push_with_context(
        Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::Refresh { repo: None } },
        Some(ctx),
    );
    let (cmd, ctx) = q.take_next().expect("should have one entry");
    assert!(matches!(cmd.action, CommandAction::Refresh { .. }));
    assert!(ctx.is_some());
    assert_eq!(ctx.unwrap().description, "Archive session");
}

#[test]
fn command_queue_push_without_context() {
    let mut q = CommandQueue::default();
    q.push(Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::Refresh { repo: None } });
    let (_, ctx) = q.take_next().expect("should have one entry");
    assert!(ctx.is_none());
}

// -- Pending action lifecycle on CommandFinished --

#[test]
fn command_finished_ok_clears_pending_action() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo].path.clone();
    let identity = WorkItemIdentity::Session("s1".into());

    app.screen.repo_pages.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
        command_id: 42,
        status: PendingStatus::InFlight,
        description: "test".into(),
    });
    app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        node_id: NodeId::new(HostName::local().as_str()),
        repo_identity: repo.clone(),
        repo: Some(repo_path),
        result: CommandValue::Ok,
    });

    assert!(!app.screen.repo_pages[&repo].pending_actions.contains_key(&identity));
}

#[test]
fn command_finished_error_transitions_to_failed() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo].path.clone();
    let identity = WorkItemIdentity::Session("s1".into());

    app.screen.repo_pages.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
        command_id: 42,
        status: PendingStatus::InFlight,
        description: "test".into(),
    });
    app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        node_id: NodeId::new(HostName::local().as_str()),
        repo_identity: repo.clone(),
        repo: Some(repo_path),
        result: CommandValue::Error { message: "boom".into() },
    });

    let pending = &app.screen.repo_pages[&repo].pending_actions[&identity];
    assert!(matches!(pending.status, PendingStatus::Failed(ref msg) if msg == "boom"));
}

#[test]
fn command_finished_cancelled_clears_pending_action() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo].path.clone();
    let identity = WorkItemIdentity::Session("s1".into());

    app.screen.repo_pages.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
        command_id: 42,
        status: PendingStatus::InFlight,
        description: "test".into(),
    });
    app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        node_id: NodeId::new(HostName::local().as_str()),
        repo_identity: repo.clone(),
        repo: Some(repo_path),
        result: CommandValue::Cancelled,
    });

    assert!(!app.screen.repo_pages[&repo].pending_actions.contains_key(&identity));
}

#[test]
fn orphaned_command_finished_harmlessly_ignored() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo].path.clone();
    let identity = WorkItemIdentity::Session("s1".into());

    // Insert pending action with command_id 99 (different from finished event)
    app.screen.repo_pages.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
        command_id: 99,
        status: PendingStatus::InFlight,
        description: "test".into(),
    });
    app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        node_id: NodeId::new(HostName::local().as_str()),
        repo_identity: repo.clone(),
        repo: Some(repo_path),
        result: CommandValue::Ok,
    });

    // The pending action with command_id 99 should still be there
    assert!(app.screen.repo_pages[&repo].pending_actions.contains_key(&identity));
}

#[test]
fn local_checkout_created_does_not_queue_workspace() {
    let mut app = stub_app();
    insert_local_host(&mut app.model, "my-desktop");
    let repo_identity = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo_identity].path.clone();

    app.in_flight.insert(42, InFlightCommand { repo_identity: repo_identity.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        node_id: NodeId::new("my-desktop"),
        repo_identity,
        repo: Some(repo_path),
        result: CommandValue::CheckoutCreated { branch: "feat".into(), path: PathBuf::from("/tmp/repo/wt-feat") },
    });

    assert!(app.proto_commands.take_next().is_none(), "workspace creation is now handled by checkout plan, not TUI");
}

#[test]
fn remote_checkout_created_does_not_queue_workspace() {
    let mut app = stub_app();
    insert_local_host(&mut app.model, "my-desktop");
    let repo_identity = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo_identity].path.clone();

    app.in_flight.insert(42, InFlightCommand { repo_identity: repo_identity.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        node_id: NodeId::new("remote-a"),
        repo_identity,
        repo: Some(repo_path),
        result: CommandValue::CheckoutCreated { branch: "feat".into(), path: PathBuf::from("/remote/wt-feat") },
    });

    assert!(app.proto_commands.take_next().is_none(), "remote checkout should not auto-create local workspace");
}

// -- TuiHostState / hosts map --

#[test]
fn host_snapshot_event_populates_hosts_map() {
    let mut app = stub_app();
    app.handle_daemon_event(DaemonEvent::HostSnapshot(Box::new(flotilla_protocol::HostSnapshot {
        seq: 1,
        environment_id: EnvironmentId::host(HostId::new("desktop-env")),
        node: NodeInfo::new(NodeId::new("desktop"), "desktop"),
        is_local: true,
        connection_status: PeerConnectionState::Connected,
        summary: HostSummary {
            environment_id: EnvironmentId::host(HostId::new("desktop-env")),
            host_name: Some(HostName::new("desktop")),
            node: NodeInfo::new(NodeId::new("desktop"), "desktop"),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    })));
    assert!(app.model.my_host().is_some());
    assert!(app.model.resolve_host(&HostName::new("desktop")).unwrap().is_local);
}

#[test]
fn stub_app_starts_with_local_host_snapshot() {
    let app = stub_app();
    assert_eq!(app.model.my_host(), Some(&HostName::local()));
}

#[test]
fn peer_host_names_returns_sorted_non_local() {
    let mut app = stub_app();
    insert_local_host(&mut app.model, "local");
    insert_peer_host(&mut app.model, "beta", PeerStatus::Connected);
    insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);
    assert_eq!(app.model.peer_host_names(), vec![HostName::new("alpha"), HostName::new("beta")]);
}

#[test]
fn duplicate_host_names_do_not_collide_and_resolve_as_ambiguous() {
    let mut app = stub_app();

    let first_env = EnvironmentId::host(HostId::new("desktop-a"));
    let second_env = EnvironmentId::host(HostId::new("desktop-b"));

    app.model.hosts.insert(first_env.clone(), TuiHostState {
        environment_id: first_env.clone(),
        host_name: HostName::new("desktop"),
        is_local: false,
        status: PeerStatus::Connected,
        summary: HostSummary {
            environment_id: first_env.clone(),
            host_name: Some(HostName::new("desktop")),
            node: NodeInfo::new(NodeId::new("node-a"), "Desktop"),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });
    app.model.hosts.insert(second_env.clone(), TuiHostState {
        environment_id: second_env.clone(),
        host_name: HostName::new("desktop"),
        is_local: false,
        status: PeerStatus::Connected,
        summary: HostSummary {
            environment_id: second_env.clone(),
            host_name: Some(HostName::new("desktop")),
            node: NodeInfo::new(NodeId::new("node-b"), "Desktop"),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });

    assert_eq!(app.model.hosts.len(), 3);
    let err = app.model.resolve_host(&HostName::new("desktop")).expect_err("duplicate host names should be ambiguous");
    assert!(err.contains("ambiguous host: desktop"), "unexpected error: {err}");
    assert!(err.contains("desktop-a"), "unexpected error: {err}");
    assert!(err.contains("desktop-b"), "unexpected error: {err}");
}

#[test]
fn local_and_remote_duplicate_host_names_are_ambiguous() {
    let mut app = stub_app();
    insert_local_host(&mut app.model, "desktop");
    insert_peer_host(&mut app.model, "desktop", PeerStatus::Connected);

    let target = ProvisioningTarget::Host { host: HostName::new("desktop") };
    let err = app.validate_provisioning_target(&target).expect_err("duplicate host names should be rejected");

    assert!(err.contains("ambiguous host: desktop"), "unexpected error: {err}");
}

#[test]
fn host_target_resolution_is_equivalent_for_many_nodes_and_shared_node_topologies() {
    let beta_environment_id = EnvironmentId::host(HostId::new("beta-host"));

    let mut many_nodes = stub_app();
    insert_host(
        &mut many_nodes.model,
        "alpha",
        EnvironmentId::host(HostId::new("alpha-host")),
        NodeId::new("node-alpha"),
        "Alpha",
        false,
        PeerStatus::Connected,
    );
    insert_host(&mut many_nodes.model, "beta", beta_environment_id.clone(), NodeId::new("node-beta"), "Beta", false, PeerStatus::Connected);
    insert_host(
        &mut many_nodes.model,
        "gamma",
        EnvironmentId::host(HostId::new("gamma-host")),
        NodeId::new("node-gamma"),
        "Gamma",
        false,
        PeerStatus::Connected,
    );

    let mut shared_node = stub_app();
    let shared_node_id = NodeId::new("node-hub");
    insert_host(
        &mut shared_node.model,
        "alpha",
        EnvironmentId::host(HostId::new("alpha-host")),
        shared_node_id.clone(),
        "Hub",
        false,
        PeerStatus::Connected,
    );
    insert_host(&mut shared_node.model, "beta", beta_environment_id.clone(), shared_node_id.clone(), "Hub", false, PeerStatus::Connected);
    insert_host(
        &mut shared_node.model,
        "gamma",
        EnvironmentId::host(HostId::new("gamma-host")),
        shared_node_id.clone(),
        "Hub",
        false,
        PeerStatus::Connected,
    );

    let many_nodes_host = many_nodes.model.resolve_host(&HostName::new("beta")).expect("resolve beta in many-nodes topology");
    let shared_node_host = shared_node.model.resolve_host(&HostName::new("beta")).expect("resolve beta in shared-node topology");

    assert_eq!(many_nodes_host.environment_id, beta_environment_id);
    assert_eq!(shared_node_host.environment_id, beta_environment_id);
    assert_ne!(many_nodes_host.summary.node.node_id, shared_node_host.summary.node.node_id);

    let (many_nodes_target_node, many_nodes_target) =
        many_nodes.model.resolve_environment_target(&beta_environment_id).expect("many-nodes environment target");
    let (shared_node_target_node, shared_node_target) =
        shared_node.model.resolve_environment_target(&beta_environment_id).expect("shared-node environment target");

    assert_eq!(many_nodes_target, ProvisioningTarget::Host { host: HostName::new("beta") });
    assert_eq!(shared_node_target, ProvisioningTarget::Host { host: HostName::new("beta") });
    assert_eq!(many_nodes_target_node, NodeId::new("node-beta"));
    assert_eq!(shared_node_target_node, shared_node_id);
}

#[test]
fn resolve_environment_target_accepts_host_environment_identity_directly() {
    let mut app = stub_app();
    insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);

    let host = app.model.resolve_host(&HostName::new("alpha")).expect("host");
    let expected_environment_id = host.environment_id.clone();
    let expected_node_id = host.summary.node.node_id.clone();

    let (node_id, target) = app.model.resolve_environment_target(&expected_environment_id).expect("resolve target");
    assert_eq!(node_id, expected_node_id);
    assert_eq!(target, ProvisioningTarget::Host { host: HostName::new("alpha") });
}

#[test]
fn resolve_environment_target_direct_non_host_environment_keeps_existing_environment_target() {
    let mut app = stub_app();
    let host_name = HostName::new("alpha");
    let environment_id = EnvironmentId::new("builder-1");
    app.model.hosts.insert(environment_id.clone(), TuiHostState {
        environment_id: environment_id.clone(),
        host_name: host_name.clone(),
        is_local: false,
        status: PeerStatus::Connected,
        summary: HostSummary {
            environment_id: environment_id.clone(),
            host_name: Some(host_name.clone()),
            node: NodeInfo::new(NodeId::new("node-alpha"), "Desktop"),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });

    let (node_id, target) = app.model.resolve_environment_target(&environment_id).expect("resolve target");
    assert_eq!(node_id, NodeId::new("node-alpha"));
    assert_eq!(target, ProvisioningTarget::ExistingEnvironment { host: host_name, env_id: environment_id });
}

#[test]
fn resolve_environment_target_accepts_non_host_environment_identity_directly() {
    let mut app = stub_app();
    let host_name = HostName::new("alpha");
    let environment_id = EnvironmentId::host(HostId::new("alpha-env"));
    let nested_env = EnvironmentId::new("builder-1");
    app.model.hosts.insert(environment_id.clone(), TuiHostState {
        environment_id: environment_id.clone(),
        host_name: host_name.clone(),
        is_local: false,
        status: PeerStatus::Connected,
        summary: HostSummary {
            environment_id,
            host_name: Some(host_name.clone()),
            node: NodeInfo::new(NodeId::new("node-alpha"), "Desktop"),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![EnvironmentInfo::Provisioned {
                id: nested_env.clone(),
                display_name: Some("builder".into()),
                image: ImageId::new("ubuntu:24.04"),
                status: EnvironmentStatus::Running,
            }],
        },
    });

    let (node_id, target) = app.model.resolve_environment_target(&nested_env).expect("resolve target");
    assert_eq!(node_id, NodeId::new("node-alpha"));
    assert_eq!(target, ProvisioningTarget::ExistingEnvironment { host: host_name, env_id: nested_env });
}

// -- Namespace snapshot / delta handling --

fn test_convoy(
    namespace: &str,
    name: &str,
    phase: flotilla_protocol::namespace::ConvoyPhase,
    initializing: bool,
) -> flotilla_protocol::namespace::ConvoySummary {
    flotilla_protocol::namespace::ConvoySummary {
        id: flotilla_protocol::namespace::ConvoyId::new(namespace, name),
        namespace: namespace.into(),
        name: name.into(),
        workflow_ref: "wf".into(),
        phase,
        message: None,
        repo_hint: None,
        tasks: vec![],
        started_at: None,
        finished_at: None,
        observed_workflow_ref: None,
        initializing,
    }
}

#[test]
fn app_applies_namespace_snapshot() {
    use flotilla_protocol::{
        namespace::{ConvoyPhase, NamespaceSnapshot},
        DaemonEvent,
    };

    let mut app = stub_app();
    let convoy = test_convoy("flotilla", "x", ConvoyPhase::Active, false);

    app.handle_daemon_event(DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![convoy],
    })));

    assert_eq!(app.convoys("flotilla").len(), 1);
    assert_eq!(app.convoys("flotilla")[0].name, "x");
}

#[test]
fn app_applies_namespace_delta() {
    use flotilla_protocol::{
        namespace::{ConvoyPhase, NamespaceDelta, NamespaceSnapshot},
        DaemonEvent,
    };

    let mut app = stub_app();
    let convoy = test_convoy("flotilla", "x", ConvoyPhase::Pending, true);

    app.handle_daemon_event(DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![convoy.clone()],
    })));

    // Update phase via delta
    let mut modified = convoy.clone();
    modified.phase = ConvoyPhase::Active;
    modified.initializing = false;
    app.handle_daemon_event(DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
        seq: 2,
        namespace: "flotilla".into(),
        changed: vec![modified],
        removed: vec![],
    })));
    assert_eq!(app.convoys("flotilla")[0].phase, ConvoyPhase::Active);

    // Remove via delta
    app.handle_daemon_event(DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
        seq: 3,
        namespace: "flotilla".into(),
        changed: vec![],
        removed: vec![convoy.id.clone()],
    })));
    assert!(app.convoys("flotilla").is_empty());
}

// -- Convoys tab rendering --

#[test]
fn screen_renders_convoys_page_on_convoys_tab() {
    use flotilla_protocol::{
        namespace::{ConvoyPhase, NamespaceSnapshot},
        DaemonEvent,
    };
    use ratatui::{backend::TestBackend, Terminal};

    use crate::widgets::InteractiveWidget as _;

    let mut app = stub_app();

    // Feed a namespace snapshot with one convoy named "demo".
    app.handle_daemon_event(DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![test_convoy("flotilla", "demo", ConvoyPhase::Active, false)],
    })));

    // Switch to the Convoys tab.
    app.ui.is_config = false;
    app.ui.is_convoys = true;

    // Render into a test terminal.
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");

    let convoys_selected = app.convoys_ui.selected.clone();
    let convoy_filter = app.convoys_ui.filter.clone();
    terminal
        .draw(|f| {
            let area = f.area();
            let mut ctx = crate::widgets::RenderContext {
                model: &app.model,
                ui: &mut app.ui,
                theme: &app.theme,
                keymap: &app.keymap,
                in_flight: &app.in_flight,
                namespaces: &app.namespaces,
                convoys_selected,
                convoy_filter: &convoy_filter,
            };
            app.screen.render(f, area, &mut ctx);
        })
        .expect("draw");

    let rendered: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
    assert!(rendered.contains("Convoys"), "expected 'Convoys' title, got: {rendered}");
    assert!(rendered.contains("demo"), "expected convoy name 'demo' in rendered output, got: {rendered}");
}

// -- Convoys tab selection --

fn make_namespace_snapshot(names: &[&str]) -> flotilla_protocol::namespace::NamespaceSnapshot {
    flotilla_protocol::namespace::NamespaceSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: names.iter().map(|n| test_convoy("flotilla", n, flotilla_protocol::namespace::ConvoyPhase::Active, false)).collect(),
    }
}

#[test]
fn convoys_tab_select_next_advances_selection() {
    use crate::keymap::Action;

    let mut app = stub_app();
    app.handle_daemon_event(DaemonEvent::NamespaceSnapshot(Box::new(make_namespace_snapshot(&["a", "b", "c"]))));
    app.ui.is_config = false;
    app.ui.is_convoys = true;

    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("a"), "first convoy should be auto-selected");

    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("b"));

    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("c"));

    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("c"), "should clamp at last convoy");

    // Also verify via convoys_tab_select_delta directly
    app.convoys_tab_select_delta(-10);
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("a"), "large negative delta clamps at first convoy");

    let _ = Action::SelectNext; // confirm Action is reachable in this module
}

#[test]
fn convoys_tab_select_prev_moves_back() {
    let mut app = stub_app();
    app.handle_daemon_event(DaemonEvent::NamespaceSnapshot(Box::new(make_namespace_snapshot(&["a", "b", "c"]))));
    app.ui.is_config = false;
    app.ui.is_convoys = true;

    // Move to the end
    app.convoys_tab_select_delta(2);
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("c"));

    app.handle_key(key(KeyCode::Char('k')));
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("b"));

    app.handle_key(key(KeyCode::Char('k')));
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("a"));

    app.handle_key(key(KeyCode::Char('k')));
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("a"), "should clamp at first convoy");
}

#[test]
fn delta_removing_selected_convoy_reselects_to_adjacent() {
    use flotilla_protocol::{
        namespace::{ConvoyId, NamespaceDelta, NamespaceSnapshot},
        DaemonEvent,
    };

    let mut app = stub_app();
    app.handle_daemon_event(DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![
            test_convoy("flotilla", "a", flotilla_protocol::namespace::ConvoyPhase::Active, false),
            test_convoy("flotilla", "b", flotilla_protocol::namespace::ConvoyPhase::Active, false),
            test_convoy("flotilla", "c", flotilla_protocol::namespace::ConvoyPhase::Active, false),
        ],
    })));
    app.ui.is_config = false;
    app.ui.is_convoys = true;

    // Move to "b"
    app.convoys_tab_select_delta(1);
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("b"));

    // Delta removes "b"
    app.handle_daemon_event(DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
        seq: 2,
        namespace: "flotilla".into(),
        changed: vec![],
        removed: vec![ConvoyId::new("flotilla", "b")],
    })));

    let selected = app.selected_convoy_id().expect("must still have a selection after removing 'b'");
    assert!(
        matches!(selected.name(), "a" | "c"),
        "after removing 'b', selection should fall back to an adjacent remaining convoy, got {}",
        selected.name()
    );
}

#[test]
fn delta_removing_all_convoys_clears_selection() {
    use flotilla_protocol::{
        namespace::{ConvoyId, NamespaceDelta, NamespaceSnapshot},
        DaemonEvent,
    };

    let mut app = stub_app();
    app.handle_daemon_event(DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![test_convoy("flotilla", "a", flotilla_protocol::namespace::ConvoyPhase::Active, false)],
    })));
    app.ui.is_config = false;
    app.ui.is_convoys = true;

    assert!(app.selected_convoy_id().is_some(), "should have a selection after snapshot");

    app.handle_daemon_event(DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
        seq: 2,
        namespace: "flotilla".into(),
        changed: vec![],
        removed: vec![ConvoyId::new("flotilla", "a")],
    })));

    assert!(app.selected_convoy_id().is_none(), "removing the last convoy should clear selection");
}
