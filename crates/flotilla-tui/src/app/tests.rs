use std::{collections::VecDeque, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{
        discovery::test_support::{fake_discovery_with_provider_set, FakeDiscoveryProviders},
        issue_tracker::IssueProvider,
    },
};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    provider_data::Issue,
    qualified_path::{HostId, QualifiedPath},
    test_support::TestIssue,
    Change, EnvironmentId, EnvironmentInfo, EnvironmentStatus, HostSummary, ImageId, IssueChangeset, IssueRef, IssueSource, NodeId,
    NodeInfo, ProvisioningTarget, RepoIdentity, RepoSelector, ViewAddress, WorkItemIdentity,
};
use ratatui::{backend::TestBackend, Terminal};
use tempfile::tempdir;
use test_support::*;
use tokio::sync::{watch, Mutex as TokioMutex, Semaphore};

use super::*;
use crate::widgets::InteractiveWidget;

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
    gate: Option<Arc<Semaphore>>,
    result: IssueResultPage,
}

struct ScriptedIssueProvider {
    steps: TokioMutex<VecDeque<QueryStep>>,
    requests: watch::Sender<Vec<u32>>,
}

impl ScriptedIssueProvider {
    fn new(steps: Vec<QueryStep>) -> Self {
        Self { steps: TokioMutex::new(steps.into()), requests: watch::channel(Vec::new()).0 }
    }

    fn requests(&self) -> Vec<u32> {
        self.requests.borrow().clone()
    }
}

#[async_trait]
impl IssueProvider for ScriptedIssueProvider {
    fn supports(&self, _source: &IssueSource) -> bool {
        true
    }

    async fn query(&self, _source: &IssueSource, _params: &IssueQuery, page: u32, _count: usize) -> Result<IssueResultPage, String> {
        self.requests.send_modify(|requests| requests.push(page));
        let step = self.steps.lock().await.pop_front().expect("unexpected issue query");
        assert_eq!(step.expected_page, page, "unexpected page requested");
        if let Some(gate) = step.gate {
            gate.acquire().await.expect("query gate should remain open").forget();
        }
        Ok(step.result)
    }

    async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String> {
        Err(format!("issue {} not found", reference.id))
    }

    async fn list_changed_since(&self, _source: &IssueSource, _since: &str, _count: usize) -> Result<IssueChangeset, String> {
        Ok(IssueChangeset { updated: vec![], closed: vec![], has_more: false })
    }

    async fn open_in_browser(&self, _reference: &IssueRef) -> Result<(), String> {
        Ok(())
    }
}

fn issue_row(id: usize) -> Issue {
    TestIssue::new(&format!("Issue {id}")).id(id.to_string()).build()
}

fn issue_page(range: std::ops::RangeInclusive<usize>, has_more: bool) -> IssueResultPage {
    IssueResultPage { items: range.map(issue_row).collect(), total: None, has_more }
}

fn daemon_test_config_store(config_dir: PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&config_dir).expect("create daemon config dir");
    std::fs::write(config_dir.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    Arc::new(ConfigStore::with_base(config_dir))
}

async fn app_with_issue_provider(service: Arc<dyn IssueProvider>) -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>, App) {
    let temp = tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_issue_tracker(service));
    let daemon =
        InProcessDaemon::new(vec![repo.clone()], daemon_test_config_store(temp.path().join("daemon-config")), discovery, HostName::local())
            .await;
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh repo");

    let daemon_handle: Arc<dyn DaemonHandle> = daemon.clone();
    let repos = daemon_handle.list_repos().await.expect("list repos");
    let app = App::new(daemon_handle, repos, Arc::new(ConfigStore::with_base(temp.path().join("tui-config"))), Theme::classic());
    (temp, repo, daemon, app)
}

async fn drain_until_requests(app: &mut App, service: &ScriptedIssueProvider, expected: &[u32]) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            app.drain_background_updates();
            let requests = service.requests();
            assert!(expected.starts_with(&requests), "unexpected issue query sequence: {requests:?}");
            if requests == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed out waiting for issue queries");
}

async fn drain_until_first_issue(app: &mut App, repo: &RepoIdentity, expected_id: &str) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            app.drain_background_updates();
            if app.repo_data[repo].read().issue_rows.first().is_some_and(|row| row.id == expected_id) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed out waiting for issue rows");
}

async fn drain_until_issue_count(app: &mut App, repo: &RepoIdentity, expected_count: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            app.drain_background_updates();
            if app.issue_views.get(repo).and_then(|view| view.default.as_ref()).is_some_and(|state| state.items.len() == expected_count) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed out waiting for issue count");
}

// -- CommandQueue --

#[test]
fn scoped_pane_esc_navigates_back() {
    use crate::keymap::Action;

    let mut app = stub_app();
    app.views = crate::app::OpenViews::scoped("convoy/flotilla/alpha".parse().expect("valid address"));
    app.sync_active_view();

    app.open_view("vessel/flotilla/alpha/build".parse().expect("valid address"));
    assert_eq!(app.views.len(), 1, "scoped panes navigate in place");

    app.dispatch_action(Action::Dismiss);
    assert_eq!(app.views.active_address(), Some(&"convoy/flotilla/alpha".parse().expect("valid address")));
    // Scoped sessions never persist an open-view set.
    assert!(app.config.load_open_views().is_none());
}

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
    assert_eq!(model.active_repo, None, "active_repo is synced from the open views, not set at construction");
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
    let page_one_gate = Arc::new(Semaphore::new(0));
    let service = Arc::new(ScriptedIssueProvider::new(vec![
        QueryStep { expected_page: 1, gate: Some(Arc::clone(&page_one_gate)), result: issue_page(1..=50, true) },
        QueryStep { expected_page: 1, gate: Some(Arc::clone(&page_one_gate)), result: issue_page(1..=50, true) },
    ]));
    let (_temp, repo, daemon, mut app) = app_with_issue_provider(service.clone()).await;

    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("repo snapshot");
    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot.clone())));
    drain_until_requests(&mut app, &service, &[1]).await;

    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot)));
    assert_eq!(service.requests(), vec![1], "initial issue fetch should stay in-flight instead of queueing a duplicate page 1");

    page_one_gate.add_permits(1);
    let repo_identity = app.model.active_repo_identity().clone();
    drain_until_first_issue(&mut app, &repo_identity, "1").await;
}

/// Regression test for #786: every production repo carries a `repository_key`
/// (the daemon's `list_repos` always resolves one), and a guard introduced in
/// #759 skipped the default issue fetch for keyed repos — leaving the repo
/// page's issues section permanently empty. The fake-discovery harness leaves
/// the key unset (the temp dir is not a git repo), so mirror the production
/// shape through the repo-added enrichment path before the snapshot arrives.
#[tokio::test]
async fn snapshot_triggers_default_issue_fetch_for_repo_with_repository_key() {
    let service = Arc::new(ScriptedIssueProvider::new(vec![QueryStep { expected_page: 1, gate: None, result: issue_page(1..=1, false) }]));
    let (_temp, repo, daemon, mut app) = app_with_issue_provider(service.clone()).await;

    let repo_identity = app.model.active_repo_identity().clone();
    let mut info = repo_info(app.model.repos[&repo_identity].path.clone(), "repo", RepoLabels::default());
    info.identity = repo_identity.clone();
    info.repository_key = Some(flotilla_protocol::RepositoryKey("repo_test".into()));
    app.handle_repo_added(info);
    assert!(app.model.repos[&repo_identity].repository_key.is_some(), "test setup should mirror production keyed repos");

    let snapshot = daemon.get_state(&RepoSelector::Path(repo)).await.expect("repo snapshot");
    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot)));

    drain_until_requests(&mut app, &service, &[1]).await;
    drain_until_first_issue(&mut app, &repo_identity, "1").await;
}

#[tokio::test]
async fn manual_refresh_replaces_default_issue_page_when_fresh_results_arrive() {
    let refreshed_page_gate = Arc::new(Semaphore::new(0));
    let service = Arc::new(ScriptedIssueProvider::new(vec![
        QueryStep { expected_page: 1, gate: None, result: issue_page(1..=1, false) },
        QueryStep { expected_page: 1, gate: Some(Arc::clone(&refreshed_page_gate)), result: issue_page(2..=2, false) },
    ]));
    let (_temp, repo, daemon, mut app) = app_with_issue_provider(service.clone()).await;

    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("repo snapshot");
    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot)));

    let repo_identity = app.model.active_repo_identity().clone();
    drain_until_first_issue(&mut app, &repo_identity, "1").await;

    let mut daemon_events = daemon.subscribe();
    app.handle_key(key(KeyCode::Char('r')));
    let (command, pending_ctx) = app.proto_commands.take_next().expect("refresh command");
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    executor::dispatch(command, &mut app, pending_ctx, event_tx);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = daemon_events.recv().await.expect("daemon event");
            let refresh_completed = matches!(event, DaemonEvent::RepoRefreshCompleted { .. });
            app.handle_daemon_event(event);
            if refresh_completed {
                break;
            }
        }
    })
    .await
    .expect("refreshed repo data");
    drain_until_requests(&mut app, &service, &[1, 1]).await;

    assert_eq!(service.requests(), vec![1, 1], "manual refresh should refetch the first issue page");
    assert_eq!(app.repo_data[&repo_identity].read().issue_rows[0].id, "1", "old issue rows should remain visible while refreshing");

    refreshed_page_gate.add_permits(1);
    drain_until_first_issue(&mut app, &repo_identity, "2").await;
}

#[tokio::test]
async fn periodic_refresh_completion_replaces_default_issue_page() {
    let service = Arc::new(ScriptedIssueProvider::new(vec![
        QueryStep { expected_page: 1, gate: None, result: issue_page(1..=1, false) },
        QueryStep { expected_page: 1, gate: None, result: issue_page(2..=2, false) },
    ]));
    let (_temp, repo, daemon, mut app) = app_with_issue_provider(service.clone()).await;

    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("repo snapshot");
    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot)));

    let repo_identity = app.model.active_repo_identity().clone();
    drain_until_first_issue(&mut app, &repo_identity, "1").await;
    app.handle_daemon_event(DaemonEvent::RepoRefreshCompleted { repo_identity: repo_identity.clone(), repo: Some(repo) });
    drain_until_first_issue(&mut app, &repo_identity, "2").await;

    assert_eq!(service.requests(), vec![1, 1]);
}

#[tokio::test]
async fn refresh_during_issue_pagination_runs_after_the_page_completes() {
    let page_two_gate = Arc::new(Semaphore::new(0));
    let service = Arc::new(ScriptedIssueProvider::new(vec![
        QueryStep { expected_page: 1, gate: None, result: issue_page(1..=50, true) },
        QueryStep { expected_page: 2, gate: Some(Arc::clone(&page_two_gate)), result: issue_page(51..=60, false) },
        QueryStep { expected_page: 1, gate: None, result: issue_page(101..=101, false) },
    ]));
    let (_temp, repo, daemon, mut app) = app_with_issue_provider(service.clone()).await;

    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("repo snapshot");
    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot)));

    let repo_identity = app.model.active_repo_identity().clone();
    drain_until_issue_count(&mut app, &repo_identity, 50).await;
    let page = app.screen.repo_pages.get_mut(&repo_identity).expect("repo page");
    page.reconcile_if_changed();
    page.table.select_flat_index(44);
    app.handle_key(key(KeyCode::Char('j')));
    drain_until_requests(&mut app, &service, &[1, 2]).await;

    app.handle_daemon_event(DaemonEvent::RepoRefreshCompleted { repo_identity: repo_identity.clone(), repo: Some(repo) });
    assert_eq!(service.requests(), vec![1, 2], "refresh should wait for the in-flight page");

    page_two_gate.add_permits(1);
    drain_until_requests(&mut app, &service, &[1, 2, 1]).await;
    drain_until_first_issue(&mut app, &repo_identity, "101").await;

    assert_eq!(service.requests(), vec![1, 2, 1], "deferred refresh should run after pagination");
}

#[tokio::test]
async fn infinite_scroll_appends_the_next_issue_page_without_duplicates() {
    let service = Arc::new(ScriptedIssueProvider::new(vec![
        QueryStep { expected_page: 1, gate: None, result: issue_page(1..=50, true) },
        QueryStep { expected_page: 2, gate: None, result: issue_page(51..=60, false) },
    ]));
    let (_temp, repo, daemon, mut app) = app_with_issue_provider(service.clone()).await;

    let snapshot = daemon.get_state(&RepoSelector::Path(repo)).await.expect("repo snapshot");
    app.handle_daemon_event(DaemonEvent::RepoSnapshot(Box::new(snapshot)));

    let repo_identity = app.model.active_repo_identity().clone();
    drain_until_issue_count(&mut app, &repo_identity, 50).await;
    let page = app.screen.repo_pages.get_mut(&repo_identity).expect("repo page");
    page.reconcile_if_changed();
    page.table.select_flat_index(44);

    app.handle_key(key(KeyCode::Char('j')));
    drain_until_requests(&mut app, &service, &[1, 2]).await;
    drain_until_issue_count(&mut app, &repo_identity, 60).await;

    let default = app.issue_views.get(&repo_identity).and_then(|view| view.default.as_ref()).expect("default issue view");
    let ids: Vec<&str> = default.items.iter().map(|issue| issue.reference.id.as_str()).collect();
    let unique_ids: std::collections::HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(service.requests(), vec![1, 2], "scrolling near the bottom should request exactly one next page");
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
        pending_fetch: Some(issue_view::PendingIssueFetch::Page(1)),
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
    // Adding a repo should not switch to it, nor open a tab for it (it may
    // arrive asynchronously, e.g. registered by another session or the CLI)
    assert_eq!(app.views.active_index(), 2, "active tab unchanged");
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[0].clone()));
    assert!(app.views.find(&ViewAddress::repo(app.model.repo_order[1].clone())).is_none(), "no tab opened for the new repo");
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
fn handle_repo_added_enriches_an_existing_repo_with_its_query_key() {
    let mut app = stub_app();
    let identity = app.model.repo_order[0].clone();
    let mut info = repo_info(app.model.repos[&identity].path.clone(), "resolved", RepoLabels::default());
    let key = flotilla_protocol::RepositoryKey("repo_resolved".into());
    info.repository_key = Some(key.clone());
    app.subscriptions_dirty = false;

    app.handle_repo_added(info);

    assert_eq!(app.model.repos[&identity].repository_key, Some(key.clone()));
    assert!(app.views.find(&ViewAddress::repo_with_key(identity, key)).is_some());
    assert!(app.subscriptions_dirty);
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
fn handle_repo_removed_last_repo_keeps_app_alive() {
    let mut app = stub_app();
    let identity = app.model.repo_order[0].clone();

    app.handle_repo_removed(&identity);

    // Removing the last repo no longer quits the TUI (ADR 0012): the repo's
    // tab stays open as a dangling error tab instead.
    assert!(!app.should_quit);
    assert!(app.model.repos.is_empty());
    assert!(app.views.find(&ViewAddress::repo(identity)).is_some(), "the removed repo's tab stays open as a dangling tab");
}

#[test]
fn handle_repo_removed_keeps_tab_open_as_dangling() {
    let mut app = stub_app_with_repos(3);
    activate_repo_tab(&mut app, 2);
    let last_identity = app.model.repo_order[2].clone();

    app.handle_repo_removed(&last_identity);

    // No silent tab pruning: the open-view set and active tab are untouched;
    // the repo's data is gone so the tab renders its dangling-repo error page.
    assert_eq!(app.views.active_index(), 4);
    assert_eq!(app.views.len(), 5);
    assert!(app.views.find(&ViewAddress::repo(last_identity.clone())).is_some());
    assert!(!app.model.repos.contains_key(&last_identity));
    assert!(!app.screen.repo_pages.contains_key(&last_identity));
}

#[test]
fn handle_repo_removed_dismisses_modals_open_on_the_active_repo_tab() {
    let mut app = stub_app_with_repos(2);
    activate_repo_tab(&mut app, 0);
    let active_identity = app.model.active_repo.clone().expect("repo tab active");
    let other_identity = app.model.repo_order[1].clone();

    // Stand-in for a modal that reads the active repo's labels
    // unconditionally in render (the delete-confirm panic scenario from
    // review) — any open modal must not outlive the repo it was opened on.
    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));

    // Untracking a DIFFERENT repo leaves the modal alone…
    app.handle_repo_removed(&other_identity);
    assert!(app.has_modal(), "modal survives removal of an inactive repo");

    // …untracking the ACTIVE tab's repo dismisses it.
    app.handle_repo_removed(&active_identity);
    assert!(!app.has_modal(), "modal must not outlive the active tab's repo");
}

#[test]
fn closing_a_dangling_tab_syncs_layout_from_new_active_page() {
    let mut app = stub_app_with_repos(2);
    // Give the two repo pages different layouts.
    let repo0 = app.model.repo_order[0].clone();
    let repo1 = app.model.repo_order[1].clone();
    app.screen.repo_pages.get_mut(&repo0).expect("page 0").layout = RepoViewLayout::Zoom;
    app.screen.repo_pages.get_mut(&repo1).expect("page 1").layout = RepoViewLayout::Below;

    // Active tab is repo-1 (Below layout). Removing the repo leaves the tab
    // dangling; closing the dangling tab moves focus left onto repo-0.
    activate_repo_tab(&mut app, 1);
    app.handle_repo_removed(&repo1);
    assert!(app.close_active_tab());

    assert_eq!(app.model.active_repo, Some(repo0));
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

    app.screen
        .repo_pages
        .get_mut(&repo)
        .unwrap()
        .pending_actions
        .insert(identity.clone(), PendingAction { status: PendingStatus::InFlight { command_id: 42 }, description: "test".into() });
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

    app.screen
        .repo_pages
        .get_mut(&repo)
        .unwrap()
        .pending_actions
        .insert(identity.clone(), PendingAction { status: PendingStatus::InFlight { command_id: 42 }, description: "test".into() });
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

    app.screen
        .repo_pages
        .get_mut(&repo)
        .unwrap()
        .pending_actions
        .insert(identity.clone(), PendingAction { status: PendingStatus::InFlight { command_id: 42 }, description: "test".into() });
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

    // Insert pending action for command 99 (different from finished event)
    app.screen
        .repo_pages
        .get_mut(&repo)
        .unwrap()
        .pending_actions
        .insert(identity.clone(), PendingAction { status: PendingStatus::InFlight { command_id: 99 }, description: "test".into() });
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
        result: CommandValue::CheckoutCreated {
            branch: "feat".into(),
            path: QualifiedPath::host(HostId::new("local-host"), "/tmp/repo/wt-feat"),
        },
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
        result: CommandValue::CheckoutCreated {
            branch: "feat".into(),
            path: QualifiedPath::host(HostId::new("remote-host"), "/remote/wt-feat"),
        },
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

// -- Result set / delta handling --

fn wire_convoy_phase(phase: crate::convoy_model::ConvoyPhase) -> flotilla_protocol::result_set::ConvoyPhase {
    use flotilla_protocol::result_set::ConvoyPhase as Wire;
    match phase {
        crate::convoy_model::ConvoyPhase::Pending => Wire::Pending,
        crate::convoy_model::ConvoyPhase::Active => Wire::Active,
        crate::convoy_model::ConvoyPhase::Completed => Wire::Completed,
        crate::convoy_model::ConvoyPhase::Failed => Wire::Failed,
        crate::convoy_model::ConvoyPhase::Cancelled => Wire::Cancelled,
    }
}

fn wire_work_phase(phase: crate::convoy_model::WorkPhase) -> flotilla_protocol::result_set::WorkPhase {
    use flotilla_protocol::result_set::WorkPhase as Wire;
    match phase {
        crate::convoy_model::WorkPhase::Pending => Wire::Pending,
        crate::convoy_model::WorkPhase::Ready => Wire::Ready,
        crate::convoy_model::WorkPhase::Launching => Wire::Launching,
        crate::convoy_model::WorkPhase::Running => Wire::Running,
        crate::convoy_model::WorkPhase::Complete => Wire::Complete,
        crate::convoy_model::WorkPhase::Failed => Wire::Failed,
        crate::convoy_model::WorkPhase::Cancelled => Wire::Cancelled,
    }
}

fn wire_convoy_row(convoy: crate::convoy_model::ConvoySummary) -> flotilla_protocol::result_set::ConvoyRow {
    use flotilla_protocol::{
        result_set::{ConvoyRow, CrewMemberSummary, VesselRow},
        HostName, ResourceRef,
    };

    let resource = ResourceRef::new("flotilla.work/v1", "Convoy", &convoy.namespace, &convoy.name);
    let vessels = convoy
        .vessels
        .into_iter()
        .map(|vessel| {
            let crew = vessel
                .crew
                .into_iter()
                .map(|process| CrewMemberSummary {
                    role: process.role,
                    command_preview: process.command_preview,
                    requested_stance: None,
                    effective_stance: None,
                })
                .collect();
            VesselRow::builder()
                .resource(resource.subresource(format!("vessels/{}", vessel.name)))
                .name(&vessel.name)
                .phase(wire_work_phase(vessel.phase))
                .crew(crew)
                .maybe_ready_at(vessel.ready_at)
                .maybe_started_at(vessel.started_at)
                .maybe_finished_at(vessel.finished_at)
                .maybe_message(vessel.message)
                .depends_on(vessel.depends_on)
                .host(vessel.host.unwrap_or_else(HostName::local))
                .maybe_attach(vessel.workspace_ref)
                .complete_work(vessel.completion_target.is_some())
                .build()
        })
        .collect();
    ConvoyRow::builder()
        .resource(resource)
        .name(convoy.name)
        .workflow_ref(convoy.workflow_ref)
        .phase(wire_convoy_phase(convoy.phase))
        .initializing(convoy.initializing)
        .maybe_message(convoy.message)
        .maybe_repo(convoy.repo_hint)
        .maybe_project_ref(convoy.project_ref)
        .maybe_started_at(convoy.started_at)
        .maybe_finished_at(convoy.finished_at)
        .maybe_observed_workflow_ref(convoy.observed_workflow_ref)
        .vessels(vessels)
        .build()
}

fn result_set_event(snapshot: impl AsRef<crate::convoy_model::ConvoyFixtureSnapshot>) -> flotilla_protocol::DaemonEvent {
    use flotilla_protocol::result_set::{ResultSet, Rows};

    let snapshot = snapshot.as_ref().clone();
    flotilla_protocol::DaemonEvent::ResultSet(Box::new(ResultSet {
        seq: snapshot.seq,
        rows: Rows::Convoys(snapshot.convoys.into_iter().map(wire_convoy_row).collect()),
        state: Default::default(),
    }))
}

fn result_delta_event(delta: impl AsRef<crate::convoy_model::ConvoyFixtureDelta>) -> flotilla_protocol::DaemonEvent {
    use flotilla_protocol::{
        result_set::{QueryChanges, ResultDelta},
        ResourceRef,
    };

    let delta = delta.as_ref().clone();
    flotilla_protocol::DaemonEvent::ResultDelta(Box::new(ResultDelta {
        seq: delta.seq,
        changes: QueryChanges::Convoys {
            changed: delta.changed.into_iter().map(wire_convoy_row).collect(),
            removed: delta
                .removed
                .into_iter()
                .map(|id| ResourceRef::new("flotilla.work/v1", "Convoy", id.namespace(), id.name()))
                .collect(),
        },
        state: None,
    }))
}

fn independent_row(name: &str, attach: Option<&str>) -> flotilla_protocol::IndependentRow {
    flotilla_protocol::IndependentRow::builder()
        .resource(flotilla_protocol::ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", name))
        .name(name)
        .repo(flotilla_protocol::RepoKey("flotilla-org/flotilla".to_string()))
        .host(HostName::local())
        .maybe_attach(attach.map(ToString::to_string))
        .phase(flotilla_protocol::SessionPhase::Running)
        .build()
}

fn independent_result_set(seq: u64, rows: Vec<flotilla_protocol::IndependentRow>) -> flotilla_protocol::DaemonEvent {
    flotilla_protocol::DaemonEvent::ResultSet(Box::new(flotilla_protocol::ResultSet {
        seq,
        rows: flotilla_protocol::Rows::Independents { scope: None, rows },
        state: Default::default(),
    }))
}

fn scoped_independent_result_set(
    seq: u64,
    scope: flotilla_protocol::QueryScope,
    rows: Vec<flotilla_protocol::IndependentRow>,
) -> flotilla_protocol::DaemonEvent {
    flotilla_protocol::DaemonEvent::ResultSet(Box::new(flotilla_protocol::ResultSet {
        seq,
        rows: flotilla_protocol::Rows::Independents { scope: Some(scope), rows },
        state: Default::default(),
    }))
}

fn independent_delta(
    seq: u64,
    changed: Vec<flotilla_protocol::IndependentRow>,
    removed: Vec<flotilla_protocol::ResourceRef>,
) -> flotilla_protocol::DaemonEvent {
    flotilla_protocol::DaemonEvent::ResultDelta(Box::new(flotilla_protocol::ResultDelta {
        seq,
        changes: flotilla_protocol::QueryChanges::Independents { scope: None, changed, removed },
        state: None,
    }))
}

fn test_convoy(
    namespace: &str,
    name: &str,
    phase: crate::convoy_model::ConvoyPhase,
    initializing: bool,
) -> crate::convoy_model::ConvoySummary {
    crate::convoy_model::ConvoySummary {
        id: crate::convoy_model::ConvoyId::new(namespace, name),
        namespace: namespace.into(),
        name: name.into(),
        workflow_ref: "wf".into(),
        phase,
        message: None,
        repo_hint: None,
        project_ref: None,
        vessels: vec![],
        started_at: None,
        finished_at: None,
        observed_workflow_ref: None,
        initializing,
    }
}

#[test]
fn app_applies_panel_snapshot() {
    use crate::convoy_model::{ConvoyFixtureSnapshot, ConvoyPhase};

    let mut app = stub_app();
    let convoy = test_convoy("flotilla", "x", ConvoyPhase::Active, false);

    app.handle_daemon_event(result_set_event(Box::new(ConvoyFixtureSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![convoy],
    })));

    assert_eq!(app.convoys("flotilla").len(), 1);
    assert_eq!(app.convoys("flotilla")[0].name, "x");
}

#[test]
fn empty_panel_snapshot_clears_convoys() {
    use crate::convoy_model::{ConvoyFixtureSnapshot, ConvoyPhase};

    let mut app = stub_app();
    app.handle_daemon_event(result_set_event(Box::new(ConvoyFixtureSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![test_convoy("flotilla", "x", ConvoyPhase::Active, false)],
    })));
    assert_eq!(app.convoys("flotilla").len(), 1);

    app.handle_daemon_event(result_set_event(Box::new(ConvoyFixtureSnapshot { seq: 2, namespace: "flotilla".into(), convoys: vec![] })));

    assert!(app.convoys("flotilla").is_empty());
}

#[test]
fn app_applies_panel_delta() {
    use crate::convoy_model::{ConvoyFixtureDelta, ConvoyFixtureSnapshot, ConvoyPhase};

    let mut app = stub_app();
    let convoy = test_convoy("flotilla", "x", ConvoyPhase::Pending, true);

    app.handle_daemon_event(result_set_event(Box::new(ConvoyFixtureSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![convoy.clone()],
    })));

    // Update phase via delta
    let mut modified = convoy.clone();
    modified.phase = ConvoyPhase::Active;
    modified.initializing = false;
    app.handle_daemon_event(result_delta_event(Box::new(ConvoyFixtureDelta {
        seq: 2,
        namespace: "flotilla".into(),
        changed: vec![modified],
        removed: vec![],
    })));
    assert_eq!(app.namespaces["flotilla"].convoys.values().next().expect("convoy").phase, ConvoyPhase::Active);

    // Remove via delta
    app.handle_daemon_event(result_delta_event(Box::new(ConvoyFixtureDelta {
        seq: 3,
        namespace: "flotilla".into(),
        changed: vec![],
        removed: vec![convoy.id.clone()],
    })));
    assert!(app.namespaces["flotilla"].convoys.is_empty());
}

#[test]
fn app_applies_independent_sets_and_removal_deltas_without_disturbing_convoys() {
    let mut app = stub_app();
    let convoy = test_convoy("flotilla", "x", crate::convoy_model::ConvoyPhase::Active, false);
    app.handle_daemon_event(result_set_event(Box::new(crate::convoy_model::ConvoyFixtureSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![convoy],
    })));
    let independent = independent_row("terminal-scratch", Some("terminal-scratch"));
    let reference = independent.resource.clone();

    app.handle_daemon_event(independent_result_set(4, vec![independent]));

    let query = flotilla_protocol::QueryId::Independents { scope: None };
    assert_eq!(app.query_tables.independents[&query].rows.len(), 1);
    assert_eq!(app.namespaces["flotilla"].convoys.len(), 1, "query snapshots replace only their own family");
    app.handle_daemon_event(independent_delta(5, vec![], vec![reference]));
    assert!(app.query_tables.independents[&query].rows.is_empty());
    assert_eq!(app.namespaces["flotilla"].convoys.len(), 1);
}

#[test]
fn app_keeps_fleet_and_project_independent_results_separate() {
    let mut app = stub_app();
    let scope = flotilla_protocol::QueryScope::new("flotilla", "roadmap");
    let fleet_query = flotilla_protocol::QueryId::Independents { scope: None };
    let project_query = flotilla_protocol::QueryId::Independents { scope: Some(scope.clone()) };

    app.handle_daemon_event(independent_result_set(4, vec![independent_row("fleet-only", None)]));
    app.handle_daemon_event(scoped_independent_result_set(2, scope, vec![independent_row("governor", Some("governor"))]));

    assert_eq!(app.query_tables.independents[&fleet_query].rows[0].name, "fleet-only");
    assert_eq!(app.query_tables.independents[&project_query].rows[0].name, "governor");
}

#[test]
fn app_applies_materialized_issue_sets_and_deltas_to_the_typed_query_cache() {
    let mut app = stub_app();
    let scope = flotilla_protocol::QueryScope::new("flotilla", "roadmap");
    let query = flotilla_protocol::QueryId::Issues { scope: scope.clone(), search: None };
    let source = IssueSource { service: "https://issues.example".into(), scope: "widgets/api".into() };
    let mut first = TestIssue::new("First materialized issue").id("LINEAR-1").build();
    first.reference.source = source.clone();
    let first_ref = first.reference.clone();

    app.handle_daemon_event(DaemonEvent::ResultSet(Box::new(flotilla_protocol::ResultSet {
        seq: 1,
        rows: flotilla_protocol::Rows::Issues {
            scope: scope.clone(),
            search: None,
            rows: vec![flotilla_protocol::IssueRow { reference: first_ref.clone(), issue: first }],
        },
        state: flotilla_protocol::ResultSetState {
            demand: Some(flotilla_protocol::DemandBackedMetadata {
                as_of: "2026-07-15T12:00:00Z".parse().expect("timestamp"),
                has_more: true,
            }),
            conditions: vec![],
        },
    })));
    assert_eq!(app.query_tables.issues[&query].rows[0].issue.reference.id, "LINEAR-1");

    let mut second = TestIssue::new("Second materialized issue").id("LINEAR-2").build();
    second.reference.source = source;
    let second_ref = second.reference.clone();
    app.handle_daemon_event(DaemonEvent::ResultDelta(Box::new(flotilla_protocol::ResultDelta {
        seq: 2,
        changes: flotilla_protocol::QueryChanges::Issues {
            scope,
            search: None,
            changed: vec![flotilla_protocol::IssueRow { reference: second_ref, issue: second }],
            removed: vec![first_ref],
        },
        state: Some(flotilla_protocol::ResultSetState {
            demand: Some(flotilla_protocol::DemandBackedMetadata {
                as_of: "2026-07-15T12:01:00Z".parse().expect("timestamp"),
                has_more: false,
            }),
            conditions: vec![],
        }),
    })));

    assert_eq!(app.query_tables.issues[&query].rows.len(), 1);
    assert_eq!(app.query_tables.issues[&query].rows[0].issue.reference.id, "LINEAR-2");
}

#[test]
fn app_applies_checkout_sets_and_removal_deltas_to_the_typed_query_cache() {
    let mut app = stub_app();
    let query = flotilla_protocol::QueryId::Checkouts { scope: None };
    let row = flotilla_protocol::CheckoutRow::builder()
        .resource(flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", "widgets"))
        .repo(flotilla_protocol::RepositoryKey("repo_widgets".into()))
        .path("/work/widgets")
        .branch("main")
        .host(HostName::new("kiwi"))
        .authority(flotilla_protocol::LifecycleAuthority::Observed)
        .build();
    app.handle_daemon_event(DaemonEvent::ResultSet(Box::new(flotilla_protocol::ResultSet {
        seq: 1,
        rows: flotilla_protocol::Rows::Checkouts { scope: None, rows: vec![row.clone()] },
        state: Default::default(),
    })));
    assert_eq!(app.query_tables.checkouts[&query].rows, vec![row.clone()]);

    app.handle_daemon_event(DaemonEvent::ResultDelta(Box::new(flotilla_protocol::ResultDelta {
        seq: 2,
        changes: flotilla_protocol::QueryChanges::Checkouts { scope: None, changed: vec![], removed: vec![row.resource] },
        state: None,
    })));
    assert!(app.query_tables.checkouts[&query].rows.is_empty());
}

#[tokio::test]
async fn materialized_issue_scroll_requests_the_next_demand_backed_page() {
    let mut app = stub_app();
    let scope = flotilla_protocol::QueryScope::new("flotilla", "roadmap");
    let query = flotilla_protocol::QueryId::Issues { scope: scope.clone(), search: None };
    app.open_view("issues?project=flotilla%2Froadmap".parse().expect("address"));
    let source = IssueSource { service: "https://issues.example".into(), scope: "widgets/api".into() };
    let rows = (1..=50)
        .map(|id| {
            let mut issue = TestIssue::new(&format!("Materialized issue {id}")).id(id.to_string()).build();
            issue.reference.source = source.clone();
            flotilla_protocol::IssueRow { reference: issue.reference.clone(), issue }
        })
        .collect();

    app.handle_daemon_event(DaemonEvent::ResultSet(Box::new(flotilla_protocol::ResultSet {
        seq: 1,
        rows: flotilla_protocol::Rows::Issues { scope, search: None, rows },
        state: flotilla_protocol::ResultSetState {
            demand: Some(flotilla_protocol::DemandBackedMetadata {
                as_of: "2026-07-15T12:00:00Z".parse().expect("timestamp"),
                has_more: true,
            }),
            conditions: vec![],
        },
    })));

    let rows = crate::app::table_rows(&app.namespaces, &app.query_tables, None);
    let view = crate::table_view::project(app.views.active_address().expect("active address"), &rows).expect("table");
    app.views.active_table_state_mut().reconcile(&view);
    app.views.active_table_state_mut().select_index(&view, 44);
    app.handle_key(key(KeyCode::Char('j')));

    assert!(app.pending_fetch_more.contains(&query));
}

#[test]
fn source_search_replaces_the_issue_subscription_without_changing_the_persisted_view() {
    let mut app = stub_app();
    app.open_view("issues?project=flotilla%2Froadmap".parse().expect("address"));
    let base = flotilla_protocol::QueryId::Issues { scope: flotilla_protocol::QueryScope::new("flotilla", "roadmap"), search: None };
    assert!(app.query_cursors().iter().any(|cursor| cursor.query == base));

    app.process_app_actions(vec![crate::widgets::AppAction::SetSourceSearch(Some("widget".into()))]);
    let search = flotilla_protocol::QueryId::Issues {
        scope: flotilla_protocol::QueryScope::new("flotilla", "roadmap"),
        search: Some("widget".into()),
    };
    let queries = app.query_cursors().into_iter().map(|cursor| cursor.query).collect::<Vec<_>>();
    assert!(queries.contains(&search));
    assert!(!queries.contains(&base), "source search replaces the base window while active");
    assert_eq!(app.views.to_entries()[app.views.active_index()].address, "issues?project=flotilla%2Froadmap");

    app.process_app_actions(vec![crate::widgets::AppAction::SetSourceSearch(None)]);
    assert!(app.query_cursors().iter().any(|cursor| cursor.query == base));
}

#[test]
fn escape_restores_the_base_issue_window_even_before_search_results_arrive() {
    let mut app = stub_app();
    app.open_view("issues?project=flotilla%2Froadmap".parse().expect("address"));
    app.process_app_actions(vec![crate::widgets::AppAction::SetSourceSearch(Some("widget".into()))]);

    app.handle_key(key(KeyCode::Esc));

    assert_eq!(app.views.active_table_state().source_search, None);
    assert!(app.query_cursors().iter().any(|cursor| cursor.query
        == flotilla_protocol::QueryId::Issues { scope: flotilla_protocol::QueryScope::new("flotilla", "roadmap"), search: None }));
}

// -- Convoys tab rendering --

#[test]
fn screen_renders_convoys_page_on_convoys_tab() {
    use ratatui::{backend::TestBackend, Terminal};

    use crate::{
        convoy_model::{ConvoyFixtureSnapshot, ConvoyPhase},
        widgets::InteractiveWidget as _,
    };

    let mut app = stub_app();

    // Feed a namespace snapshot with one convoy named "demo".
    app.handle_daemon_event(result_set_event(Box::new(ConvoyFixtureSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![test_convoy("flotilla", "demo", ConvoyPhase::Active, false)],
    })));

    // Switch to the Convoys tab.
    app.switch_tab(1);

    // Render into a test terminal.
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");

    terminal
        .draw(|f| {
            let area = f.area();
            let mut ctx = crate::widgets::RenderContext {
                model: &app.model,
                views: &mut app.views,
                ui: &mut app.ui,
                theme: &app.theme,
                keymap: &app.keymap,
                in_flight: &app.in_flight,
                namespaces: &app.namespaces,
                query_tables: &app.query_tables,
            };
            app.screen.render(f, area, &mut ctx);
        })
        .expect("draw");

    let rendered: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
    assert!(rendered.contains("Convoys"), "expected 'Convoys' title, got: {rendered}");
    assert!(rendered.contains("demo"), "expected convoy name 'demo' in rendered output, got: {rendered}");
}

// -- Curated table navigation and actions (#733) --

fn make_convoy_fixture_snapshot(names: &[&str]) -> crate::convoy_model::ConvoyFixtureSnapshot {
    crate::convoy_model::ConvoyFixtureSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: names.iter().map(|name| test_convoy("flotilla", name, crate::convoy_model::ConvoyPhase::Active, false)).collect(),
    }
}

fn convoy_with_vessels(name: &str, vessel_names: &[&str]) -> crate::convoy_model::ConvoySummary {
    let mut convoy = test_convoy("flotilla", name, crate::convoy_model::ConvoyPhase::Active, false);
    convoy.vessels = vessel_names
        .iter()
        .map(|vessel_name| crate::convoy_model::VesselSummary {
            name: (*vessel_name).into(),
            depends_on: vec![],
            phase: crate::convoy_model::WorkPhase::Running,
            crew: vec![],
            host: Some(HostName::local()),
            checkout: None,
            workspace_ref: Some(format!("ws://{vessel_name}")),
            completion_target: Some(crate::convoy_model::WorkCompletionTarget {
                convoy: name.to_string(),
                vessel: (*vessel_name).to_string(),
                host: HostName::local(),
            }),
            ready_at: None,
            started_at: None,
            finished_at: None,
            message: None,
        })
        .collect();
    convoy
}

fn snapshot_with(convoys: Vec<crate::convoy_model::ConvoySummary>) -> crate::convoy_model::ConvoyFixtureSnapshot {
    crate::convoy_model::ConvoyFixtureSnapshot { seq: 1, namespace: "flotilla".into(), convoys }
}

fn selected_table_name(app: &App) -> Option<String> {
    let address = app.views.active_address()?;
    let name_column = match address {
        ViewAddress::Convoys { .. } | ViewAddress::Independents { .. } => 0,
        ViewAddress::Project { .. } => 1,
        ViewAddress::Convoy { .. } | ViewAddress::Vessel { .. } => 1,
        ViewAddress::Issues { .. } | ViewAddress::Checkouts { .. } => 0,
        ViewAddress::Overview | ViewAddress::Repo { .. } => return None,
    };
    let rows = crate::app::table_rows(&app.namespaces, &app.query_tables, app.views.active_table_state().source_search.as_deref());
    let view = crate::table_view::project(address, &rows).ok()?;
    app.views.active_table_state().selected_row(&view).map(|row| row.cells[name_column].text.clone())
}

#[test]
fn keyboard_uses_one_cursor_and_drills_with_tab_local_back_history() {
    let mut app = stub_app();
    app.handle_daemon_event(result_set_event(Box::new(make_convoy_fixture_snapshot(&["alpha", "bravo"]))));
    app.switch_tab(1);

    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(selected_table_name(&app).as_deref(), Some("bravo"));

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.views.active_address(), Some(&"convoy/flotilla/bravo".parse().expect("valid address")));
    assert_eq!(app.views.len(), 3, "drill mutates the tab rather than opening another one");

    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.views.active_address(), Some(&"convoys/flotilla".parse().expect("valid address")));
    assert_eq!(selected_table_name(&app).as_deref(), Some("bravo"), "back restores the prior cursor");
}

#[test]
fn describe_opens_for_the_selected_table_row() {
    let mut app = stub_app();
    app.handle_daemon_event(result_set_event(Box::new(make_convoy_fixture_snapshot(&["alpha"]))));
    app.switch_tab(1);

    app.handle_key(key(KeyCode::Char('y')));

    assert!(app.screen.modal_stack.last().is_some_and(|widget| widget.as_any().is::<crate::widgets::describe::DescribeWidget>()));
}

#[test]
fn action_menu_reports_when_the_selected_table_row_has_no_actions() {
    let mut app = stub_app();
    app.handle_daemon_event(result_set_event(Box::new(make_convoy_fixture_snapshot(&["alpha"]))));
    app.switch_tab(1);

    app.handle_key(key(KeyCode::Char('.')));

    assert_eq!(app.model.status_message.as_deref(), Some("No actions available for the selected row"));
}

#[test]
fn generated_vessel_actions_execute_attach_and_force_complete() {
    let mut app = stub_app();
    let repo_identity = app.model.repo_order[0].clone();
    app.handle_daemon_event(result_set_event(Box::new(snapshot_with(vec![convoy_with_vessels("alpha", &["implement"])]))));
    app.switch_tab(1);
    app.handle_key(key(KeyCode::Enter));

    app.handle_key(key(KeyCode::Char('.')));
    app.handle_key(key(KeyCode::Enter));
    let attach = app.proto_commands.take_next().expect("attach command").0;
    assert!(matches!(attach.action, flotilla_protocol::CommandAction::SelectWorkspace { ref ws_ref } if ws_ref == "ws://implement"));
    assert_eq!(attach.context_repo, Some(RepoSelector::Identity(repo_identity)));

    app.handle_key(key(KeyCode::Char('.')));
    app.handle_key(key(KeyCode::Char('x')));
    let complete = app.proto_commands.take_next().expect("force-complete command").0;
    assert!(matches!(
        complete.action,
        flotilla_protocol::CommandAction::ConvoyWorkForceComplete { ref convoy, ref work, message: None }
            if convoy == "alpha" && work == "implement"
    ));
    assert_eq!(complete.context_repo, None, "force-complete remains a context-free command");
}

#[test]
fn independent_view_subscribes_and_generates_a_pane_attach_query() {
    let mut app = stub_app();
    app.open_view(ViewAddress::Independents { scope: None });
    app.handle_daemon_event(independent_result_set(7, vec![independent_row("terminal-scratch", Some("terminal-scratch"))]));

    assert!(app
        .query_cursors()
        .iter()
        .any(|cursor| cursor.query == (flotilla_protocol::QueryId::Independents { scope: None }) && cursor.since == Some(7)));

    app.handle_key(key(KeyCode::Char('.')));
    assert_eq!(selected_table_name(&app).as_deref(), Some("terminal-scratch"));
    app.handle_key(key(KeyCode::Enter));
    let command = app.proto_commands.take_next().expect("attach query").0;
    assert!(matches!(
        command.action,
        CommandAction::AttachTransient { ref reference, host: Some(ref host) }
            if reference == "terminal-scratch" && host == &HostName::local()
    ));
}

#[test]
fn generated_vessel_actions_route_to_the_vessels_host() {
    let mut app = stub_app();
    let repo_identity = app.model.repo_order[0].clone();
    insert_peer_host(&mut app.model, "feta", PeerStatus::Connected);
    let mut convoy = convoy_with_vessels("alpha", &["implement"]);
    convoy.vessels[0].host = Some(HostName::new("feta"));
    convoy.vessels[0].completion_target.as_mut().expect("completion capability").host = HostName::new("feta");
    app.handle_daemon_event(result_set_event(Box::new(snapshot_with(vec![convoy]))));
    app.switch_tab(1);
    app.handle_key(key(KeyCode::Enter));

    app.handle_key(key(KeyCode::Char('.')));
    app.handle_key(key(KeyCode::Enter));

    let command = app.proto_commands.take_next().expect("remote attach command").0;
    assert_eq!(command.node_id, Some(NodeId::new("node-feta-peer")));
    assert_eq!(command.context_repo, Some(RepoSelector::Identity(repo_identity)));
}

#[test]
fn generated_attach_uses_the_convoys_repo_hint() {
    let mut app = stub_app_with_repos(2);
    let expected_repo = app.model.repo_order[1].clone();
    let mut convoy = convoy_with_vessels("alpha", &["implement"]);
    convoy.repo_hint = Some(flotilla_protocol::RepoKey(expected_repo.path.clone()));
    app.handle_daemon_event(result_set_event(Box::new(snapshot_with(vec![convoy]))));
    app.switch_tab(1);
    app.handle_key(key(KeyCode::Enter));

    app.handle_key(key(KeyCode::Char('.')));
    app.handle_key(key(KeyCode::Enter));

    let command = app.proto_commands.take_next().expect("attach command").0;
    assert_eq!(command.context_repo, Some(RepoSelector::Identity(expected_repo)));
}

#[test]
fn table_action_repo_hint_matches_remote_repo_identity() {
    let identity = RepoIdentity { authority: "github.com".into(), path: "flotilla-org/flotilla".into() };

    assert!(super::key_handlers::repo_identity_matches_hint(
        &identity,
        &flotilla_protocol::RepoKey("github-com-flotilla-org-flotilla".into())
    ));
}

#[test]
fn mouse_click_selects_and_double_click_drills() {
    let mut app = stub_app();
    app.handle_daemon_event(result_set_event(Box::new(make_convoy_fixture_snapshot(&["alpha", "bravo"]))));
    app.switch_tab(1);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let mut ctx = crate::widgets::RenderContext {
                model: &app.model,
                views: &mut app.views,
                ui: &mut app.ui,
                theme: &app.theme,
                keymap: &app.keymap,
                in_flight: &app.in_flight,
                namespaces: &app.namespaces,
                query_tables: &app.query_tables,
            };
            app.screen.render(frame, frame.area(), &mut ctx);
        })
        .expect("draw");
    app.handle_mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 3, row: 0, modifiers: KeyModifiers::NONE });
    assert_eq!(selected_table_name(&app).as_deref(), Some("alpha"), "wheel events outside the table do not move its cursor");

    let click = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 3, row: 5, modifiers: KeyModifiers::NONE };

    app.handle_mouse(click);
    assert_eq!(selected_table_name(&app).as_deref(), Some("bravo"));
    app.handle_mouse(click);
    assert_eq!(app.views.active_address(), Some(&"convoy/flotilla/bravo".parse().expect("valid address")));
}

#[test]
fn table_page_keeps_shell_palette_tab_and_quit_bindings() {
    let mut app = stub_app_with_repos(1);
    app.switch_tab(1);
    app.handle_key(key(KeyCode::Char('/')));
    assert!(app.screen.has_modal());
    app.dismiss_modals();

    app.handle_key(key(KeyCode::Char(']')));
    assert_eq!(app.views.active_index(), 2);
    app.switch_tab(1);
    app.handle_key(key(KeyCode::Char('q')));
    assert!(app.should_quit);
}

#[test]
fn escape_clears_a_table_find_before_navigating_back() {
    let mut app = stub_app_with_repos(1);
    app.switch_tab(1);
    let address = app.views.active_address().cloned().expect("active table view");
    app.process_app_actions(vec![crate::widgets::AppAction::SetTableFilter("stalled".into())]);

    app.handle_key(key(KeyCode::Esc));

    assert!(app.views.active_table_state().filter.is_empty());
    assert_eq!(app.views.active_address(), Some(&address), "clearing Find does not navigate the View");
}

#[test]
fn table_refresh_requests_a_fresh_named_query_snapshot() {
    let mut app = stub_app();
    app.switch_tab(1);
    app.query_seqs.insert(flotilla_protocol::QueryId::Convoys, 42);
    app.subscriptions_dirty = false;

    app.handle_key(key(KeyCode::Char('r')));

    assert!(!app.query_seqs.contains_key(&flotilla_protocol::QueryId::Convoys));
    assert!(app.subscriptions_dirty);
    assert!(app.proto_commands.take_next().is_none(), "table refresh resubscribes instead of dispatching a repo command");
}
