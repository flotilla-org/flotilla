use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    attachable::{shared_in_memory_attachable_store, AttachableSet, AttachableSetId, ProviderBinding, TerminalPurpose},
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    path_context::ExecutionEnvironmentPath,
    providers::{
        ai_utility::AiUtility,
        change_request::ChangeRequestTracker,
        coding_agent::CloudAgentService,
        discovery::{
            test_support::{
                fake_discovery, fake_discovery_with_provider_set, fake_discovery_with_providers, fake_vcs_discovery, git_process_discovery,
                init_git_repo_with_remote, FakeCheckoutManager, FakeCheckoutManagerFactory, FakeDiscoveryProviders, FakeIssueTracker,
                FakeTerminalPool, FakeVcsFactory, FakeVcsState, FakeWorkspaceManager,
            },
            DiscoveryRuntime, EnvironmentAssertion, EnvironmentBag, Factory, HostPlatform, ProviderCategory, ProviderDescriptor,
            RepoDetector, UnmetRequirement,
        },
        types::{ChangeRequest, CloudAgentSession, RepoCriteria, SessionStatus, Workspace},
    },
};
use flotilla_protocol::{
    AssociationKey, Change, Checkout, CheckoutSelector, CheckoutTarget, Command, CommandAction, CommandValue, CorrelationKey, DaemonEvent,
    HostEnvironment, HostName, HostPath, HostProviderStatus, HostSummary, Issue, PeerConnectionState, ProviderData, RepoIdentity,
    RepoSelector, StreamKey, SystemInfo, ToolInventory, TopologyRoute, WorkItemKind,
};
use tokio::sync::Notify;

struct FixedRemoteHostDetector {
    owner: &'static str,
    repo: &'static str,
}

#[async_trait]
impl RepoDetector for FixedRemoteHostDetector {
    async fn detect(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: &dyn flotilla_core::providers::CommandRunner,
        _env: &dyn flotilla_core::providers::discovery::EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        vec![EnvironmentAssertion::remote_host(HostPlatform::GitHub, self.owner, self.repo, "origin")]
    }
}

struct SlowCloudAgent {
    archive_started: Notify,
    archive_release: Notify,
}

impl SlowCloudAgent {
    fn new() -> Self {
        Self { archive_started: Notify::new(), archive_release: Notify::new() }
    }

    async fn wait_for_archive_start(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.archive_started.notified()).await.expect("archive should start");
    }

    fn release_archive(&self) {
        self.archive_release.notify_waiters();
    }
}

#[async_trait]
impl CloudAgentService for SlowCloudAgent {
    async fn list_sessions(&self, _: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
        Ok(vec![("sess-1".into(), CloudAgentSession {
            title: "Slow Session".into(),
            status: SessionStatus::Running,
            model: None,
            updated_at: None,
            correlation_keys: vec![CorrelationKey::SessionRef("slow-agent".into(), "sess-1".into())],
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        })])
    }

    async fn archive_session(&self, _: &str) -> Result<(), String> {
        self.archive_started.notify_waiters();
        self.archive_release.notified().await;
        Ok(())
    }

    async fn attach_command(&self, _: &str) -> Result<String, String> {
        Ok("attach slow-session".into())
    }
}

struct SlowCloudAgentFactory {
    agent: Arc<SlowCloudAgent>,
}

#[async_trait]
impl Factory for SlowCloudAgentFactory {
    type Output = dyn CloudAgentService;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::CloudAgent, "slow-agent", "Slow Agent", "AG", "Sessions", "session")
    }

    async fn probe(
        &self,
        _: &EnvironmentBag,
        _: &ConfigStore,
        _: &ExecutionEnvironmentPath,
        _: Arc<dyn flotilla_core::providers::CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.agent) as Arc<dyn CloudAgentService>)
    }
}

fn slow_cloud_agent_discovery(agent: Arc<SlowCloudAgent>) -> DiscoveryRuntime {
    let mut runtime = fake_discovery(false);
    runtime.factories.cloud_agents.push(Box::new(SlowCloudAgentFactory { agent }));
    runtime
}

struct SlowAiUtility {
    generation_started: Notify,
    generation_release: Notify,
}

impl SlowAiUtility {
    fn new() -> Self {
        Self { generation_started: Notify::new(), generation_release: Notify::new() }
    }

    async fn wait_for_generation_start(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.generation_started.notified()).await.expect("generation should start");
    }

    fn release_generation(&self) {
        self.generation_release.notify_waiters();
    }
}

#[async_trait]
impl AiUtility for SlowAiUtility {
    async fn generate_branch_name(&self, _: &str) -> Result<String, String> {
        self.generation_started.notify_waiters();
        self.generation_release.notified().await;
        Ok("feat/slow-branch".into())
    }
}

struct SlowAiUtilityFactory {
    utility: Arc<SlowAiUtility>,
}

#[async_trait]
impl Factory for SlowAiUtilityFactory {
    type Output = dyn AiUtility;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::AiUtility, "slow-ai")
    }

    async fn probe(
        &self,
        _: &EnvironmentBag,
        _: &ConfigStore,
        _: &ExecutionEnvironmentPath,
        _: Arc<dyn flotilla_core::providers::CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        Ok(Arc::clone(&self.utility) as Arc<dyn AiUtility>)
    }
}

fn slow_ai_discovery(utility: Arc<SlowAiUtility>) -> DiscoveryRuntime {
    let mut runtime = fake_discovery(false);
    runtime.factories.ai_utilities.push(Box::new(SlowAiUtilityFactory { utility }));
    runtime
}

fn sample_remote_host_summary(name: &str) -> HostSummary {
    HostSummary {
        host_name: HostName::new(name),
        system: SystemInfo {
            home_dir: Some(PathBuf::from(format!("/home/{name}"))),
            os: Some("linux".into()),
            arch: Some("aarch64".into()),
            cpu_count: Some(4),
            memory_total_mb: Some(8192),
            environment: HostEnvironment::Container,
        },
        inventory: ToolInventory::default(),
        providers: vec![HostProviderStatus { category: "vcs".into(), name: "Git".into(), healthy: true }],
        environments: vec![],
    }
}

fn definitely_remote_host() -> HostName {
    if HostName::local().to_string() == "test-remote-host" {
        HostName::new("test-remote-host-alt")
    } else {
        HostName::new("test-remote-host")
    }
}

fn test_repo_identity() -> RepoIdentity {
    RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
}

fn local_bare_remote_discovery() -> DiscoveryRuntime {
    let mut runtime = git_process_discovery(false);
    runtime.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));
    runtime
}

struct FailingChangeRequestTracker;

#[async_trait]
impl ChangeRequestTracker for FailingChangeRequestTracker {
    async fn list_change_requests(&self, _: &Path, _: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        Err("change request listing failed".into())
    }

    async fn get_change_request(&self, _: &Path, id: &str) -> Result<(String, ChangeRequest), String> {
        Err(format!("change request {id} not found"))
    }

    async fn open_in_browser(&self, _: &Path, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn close_change_request(&self, _: &Path, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn list_merged_branch_names(&self, _: &Path, _: usize) -> Result<Vec<String>, String> {
        Err("merged branch listing failed".into())
    }
}

async fn daemon_for_cwd() -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::local()).await;
    (temp, repo, daemon)
}

async fn daemon_for_plain_dir() -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::local()).await;
    (temp, repo, daemon)
}

async fn daemon_for_plain_dir_with_discovery(discovery: DiscoveryRuntime) -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;
    (temp, repo, daemon)
}

fn init_bare_git_remote(path: &Path) {
    let status = std::process::Command::new("git")
        .args(["init", "--bare", "--initial-branch=main"])
        .arg(path)
        .status()
        .expect("run git init --bare");
    assert!(status.success(), "git init --bare should succeed");
}

fn init_git_repo_with_local_bare_remote(path: &Path, remote_path: &Path) -> RepoIdentity {
    init_bare_git_remote(remote_path);
    init_git_repo_with_remote(path, remote_path.to_str().expect("remote path utf8"))
}

async fn daemon_for_fake_repo() -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>, RepoIdentity) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let state =
        FakeVcsState::builder(repo.clone()).branch("main", true).remote_branch("main").checkout("main").is_main(true).build().build();

    let mut discovery = fake_vcs_discovery(state);
    discovery.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;
    let identity = daemon.tracked_repo_identity_for_path(&repo).await.expect("identity");
    (temp, repo, daemon, identity)
}

async fn daemon_for_duplicate_fake_repos() -> (tempfile::TempDir, PathBuf, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    std::fs::create_dir_all(&repo_a).expect("create repo-a dir");
    std::fs::create_dir_all(&repo_b).expect("create repo-b dir");

    let state_a = FakeVcsState::builder(repo_a.clone()).branch("main", true).checkout("main").is_main(true).build().build();
    let state_b = FakeVcsState::builder(repo_b.clone()).branch("main", true).checkout("main").is_main(true).build().build();

    let mut discovery = fake_discovery(false);
    discovery.factories.vcs = vec![Box::new(FakeVcsFactory::new(state_a.clone())), Box::new(FakeVcsFactory::new(state_b.clone()))];
    discovery.factories.checkout_managers =
        vec![Box::new(FakeCheckoutManagerFactory::new(state_a)), Box::new(FakeCheckoutManagerFactory::new(state_b))];
    discovery.repo_detectors.push(Box::new(FixedRemoteHostDetector { owner: "owner", repo: "repo" }));

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo_a.clone(), repo_b.clone()], config, discovery, HostName::local()).await;
    (temp, repo_a, repo_b, daemon)
}

#[tokio::test]
async fn list_hosts_includes_local_and_configured_disconnected_peers() {
    let (_temp, _repo, daemon, _identity) = daemon_for_fake_repo().await;

    daemon.set_configured_peer_names(vec![HostName::new("remote")]).await;

    let hosts = daemon.list_hosts_internal().await.expect("list hosts");

    assert!(hosts.hosts.iter().any(|entry| entry.host == HostName::local() && entry.is_local));
    assert!(hosts.hosts.iter().any(|entry| {
        entry.host == HostName::new("remote")
            && entry.configured
            && !entry.has_summary
            && entry.connection_status == PeerConnectionState::Disconnected
    }));
}

#[tokio::test]
async fn get_host_providers_returns_local_summary_and_errors_for_unknown_remote_summary() {
    let (_temp, _repo, daemon, _identity) = daemon_for_fake_repo().await;

    daemon.set_configured_peer_names(vec![HostName::new("remote")]).await;

    let local_host = daemon.host_name().to_string();
    let local = daemon.get_host_providers_internal(&local_host).await.expect("local host providers should resolve");
    assert_eq!(local.host, *daemon.host_name());
    assert_eq!(local.summary.host_name, *daemon.host_name());

    let err = daemon.get_host_providers_internal("remote").await.expect_err("remote host without summary should error");
    assert!(err.contains("summary"), "unexpected error: {err}");
}

#[tokio::test]
async fn list_hosts_counts_remote_repo_overlay_and_get_topology_returns_mirrored_routes() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;

    daemon.set_configured_peer_names(vec![HostName::new("remote")]).await;
    daemon.set_peer_host_summaries(HashMap::from([(HostName::new("remote"), sample_remote_host_summary("remote"))])).await;
    daemon
        .set_topology_routes(vec![TopologyRoute {
            target: HostName::new("remote"),
            next_hop: HostName::new("relay"),
            direct: false,
            connected: true,
            fallbacks: vec![HostName::new("backup-relay")],
        }])
        .await;

    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(HostPath::new(HostName::new("remote"), "/srv/remote/repo"), Checkout {
        branch: "peer-branch".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });
    daemon.send_event(DaemonEvent::PeerStatusChanged { host: HostName::new("remote"), status: PeerConnectionState::Connected });
    daemon.set_peer_providers(&repo, vec![(HostName::new("remote"), peer_data)], 0).await;

    let hosts = daemon.list_hosts_internal().await.expect("list hosts");
    let remote = hosts.hosts.iter().find(|entry| entry.host == HostName::new("remote")).expect("remote host entry");
    assert_eq!(remote.repo_count, 1);
    assert!(remote.work_item_count >= 1, "remote overlay should contribute work items");

    let topology = daemon.get_topology().await.expect("topology");
    assert_eq!(topology.routes.len(), 1);
    assert_eq!(topology.routes[0].target, HostName::new("remote"));
    assert_eq!(topology.routes[0].next_hop, HostName::new("relay"));
}

#[tokio::test]
async fn get_topology_includes_configured_but_disconnected_peers() {
    let (_temp, _repo, daemon, _identity) = daemon_for_fake_repo().await;

    // Configure two peers but only set routes for one
    daemon.set_configured_peer_names(vec![HostName::new("connected"), HostName::new("unreachable")]).await;
    daemon
        .set_topology_routes(vec![TopologyRoute {
            target: HostName::new("connected"),
            next_hop: HostName::new("connected"),
            direct: true,
            connected: true,
            fallbacks: vec![],
        }])
        .await;

    let topology = daemon.get_topology().await.expect("topology");

    // Should have entries for both peers
    assert_eq!(topology.routes.len(), 2, "should include both connected and disconnected peers");

    let connected = topology.routes.iter().find(|r| r.target == HostName::new("connected")).expect("connected peer");
    assert!(connected.connected);
    assert!(connected.direct);

    let unreachable = topology.routes.iter().find(|r| r.target == HostName::new("unreachable")).expect("unreachable peer");
    assert!(!unreachable.connected, "configured-but-never-connected peer should show as disconnected");
    assert!(unreachable.direct, "disconnected peer should show as direct (no relay known)");
    assert!(unreachable.fallbacks.is_empty());
}

async fn recv_event(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>) -> DaemonEvent {
    tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await.expect("timeout waiting for event").expect("recv error")
}

async fn trigger_refresh_and_recv(
    daemon: &Arc<InProcessDaemon>,
    repo: &Path,
    rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
) -> DaemonEvent {
    daemon.refresh(&RepoSelector::Path(repo.to_path_buf())).await.expect("refresh should succeed");
    recv_event(rx).await
}

#[tokio::test]
async fn daemon_broadcasts_snapshots() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    let event = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    match event {
        DaemonEvent::RepoSnapshot(snap) => {
            assert_eq!(snap.repo, repo);
            assert!(snap.seq > 0);
        }
        DaemonEvent::RepoDelta(delta) => {
            assert_eq!(delta.repo, repo);
            assert!(delta.seq > 0);
        }
        other => panic!("expected RepoSnapshot or RepoDelta, got {:?}", other),
    }
}

#[tokio::test]
async fn execute_broadcasts_lifecycle_events() {
    let (_temp, repo, daemon, identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    // Execute a command that goes through the spawned task path.
    // ArchiveSession with a non-existent ID returns immediately with
    // "session not found" — no external API calls, deterministic.
    // We only care about the lifecycle events, not the command result.
    let command = Command {
        host: None,
        environment: None,
        context_repo: Some(RepoSelector::Identity(identity.clone())),
        action: CommandAction::ArchiveSession { session_id: "nonexistent-session".into() },
    };
    let command_id = daemon.execute(command).await.expect("execute should return a command id");

    // Collect CommandStarted and CommandFinished events, skipping any
    // Repo snapshot events that arrive from the background refresh loop.
    let timeout = std::time::Duration::from_secs(10);
    let mut got_started = false;
    let mut got_finished = false;
    let mut started_id = None;
    let mut finished_id = None;

    let result = tokio::time::timeout(timeout, async {
        while !got_started || !got_finished {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { command_id: id, host, repo_identity, repo: ref event_repo, .. }) => {
                    assert_eq!(host, HostName::local(), "CommandStarted host should default to local host");
                    assert_eq!(repo_identity, identity, "CommandStarted repo identity should match executed repo");
                    assert_eq!(event_repo, &repo, "CommandStarted repo should match executed repo");
                    started_id = Some(id);
                    got_started = true;
                }
                Ok(DaemonEvent::CommandFinished { command_id: id, host, repo_identity, repo: ref event_repo, .. }) => {
                    assert_eq!(host, HostName::local(), "CommandFinished host should default to local host");
                    assert_eq!(repo_identity, identity, "CommandFinished repo identity should match executed repo");
                    assert_eq!(event_repo, &repo, "CommandFinished repo should match executed repo");
                    finished_id = Some(id);
                    got_finished = true;
                }
                Ok(_) => {
                    // Skip snapshot and other events
                }
                Err(e) => panic!("unexpected recv error: {:?}", e),
            }
        }
    })
    .await;

    result.expect("timed out waiting for lifecycle events");

    // Both events must carry the same command ID returned by execute()
    assert_eq!(started_id, Some(command_id), "CommandStarted id should match the id returned by execute()");
    assert_eq!(finished_id, Some(command_id), "CommandFinished id should match the id returned by execute()");
}

#[tokio::test]
async fn fetch_checkout_status_accepts_identity_context_repo() {
    let (_temp, _repo, daemon, identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    let command = Command {
        host: None,
        environment: None,
        context_repo: Some(RepoSelector::Identity(identity.clone())),
        action: CommandAction::FetchCheckoutStatus { branch: "main".into(), checkout_path: None, change_request_id: None },
    };

    let command_id = daemon.execute(command).await.expect("status command should resolve via identity context repo");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: finished_id, repo_identity, result, .. }) if finished_id == command_id => {
                    assert_eq!(repo_identity, identity, "finished event should preserve repo identity");
                    break result;
                }
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for checkout status command to finish");

    assert!(matches!(result, CommandValue::CheckoutStatus(_)), "expected checkout status result via identity context repo, got {result:?}");
}

#[tokio::test]
async fn archive_session_can_be_cancelled_while_provider_call_is_in_flight() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let agent = Arc::new(SlowCloudAgent::new());
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, slow_cloud_agent_discovery(Arc::clone(&agent)), HostName::local()).await;
    let mut rx = daemon.subscribe();

    let refresh_event = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;
    match refresh_event {
        DaemonEvent::RepoSnapshot(snap) => assert!(snap.providers.sessions.contains_key("sess-1"), "refresh should expose sess-1"),
        DaemonEvent::RepoDelta(delta) => {
            assert!(delta.work_items.iter().any(|item| item.session_key.as_deref() == Some("sess-1")), "refresh should expose sess-1")
        }
        other => panic!("expected snapshot event, got {other:?}"),
    }

    let command = Command {
        host: None,
        environment: None,
        context_repo: Some(RepoSelector::Path(repo.clone())),
        action: CommandAction::ArchiveSession { session_id: "sess-1".into() },
    };
    let command_id = daemon.execute(command).await.expect("execute should return a command id");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { command_id: id, .. }) if id == command_id => break,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for command start");

    agent.wait_for_archive_start().await;
    daemon.cancel(command_id).await.expect("cancel should succeed while archive is in flight");
    agent.release_archive();

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for command finish");

    assert_eq!(result, CommandValue::Cancelled);
}

#[tokio::test]
async fn generate_branch_name_can_be_cancelled_while_provider_call_is_in_flight() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let utility = Arc::new(SlowAiUtility::new());
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, slow_ai_discovery(Arc::clone(&utility)), HostName::local()).await;
    let mut rx = daemon.subscribe();

    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh should succeed");

    let command = Command {
        host: None,
        environment: None,
        context_repo: Some(RepoSelector::Path(repo.clone())),
        action: CommandAction::GenerateBranchName { issue_keys: vec!["42".into()] },
    };
    let command_id = daemon.execute(command).await.expect("execute should return a command id");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { command_id: id, .. }) if id == command_id => break,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for command start");

    utility.wait_for_generation_start().await;
    daemon.cancel(command_id).await.expect("cancel should succeed while generation is in flight");
    utility.release_generation();

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for command finish");

    assert_eq!(result, CommandValue::Cancelled);
}

#[tokio::test]
async fn replay_since_returns_full_snapshot_for_unknown_seq() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let identity = daemon.tracked_repo_identity_for_path(&repo).await.expect("repo identity should be detected");

    // Wait for at least one broadcast so the daemon has state
    let mut rx = daemon.subscribe();
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Request replay with a seq that won't be in the delta log
    let last_seen = HashMap::from([(StreamKey::Repo { identity }, 999999)]);
    let events = daemon.replay_since(&last_seen).await.expect("replay_since");

    // Should get one RepoSnapshot + at least one HostSnapshot (local host)
    let repo_events: Vec<_> = events.iter().filter(|e| matches!(e, DaemonEvent::RepoSnapshot(_))).collect();
    assert_eq!(repo_events.len(), 1, "should get exactly one repo snapshot");
    match &repo_events[0] {
        DaemonEvent::RepoSnapshot(snap) => {
            assert_eq!(snap.repo, repo);
        }
        other => panic!("expected RepoSnapshot, got {:?}", other),
    }
}

#[tokio::test]
async fn replay_since_returns_full_snapshot_for_new_repo() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;

    // Wait for at least one broadcast
    let mut rx = daemon.subscribe();
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Request replay with empty last_seen (new client)
    let events = daemon.replay_since(&HashMap::new()).await.expect("replay_since");

    // Should get one RepoSnapshot + at least one HostSnapshot (local host)
    let repo_events: Vec<_> = events.iter().filter(|e| matches!(e, DaemonEvent::RepoSnapshot(_))).collect();
    assert_eq!(repo_events.len(), 1, "should get one repo snapshot per tracked repo");
    match &repo_events[0] {
        DaemonEvent::RepoSnapshot(snap) => {
            assert_eq!(snap.repo, repo);
        }
        other => panic!("expected RepoSnapshot, got {:?}", other),
    }
    // Verify local host snapshot is present
    let host_events: Vec<_> = events.iter().filter(|e| matches!(e, DaemonEvent::HostSnapshot(_))).collect();
    assert!(!host_events.is_empty(), "should include at least one HostSnapshot for local host");
}

#[tokio::test]
async fn replay_since_returns_empty_when_up_to_date() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let identity = daemon.tracked_repo_identity_for_path(&repo).await.expect("repo identity should be detected");

    // Wait for the first snapshot to get the current seq
    let mut rx = daemon.subscribe();
    let event = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let current_seq = match event {
        DaemonEvent::RepoSnapshot(snap) => snap.seq,
        DaemonEvent::RepoDelta(delta) => delta.seq,
        other => panic!("expected snapshot event, got {:?}", other),
    };

    // Request replay at current seq — should return no repo events (still get HostSnapshots)
    let last_seen = HashMap::from([(StreamKey::Repo { identity }, current_seq)]);
    let events = daemon.replay_since(&last_seen).await.expect("replay_since");

    let repo_events: Vec<_> = events.iter().filter(|e| matches!(e, DaemonEvent::RepoSnapshot(_) | DaemonEvent::RepoDelta(_))).collect();
    assert!(repo_events.is_empty(), "should have no repo events when up to date");
}

#[tokio::test]
async fn replay_since_returns_no_host_event_when_host_cursor_is_current() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;

    let mut rx = daemon.subscribe();
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    daemon.set_configured_peer_names(vec![HostName::new("alpha")]).await;
    daemon.send_event(DaemonEvent::PeerStatusChanged { host: HostName::new("alpha"), status: PeerConnectionState::Connected });
    daemon.set_peer_host_summaries(HashMap::from([(HostName::new("alpha"), sample_remote_host_summary("alpha"))])).await;

    let events = daemon.replay_since(&HashMap::new()).await.expect("initial host replay");
    let local_host = daemon.host_name().clone();
    let local_seq = events
        .iter()
        .find_map(|event| match event {
            DaemonEvent::HostSnapshot(snap) if snap.host_name == local_host => Some(snap.seq),
            _ => None,
        })
        .expect("initial replay should include local host snapshot");

    let events = daemon
        .replay_since(&HashMap::from([(StreamKey::Host { host_name: local_host.clone() }, local_seq)]))
        .await
        .expect("host replay with current cursor");

    assert!(
        !events.iter().any(|event| matches!(event, DaemonEvent::HostSnapshot(snap) if snap.host_name == local_host)),
        "current host cursor should suppress replay for that host"
    );
}

#[tokio::test]
async fn replay_since_returns_only_stale_host_snapshots() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;

    let mut rx = daemon.subscribe();
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    daemon.set_configured_peer_names(vec![HostName::new("alpha"), HostName::new("beta")]).await;
    daemon.send_event(DaemonEvent::PeerStatusChanged { host: HostName::new("alpha"), status: PeerConnectionState::Connected });
    daemon.send_event(DaemonEvent::PeerStatusChanged { host: HostName::new("beta"), status: PeerConnectionState::Disconnected });
    daemon
        .set_peer_host_summaries(HashMap::from([
            (HostName::new("alpha"), sample_remote_host_summary("alpha")),
            (HostName::new("beta"), sample_remote_host_summary("beta")),
        ]))
        .await;

    let events = daemon.replay_since(&HashMap::new()).await.expect("initial host replay");
    let mut host_seqs = HashMap::new();
    for event in &events {
        if let DaemonEvent::HostSnapshot(snap) = event {
            host_seqs.insert(snap.host_name.clone(), snap.seq);
        }
    }

    let local_host = daemon.host_name().clone();
    let alpha = HostName::new("alpha");
    let beta = HostName::new("beta");
    let last_seen = HashMap::from([
        (StreamKey::Host { host_name: local_host.clone() }, *host_seqs.get(&local_host).expect("local host seq")),
        (StreamKey::Host { host_name: alpha.clone() }, *host_seqs.get(&alpha).expect("alpha seq")),
        (StreamKey::Host { host_name: beta.clone() }, 0),
    ]);

    let events = daemon.replay_since(&last_seen).await.expect("host replay with mixed cursors");
    let replayed_hosts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            DaemonEvent::HostSnapshot(snap) => Some(snap.host_name.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(replayed_hosts, vec![beta], "only stale hosts should replay");
}

#[tokio::test]
async fn replay_since_includes_non_config_backed_known_hosts() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;

    let mut rx = daemon.subscribe();
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let peer_host = HostName::new("inbound-only");
    daemon.send_event(DaemonEvent::PeerStatusChanged { host: peer_host.clone(), status: PeerConnectionState::Connected });
    daemon.set_peer_host_summaries(HashMap::from([(peer_host.clone(), sample_remote_host_summary("inbound-only"))])).await;

    let events = daemon.replay_since(&HashMap::new()).await.expect("host replay");

    assert!(
        events.iter().any(|event| matches!(event, DaemonEvent::HostSnapshot(snap) if snap.host_name == peer_host)),
        "known non-config-backed hosts should still replay"
    );
}

#[tokio::test]
async fn publish_peer_summary_normalizes_host_name() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;

    let mut rx = daemon.subscribe();
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let peer_host = HostName::new("remote-host");
    daemon
        .publish_peer_summary(&peer_host, HostSummary {
            host_name: HostName::new("spoofed-host"),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        })
        .await;

    let replay = daemon.replay_since(&HashMap::new()).await.expect("replay_since");
    let snapshot = replay
        .iter()
        .find_map(|event| match event {
            DaemonEvent::HostSnapshot(snap) if snap.host_name == peer_host => Some(snap),
            _ => None,
        })
        .expect("remote host snapshot");
    assert_eq!(snapshot.host_name, peer_host);
    assert_eq!(snapshot.summary.host_name, peer_host);
}

#[tokio::test]
async fn set_peer_providers_emits_host_snapshot_for_overlay_only_host() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let overlay_host = HostName::new("overlay-live");
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(HostPath::new(overlay_host.clone(), "/srv/overlay/repo"), Checkout {
        branch: "overlay-branch".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    daemon.set_peer_providers(&repo, vec![(overlay_host.clone(), peer_data)], 0).await;

    let host_event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match rx.recv().await.expect("recv") {
                DaemonEvent::HostSnapshot(snap) if snap.host_name == overlay_host => return snap,
                _ => continue,
            }
        }
    })
    .await
    .expect("timeout waiting for overlay host snapshot");
    assert_eq!(host_event.host_name, overlay_host);
}

#[tokio::test]
async fn replay_since_includes_overlay_only_hosts() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let overlay_host = HostName::new("overlay-only");
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(HostPath::new(overlay_host.clone(), "/srv/overlay/repo"), Checkout {
        branch: "overlay-branch".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    daemon.set_peer_providers(&repo, vec![(overlay_host.clone(), peer_data)], 0).await;
    let _ = recv_event(&mut rx).await;

    let events = daemon.replay_since(&HashMap::new()).await.expect("host replay");
    assert!(
        events.iter().any(|event| matches!(event, DaemonEvent::HostSnapshot(snap) if snap.host_name == overlay_host)),
        "hosts known only through remote overlay data should replay"
    );
}

#[tokio::test]
async fn list_hosts_and_replay_drop_stale_non_configured_hosts() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let transient_host = HostName::new("transient");
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(HostPath::new(transient_host.clone(), "/srv/transient/repo"), Checkout {
        branch: "transient-branch".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    daemon.publish_peer_connection_status(&transient_host, PeerConnectionState::Connected).await;
    daemon.set_peer_host_summaries(HashMap::from([(transient_host.clone(), sample_remote_host_summary("transient"))])).await;
    daemon.set_peer_providers(&repo, vec![(transient_host.clone(), peer_data)], 0).await;
    let _ = recv_event(&mut rx).await;

    let hosts = daemon.list_hosts_internal().await.expect("list hosts");
    assert!(hosts.hosts.iter().any(|entry| entry.host == transient_host), "transient host should be visible while backed by state");

    daemon.publish_peer_connection_status(&transient_host, PeerConnectionState::Disconnected).await;
    daemon.set_peer_host_summaries(HashMap::new()).await;
    daemon.set_peer_providers(&repo, vec![], 1).await;
    let removed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match rx.recv().await.expect("recv") {
                DaemonEvent::HostRemoved { host, seq } if host == transient_host => return seq,
                _ => continue,
            }
        }
    })
    .await
    .expect("timeout waiting for host removal");
    assert!(removed >= 1, "host removal should carry a stream seq");

    let hosts = daemon.list_hosts_internal().await.expect("list hosts");
    assert!(
        !hosts.hosts.iter().any(|entry| entry.host == transient_host),
        "stale non-configured host should be pruned once summary, connection, and overlay data are gone"
    );

    let replay = daemon.replay_since(&HashMap::new()).await.expect("replay_since");
    assert!(
        !replay.iter().any(|event| matches!(event, DaemonEvent::HostSnapshot(snap) if snap.host_name == transient_host)),
        "pruned hosts should not keep replaying"
    );
}

#[tokio::test]
async fn clearing_summary_for_visible_host_emits_host_snapshot() {
    let (_temp, _repo, daemon, _identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();
    let peer_host = HostName::new("configured-peer");

    daemon.set_configured_peer_names(vec![peer_host.clone()]).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match rx.recv().await.expect("recv") {
                DaemonEvent::HostSnapshot(snap) if snap.host_name == peer_host => return snap,
                _ => continue,
            }
        }
    })
    .await
    .expect("timeout waiting for configured host snapshot");

    daemon.set_peer_host_summaries(HashMap::from([(peer_host.clone(), sample_remote_host_summary("configured-peer"))])).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match rx.recv().await.expect("recv") {
                DaemonEvent::HostSnapshot(snap) if snap.host_name == peer_host && !snap.summary.providers.is_empty() => return snap,
                _ => continue,
            }
        }
    })
    .await
    .expect("timeout waiting for summary snapshot");

    daemon.set_peer_host_summaries(HashMap::new()).await;
    let cleared = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match rx.recv().await.expect("recv") {
                DaemonEvent::HostSnapshot(snap) if snap.host_name == peer_host => return snap,
                _ => continue,
            }
        }
    })
    .await
    .expect("timeout waiting for cleared summary snapshot");
    assert!(cleared.summary.providers.is_empty(), "cleared summary should fall back to the default empty summary");
}

/// replay_since must include peer provider data, just like get_state and live
/// broadcasts. A late-subscribing or reconnecting client should see the same
/// merged view (local + peer checkouts with correct host attribution) as a
/// client that was connected from the start.
#[tokio::test]
async fn replay_since_includes_peer_checkouts_with_correct_host() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    // Initial refresh
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Use a peer host name that won't collide with the local hostname
    let peer_host = HostName::new("remote-peer-host");
    let peer_checkout_path = HostPath::new(peer_host.clone(), "/srv/remote/repo");
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(peer_checkout_path.clone(), Checkout {
        branch: "peer-feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    daemon.set_peer_providers(&repo, vec![(peer_host.clone(), peer_data)], 0).await;
    let _ = recv_event(&mut rx).await;

    // Trigger refresh so poll_snapshots stores updated state
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Simulate a new client connecting — replay_since with empty last_seen
    let events = daemon.replay_since(&HashMap::new()).await.expect("replay_since");

    let snap = events
        .iter()
        .find_map(|e| match e {
            DaemonEvent::RepoSnapshot(s) if s.repo == repo => Some(s),
            _ => None,
        })
        .expect("should have a RepoSnapshot for our repo");

    // Peer checkout must be present, attributed to its real host (not local)
    assert!(
        snap.providers.checkouts.contains_key(&peer_checkout_path),
        "replay snapshot must include peer checkout under remote-peer-host, got keys: {:?}",
        snap.providers.checkouts.keys().collect::<Vec<_>>()
    );

    // No ghost checkout under local host
    let local_host = HostName::local();
    let ghost = HostPath::new(local_host, PathBuf::from("/srv/remote/repo"));
    assert!(!snap.providers.checkouts.contains_key(&ghost), "replay snapshot must not re-attribute peer checkout to local host");
}

/// Unknown-seq fallback should include peer checkouts with correct host attribution,
/// not just local provider data.
#[tokio::test]
async fn replay_since_unknown_seq_includes_peer_checkouts_with_correct_host() {
    let (_temp, repo, daemon, identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    // Initial refresh so daemon has state
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Add peer providers
    let peer_host = HostName::new("remote-peer-host");
    let peer_checkout_path = HostPath::new(peer_host.clone(), "/srv/remote/repo");
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(peer_checkout_path.clone(), Checkout {
        branch: "peer-feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    daemon.set_peer_providers(&repo, vec![(peer_host.clone(), peer_data)], 0).await;
    let _ = recv_event(&mut rx).await;
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Request replay with a seq that can never appear in the delta log
    let last_seen = HashMap::from([(StreamKey::Repo { identity }, u64::MAX)]);
    let events = daemon.replay_since(&last_seen).await.expect("replay_since");

    let snap = events
        .iter()
        .find_map(|e| match e {
            DaemonEvent::RepoSnapshot(s) if s.repo == repo => Some(s),
            _ => None,
        })
        .expect("unknown-seq fallback should produce a RepoSnapshot");

    // Peer checkout must be present with remote host attribution
    assert!(
        snap.providers.checkouts.contains_key(&peer_checkout_path),
        "unknown-seq snapshot must include peer checkout under remote-peer-host, got keys: {:?}",
        snap.providers.checkouts.keys().collect::<Vec<_>>()
    );

    // No ghost checkout under local host
    let local_host = HostName::local();
    let ghost = HostPath::new(local_host, PathBuf::from("/srv/remote/repo"));
    assert!(!snap.providers.checkouts.contains_key(&ghost), "snapshot must not re-attribute peer checkout to local host");
}

/// Delta replay path should include peer checkout changes in the replayed
/// deltas, and the full snapshot (used for issue metadata in replay_since)
/// should reflect the peer-merged view.
#[tokio::test]
async fn replay_since_delta_replay_includes_peer_data() {
    let (_temp, repo, daemon, identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    // First refresh — establishes seq in delta log
    let event = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;
    let first_seq = match event {
        DaemonEvent::RepoSnapshot(snap) => snap.seq,
        DaemonEvent::RepoDelta(delta) => delta.seq,
        other => panic!("expected snapshot event, got {:?}", other),
    };

    // Add peer providers with a checkout
    let peer_host = HostName::new("delta-peer-host");
    let peer_checkout_path = HostPath::new(peer_host.clone(), "/srv/delta-peer/repo");
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(peer_checkout_path.clone(), Checkout {
        branch: "delta-feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    daemon.set_peer_providers(&repo, vec![(peer_host.clone(), peer_data)], 0).await;
    let _ = recv_event(&mut rx).await;

    // Second refresh — creates a delta entry from first_seq to new seq
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Replay from first_seq — should get delta entries (not full snapshot)
    let last_seen = HashMap::from([(StreamKey::Repo { identity }, first_seq)]);
    let events = daemon.replay_since(&last_seen).await.expect("replay_since");

    // Should get RepoDelta(s), not a full RepoSnapshot
    let deltas: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DaemonEvent::RepoDelta(d) if d.repo == repo => Some(d),
            _ => None,
        })
        .collect();
    assert!(!deltas.is_empty(), "delta replay should produce RepoDelta events");

    // The delta's changes should include the peer checkout being added
    let has_peer_checkout_change = deltas.iter().any(|d| {
        d.changes.iter().any(|c| match c {
            Change::Checkout { key, op: flotilla_protocol::EntryOp::Added(_) } => key.path == Path::new("/srv/delta-peer/repo"),
            _ => false,
        })
    });
    assert!(has_peer_checkout_change, "delta replay should include an Added checkout change for the peer checkout");

    // Verify the full snapshot (built by replay_since via
    // build_repo_snapshot_with_peers) also contains the peer checkout.
    // This confirms the snapshot used for issue metadata on the delta
    // replay path is peer-merged, not local-only.
    let full_events = daemon.replay_since(&HashMap::new()).await.expect("replay_since full");
    let full_snap = full_events
        .iter()
        .find_map(|e| match e {
            DaemonEvent::RepoSnapshot(s) if s.repo == repo => Some(s),
            _ => None,
        })
        .expect("full replay should produce RepoSnapshot");

    assert!(
        full_snap.providers.checkouts.contains_key(&peer_checkout_path),
        "full snapshot must include peer checkout, confirming build_repo_snapshot_with_peers is used on replay"
    );
}

#[tokio::test]
async fn add_and_remove_repo_updates_state_and_emits_events() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("new-repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();

    let add_id = daemon
        .execute(Command { host: None, environment: None, context_repo: None, action: CommandAction::TrackRepoPath { path: repo.clone() } })
        .await
        .expect("add_repo command should return an id");

    let (started_add, finished_add, added) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut started = None;
        let mut finished = None;
        let mut added = None;
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { command_id, repo_identity, .. }) if command_id == add_id => started = Some(repo_identity),
                Ok(DaemonEvent::CommandFinished { command_id, repo_identity, result, .. }) if command_id == add_id => {
                    finished = Some((repo_identity, result));
                }
                Ok(DaemonEvent::RepoTracked(info)) => added = Some(*info),
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
            if let (Some(_), Some(_), Some(_)) = (&started, &finished, &added) {
                break (started.take().expect("started set"), finished.take().expect("finished set"), added.take().expect("added set"));
            }
        }
    })
    .await
    .expect("timeout waiting for add command events");
    let (finished_identity, finished_result) = finished_add;
    assert!(matches!(finished_result, CommandValue::RepoTracked { ref path, .. } if *path == repo));
    assert_eq!(finished_identity, added.identity, "CommandFinished should use the tracked repo identity");
    assert_eq!(started_add, added.identity, "CommandStarted should use the tracked repo identity");
    assert_eq!(added.path, repo);

    let repos = daemon.list_repos().await.expect("list_repos after add");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].path, repo);

    let remove_id = daemon
        .execute(Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::UntrackRepo { repo: RepoSelector::Query("new-repo".into()) },
        })
        .await
        .expect("remove_repo command should return an id");
    let (finished_remove, removed) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut finished = None;
        let mut removed = None;
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == remove_id => finished = Some(result),
                Ok(DaemonEvent::RepoUntracked { path, .. }) => removed = Some(path),
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
            if let (Some(_), Some(_)) = (&finished, &removed) {
                break (finished.take().expect("finished set"), removed.take().expect("removed set"));
            }
        }
    })
    .await
    .expect("timeout waiting for remove command events");
    assert!(matches!(finished_remove, CommandValue::RepoUntracked { ref path } if *path == repo));
    assert_eq!(removed, repo);

    let repos = daemon.list_repos().await.expect("list_repos after remove");
    assert!(repos.is_empty());
}

#[tokio::test]
async fn duplicate_local_roots_share_identity_but_remain_tracked() {
    let (_temp, repo_a, repo_b, daemon) = daemon_for_duplicate_fake_repos().await;

    let identity_a = daemon.tracked_repo_identity_for_path(&repo_a).await.expect("identity for first repo");
    let identity_b = daemon.tracked_repo_identity_for_path(&repo_b).await.expect("identity for second repo");
    assert_eq!(identity_a, identity_b, "same upstream repo should resolve to one repo identity");

    let tracked = daemon.tracked_repo_paths().await;
    assert!(tracked.contains(&repo_a));
    assert!(tracked.contains(&repo_b));

    let repos = daemon.list_repos().await.expect("list_repos");
    assert_eq!(repos.len(), 1, "list_repos should expose one logical repo per identity");
    assert_eq!(repos[0].identity, identity_a);
    assert_eq!(repos[0].path, repo_a, "first tracked root should remain the deterministic preferred path");

    daemon.remove_repo(&repo_a).await.expect("remove preferred root");
    let repos = daemon.list_repos().await.expect("list_repos after removing preferred root");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].identity, identity_b);
    assert_eq!(repos[0].path, repo_b, "remaining root should become the preferred path");
    assert!(daemon.tracked_repo_identity_for_path(&repo_a).await.is_none());
    assert_eq!(daemon.tracked_repo_identity_for_path(&repo_b).await, Some(identity_b));
}

// TODO(task-9): Migrate to fake VCS — this test depends on real git for two reasons:
// 1. `normalize_repo_path` uses `GitVcs` directly to canonicalize symlinked temp paths
//    (e.g. /var → /private/var on macOS), so `tracked_path == canonical_repo` requires
//    a real git process to resolve the canonical form.
// 2. The identity match relies on git reading the remote URL; `local_bare_remote_discovery`
//    uses a real git runner to detect `github.com/owner/repo` from the remote.
// Skipping fake migration until `normalize_repo_path` uses an injectable Vcs.
#[tokio::test]
async fn adding_local_clone_promotes_remote_only_identity_to_local_execution() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let local_repo = temp.path().join("repo");
    let remote = temp.path().join("origin.git");
    let _ = init_git_repo_with_local_bare_remote(&local_repo, &remote);
    let identity = test_repo_identity();
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![], config, local_bare_remote_discovery(), HostName::local()).await;

    daemon.add_virtual_repo(identity.clone(), PathBuf::from("/remote/desktop/owner/repo"), vec![], 0).await.expect("add virtual repo");
    let (tracked_path, _) = daemon.add_repo(&local_repo).await.expect("add local repo");
    // Path may be canonicalized (e.g. /var -> /private/var on macOS)
    let canonical_repo = std::fs::canonicalize(&local_repo).unwrap_or_else(|_| local_repo.clone());

    let repos = daemon.list_repos().await.expect("list repos");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].identity, identity);
    assert_eq!(repos[0].path, canonical_repo, "local clone should become the preferred executable path");
    assert_eq!(tracked_path, canonical_repo);
    assert_eq!(daemon.preferred_local_path_for_identity(&identity).await, Some(canonical_repo.clone()));
    assert!(daemon.get_local_providers(&canonical_repo).await.is_some(), "local providers should now resolve for the identity");
    assert_eq!(daemon.tracked_repo_paths().await, vec![canonical_repo]);
}

#[tokio::test]
async fn removing_preferred_root_emits_snapshot_for_new_preferred_path() {
    let (_temp, repo_a, repo_b, daemon) = daemon_for_duplicate_fake_repos().await;
    let mut rx = daemon.subscribe();

    daemon.refresh(&RepoSelector::Path(repo_a.clone())).await.expect("refresh first repo");
    let _ = recv_event(&mut rx).await;

    daemon.remove_repo(&repo_a).await.expect("remove preferred root");

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::RepoSnapshot(snapshot)) => break Some(snapshot.repo),
                Ok(DaemonEvent::RepoDelta(delta)) => break Some(delta.repo),
                Ok(_) => {}
                Err(_) => break None,
            }
        }
    })
    .await
    .expect("timeout waiting for preferred-path snapshot")
    .expect("snapshot event");

    assert_eq!(event, repo_b, "surviving root should be broadcast immediately as the new preferred path");
}

#[tokio::test]
async fn get_local_providers_excludes_peer_overlay_data() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;

    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh local repo");

    let peer_checkout = HostPath::new(HostName::new("follower"), "/srv/follower/repo");
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(peer_checkout.clone(), Checkout {
        branch: "peer-branch".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    daemon.set_peer_providers(&repo, vec![(HostName::new("follower"), peer_data)], 0).await;

    let (providers, _) = daemon.get_local_providers(&repo).await.expect("local providers after peer overlay");
    assert!(
        !providers.checkouts.contains_key(&HostPath::new(HostName::local(), "/srv/follower/repo")),
        "peer overlay checkout should not be restamped and re-broadcast as local data"
    );
    assert!(
        !providers.checkouts.values().any(|checkout| checkout.branch == "peer-branch"),
        "peer overlay checkout should be excluded from local replication"
    );
}

/// Regression test: after poll_snapshots stores merged (local + peer) data
/// in last_snapshot, get_state must not re-attribute peer checkouts to the
/// local host. The bug: normalize_local_provider_hosts stamps ALL checkouts
/// in the merged base with the local host, then merge_provider_data adds
/// the real peer checkouts again — duplicating them.
#[tokio::test]
async fn get_state_does_not_reattribute_peer_checkouts_after_poll() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    // Initial refresh — populates last_snapshot with local-only data
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let peer_host = HostName::new("remote-peer-host");
    let peer_checkout_path = HostPath::new(peer_host.clone(), "/srv/remote/repo");
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(peer_checkout_path.clone(), Checkout {
        branch: "peer-feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    // Set peer providers
    daemon.set_peer_providers(&repo, vec![(peer_host.clone(), peer_data)], 0).await;
    let _ = recv_event(&mut rx).await;

    // Trigger refresh so poll_snapshots runs and stores merged data in last_snapshot.
    // This is the critical step — poll_snapshots merges local + peer into re_snapshot
    // and stores it in state.last_snapshot.
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Now get_state reads last_snapshot (merged) as base, normalizes ALL checkouts
    // to local host, then merges peers again. With the bug, kiwi's checkout appears
    // both as local (re-stamped) and as kiwi (re-merged).
    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("get_state after poll with peers");

    // The peer checkout should appear exactly once, attributed to kiwi
    let kiwi_checkouts: Vec<_> = snapshot.providers.checkouts.keys().filter(|hp| hp.host == peer_host).collect();
    assert_eq!(kiwi_checkouts.len(), 1, "peer checkout should appear once under kiwi");

    // The peer checkout must NOT appear re-attributed to the local host
    let local_host = HostName::local();
    let ghost_checkout = HostPath::new(local_host, PathBuf::from("/srv/remote/repo"));
    assert!(!snapshot.providers.checkouts.contains_key(&ghost_checkout), "peer checkout must not be re-stamped as a local checkout");
}

/// After poll_snapshots stores merged data, a second set_peer_providers call
/// should not duplicate peer checkouts via the normalize-then-merge path.
#[tokio::test]
async fn set_peer_providers_after_poll_does_not_duplicate_checkouts() {
    let (_temp, repo, daemon, _identity) = daemon_for_fake_repo().await;
    let mut rx = daemon.subscribe();

    // Initial refresh
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let peer_host = HostName::new("remote-peer-host");
    let peer_checkout_path = HostPath::new(peer_host.clone(), "/srv/remote/repo");
    let make_peer_data = |branch: &str| {
        let mut pd = ProviderData::default();
        pd.checkouts.insert(peer_checkout_path.clone(), Checkout {
            branch: branch.into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
            environment_id: None,
        });
        pd
    };

    // First peer update
    daemon.set_peer_providers(&repo, vec![(peer_host.clone(), make_peer_data("feat-v1"))], 0).await;
    let _ = recv_event(&mut rx).await;

    // Trigger refresh so poll_snapshots stores merged data in last_snapshot
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Second peer update — broadcast_snapshot_inner reads the merged last_snapshot,
    // normalizes all checkouts to local host, then merges peers again.
    daemon.set_peer_providers(&repo, vec![(peer_host.clone(), make_peer_data("feat-v2"))], 1).await;
    let _ = recv_event(&mut rx).await;

    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("get_state after poll + second peer update");

    let peer_count = snapshot.providers.checkouts.keys().filter(|hp| hp.host == peer_host).count();
    assert_eq!(peer_count, 1, "peer should have exactly 1 checkout, got {peer_count}");

    let local_host = HostName::local();
    let ghost_checkout = HostPath::new(local_host, PathBuf::from("/srv/remote/repo"));
    assert!(
        !snapshot.providers.checkouts.contains_key(&ghost_checkout),
        "peer path must not appear as a local checkout after poll + repeated peer updates"
    );
}

#[tokio::test]
async fn in_process_daemon_keeps_remote_attachable_set_anchor_when_local_workspace_is_bound() {
    let remote_host = definitely_remote_host();
    let remote_checkout = HostPath::new(remote_host.clone(), "/home/robert/dev/flotilla.terminal-stuff");
    let set_id = AttachableSetId::new("set-remote");
    let workspace_ref = "workspace:9".to_string();
    let workspace_manager = Arc::new(FakeWorkspaceManager::new());
    let attachable_store = shared_in_memory_attachable_store();

    workspace_manager
        .add_workspaces(vec![(workspace_ref.clone(), Workspace {
            name: "attachable-correlation@feta".into(),
            directories: vec![PathBuf::from("/Users/robert/dev/flotilla")],
            correlation_keys: vec![],
            attachable_set_id: None,
        })])
        .await;

    {
        let mut store = attachable_store.lock().expect("lock attachable store");
        store.insert_set(AttachableSet {
            id: set_id.clone(),
            host_affinity: Some(remote_host.clone()),
            checkout: Some(remote_checkout.clone()),
            template_identity: None,
            members: vec![],
        });
        store.replace_binding(ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: "fake-workspaces".into(),
            object_kind: flotilla_core::attachable::BindingObjectKind::AttachableSet,
            object_id: set_id.to_string(),
            external_ref: workspace_ref.clone(),
        });
    }

    let discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_workspace_manager(workspace_manager).with_attachable_store(Arc::clone(&attachable_store)),
    );
    let (_temp, repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    let mut rx = daemon.subscribe();

    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(remote_checkout.clone(), Checkout {
        branch: "attachable-correlation".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![
            CorrelationKey::Branch("attachable-correlation".into()),
            CorrelationKey::CheckoutPath(remote_checkout.clone()),
        ],
        association_keys: vec![],
        environment_id: None,
    });
    // The remote host projects sets matching its own checkouts, so
    // peer data includes the attachable set (simulating what the
    // remote refresh cycle would produce).
    peer_data.attachable_sets.insert(set_id.clone(), AttachableSet {
        id: set_id.clone(),
        host_affinity: Some(remote_host.clone()),
        checkout: Some(remote_checkout.clone()),
        template_identity: None,
        members: vec![],
    });
    daemon.set_peer_providers(&repo, vec![(remote_host.clone(), peer_data)], 0).await;
    let _ = recv_event(&mut rx).await;

    // Local providers no longer project sets whose checkout lives on a
    // remote host — the set arrives via peer data merge instead.
    let (local_providers, _) = daemon.get_local_providers(&repo).await.expect("local providers");
    assert!(local_providers.attachable_sets.get(&set_id).is_none(), "remote-checkout set should not appear in local projection");
    assert_eq!(
        local_providers.workspaces.get(&workspace_ref).and_then(|workspace| workspace.attachable_set_id.as_ref()),
        Some(&set_id),
        "workspace projection should retain the remote attachable set id"
    );

    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("merged state");
    let merged_set = snapshot.providers.attachable_sets.get(&set_id).expect("merged attachable set");
    assert_eq!(merged_set.host_affinity.as_ref(), Some(&remote_host));
    assert_eq!(merged_set.checkout.as_ref(), Some(&remote_checkout));

    let set_item =
        snapshot.work_items.iter().find(|item| item.attachable_set_id.as_ref() == Some(&set_id)).expect("attachable set work item");
    assert_eq!(set_item.host, remote_host);
    assert_eq!(set_item.checkout.as_ref().map(|checkout| &checkout.key), Some(&remote_checkout));
    assert_eq!(set_item.workspace_refs, vec![workspace_ref]);
}

#[tokio::test]
async fn in_process_daemon_correlates_workspace_into_one_remote_checkout_item() {
    let remote_host = definitely_remote_host();
    let remote_checkout = HostPath::new(remote_host.clone(), "/home/robert/dev/flotilla.issue-356-watch");
    let set_id = AttachableSetId::new("set-issue-356-watch");
    let workspace_ref = "workspace:10".to_string();
    let workspace_manager = Arc::new(FakeWorkspaceManager::new());
    let terminal_pool = Arc::new(FakeTerminalPool::new());
    let attachable_store = shared_in_memory_attachable_store();

    workspace_manager
        .add_workspaces(vec![(workspace_ref.clone(), Workspace {
            name: "issue-356-watch@feta".into(),
            directories: vec![PathBuf::from("/Users/robert/dev/flotilla")],
            correlation_keys: vec![],
            attachable_set_id: None,
        })])
        .await;

    {
        let mut store = attachable_store.lock().expect("lock attachable store");
        store.insert_set(AttachableSet {
            id: set_id.clone(),
            host_affinity: Some(remote_host.clone()),
            checkout: Some(remote_checkout.clone()),
            template_identity: None,
            members: vec![],
        });
        store.replace_binding(ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: "fake-workspaces".into(),
            object_kind: flotilla_core::attachable::BindingObjectKind::AttachableSet,
            object_id: set_id.to_string(),
            external_ref: workspace_ref.clone(),
        });
    }

    let discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new()
            .with_workspace_manager(workspace_manager)
            .with_terminal_pool(terminal_pool)
            .with_attachable_store(Arc::clone(&attachable_store)),
    );
    let (_temp, repo, daemon) = daemon_for_plain_dir_with_discovery(discovery).await;
    let mut rx = daemon.subscribe();

    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(remote_checkout.clone(), Checkout {
        branch: "issue-356-watch".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::Branch("issue-356-watch".into()), CorrelationKey::CheckoutPath(remote_checkout.clone())],
        association_keys: vec![],
        environment_id: None,
    });
    // The remote host projects sets matching its own checkouts, so
    // peer data includes the attachable set (simulating what the
    // remote refresh cycle would produce).
    peer_data.attachable_sets.insert(set_id.clone(), AttachableSet {
        id: set_id.clone(),
        host_affinity: Some(remote_host.clone()),
        checkout: Some(remote_checkout.clone()),
        template_identity: None,
        members: vec![],
    });
    daemon.set_peer_providers(&repo, vec![(remote_host.clone(), peer_data)], 0).await;
    let _ = recv_event(&mut rx).await;

    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("merged state");
    assert_eq!(
        snapshot.providers.workspaces.get(&workspace_ref).and_then(|workspace| workspace.attachable_set_id.as_ref()),
        Some(&set_id),
        "workspace projection should retain the shared attachable set id"
    );

    let matching_items: Vec<_> = snapshot.work_items.iter().filter(|item| item.attachable_set_id.as_ref() == Some(&set_id)).collect();
    assert_eq!(matching_items.len(), 1, "shared attachable identity should produce one correlated work item");
    let item = matching_items[0];
    assert_eq!(item.kind, WorkItemKind::Checkout, "checkout should remain the primary anchor when present");
    assert_eq!(item.host, remote_host);
    assert_eq!(item.checkout.as_ref().map(|checkout| &checkout.key), Some(&remote_checkout));
    assert_eq!(item.workspace_refs, vec![workspace_ref]);
}

#[tokio::test]
async fn inline_issue_command_returns_zero_and_skips_lifecycle_events() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    // Wait for initial snapshot event before issuing command.
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let command_id = daemon
        .execute(Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::ClearIssueSearch { repo: RepoSelector::Path(repo.clone()) },
        })
        .await
        .expect("inline command should succeed");
    assert_eq!(command_id, 0, "inline issue commands should return id=0");

    // Inline commands should not emit CommandStarted/Finished lifecycle events.
    let no_lifecycle = tokio::time::timeout(std::time::Duration::from_millis(300), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { .. }) | Ok(DaemonEvent::CommandFinished { .. }) => {
                    return false;
                }
                Ok(_) => {}
                Err(_) => return true,
            }
        }
    })
    .await;
    assert!(no_lifecycle.is_err() || no_lifecycle.unwrap(), "inline command unexpectedly emitted lifecycle event");
}

#[tokio::test]
async fn execute_on_untracked_repo_returns_error_without_started_event() {
    let config = Arc::new(ConfigStore::with_base(tempfile::tempdir().expect("tempdir").path()));
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();
    let repo = std::path::PathBuf::from("/tmp/does-not-exist-for-daemon-test");

    let err = daemon
        .execute(Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::Refresh { repo: Some(RepoSelector::Path(repo.clone())) },
        })
        .await
        .expect_err("untracked repo should fail");
    assert!(err.contains("repo not tracked"));

    let started = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { .. }) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await;
    assert!(started.is_err() || !started.unwrap(), "should not emit CommandStarted for invalid repo");
}

#[tokio::test]
async fn untrack_missing_repo_returns_error_without_started_event() {
    let config = Arc::new(ConfigStore::with_base(tempfile::tempdir().expect("tempdir").path()));
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();
    let repo = std::path::PathBuf::from("/tmp/does-not-exist-for-daemon-test");

    let err = daemon
        .execute(Command { host: None, environment: None, context_repo: None, action: CommandAction::UntrackRepo { repo: RepoSelector::Path(repo.clone()) } })
        .await
        .expect_err("untracked repo removal should fail");
    assert!(err.contains("repo not tracked"));

    let started = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { .. }) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await;
    assert!(started.is_err() || !started.unwrap(), "should not emit CommandStarted for missing repo removal");
}

#[tokio::test]
async fn refresh_all_command_refreshes_every_tracked_repo() {
    let temp = tempfile::tempdir().unwrap();
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo_a.clone(), repo_b.clone()], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();

    let refresh_id = daemon
        .execute(Command { host: None, environment: None, context_repo: None, action: CommandAction::Refresh { repo: None } })
        .await
        .expect("refresh all should return an id");

    let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == refresh_id => break result,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for refresh all CommandFinished");

    assert!(matches!(finished, CommandValue::Refreshed { repos } if repos.len() == 2));
}

#[tokio::test]
async fn remove_checkout_command_accepts_selector_queries() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let err = daemon
        .execute(Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("does-not-exist".into()) },
        })
        .await
        .expect_err("missing checkout should fail cleanly");

    assert!(
        err.contains("checkout") || err.contains("does-not-exist") || err.contains(repo.to_string_lossy().as_ref()),
        "expected checkout resolution error, got {err}"
    );
}

#[tokio::test]
async fn fetch_checkout_status_uses_context_repo_when_checkout_path_is_absent() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    let command = Command {
        host: None,
        environment: None,
        context_repo: Some(RepoSelector::Path(repo.clone())),
        action: CommandAction::FetchCheckoutStatus { branch: "main".into(), checkout_path: None, change_request_id: None },
    };

    let command_id = daemon.execute(command).await.expect("status command should resolve via context repo");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: finished_id, result, .. }) if finished_id == command_id => break result,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for checkout status command to finish");

    assert!(matches!(result, CommandValue::CheckoutStatus(_)), "expected checkout status result via context repo, got {result:?}");
}

#[tokio::test]
async fn checkout_target_branch_and_fresh_branch_are_distinct_errors() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    let branch_id = daemon
        .execute(Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::Checkout {
                repo: RepoSelector::Path(repo.clone()),
                target: CheckoutTarget::Branch("definitely-missing-branch".into()),
                issue_ids: vec![],
            },
        })
        .await
        .expect("checking out a missing existing branch should return a command id");

    let fresh_id = daemon
        .execute(Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::Checkout {
                repo: RepoSelector::Path(repo),
                target: CheckoutTarget::FreshBranch("main".into()),
                issue_ids: vec![],
            },
        })
        .await
        .expect("creating a fresh branch that already exists should return a command id");
    let mut branch_err = None;
    let mut fresh_err = None;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while branch_err.is_none() || fresh_err.is_none() {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == branch_id => match result {
                    CommandValue::Error { message } => branch_err = Some(message),
                    other => panic!("expected error for Branch checkout, got {other:?}"),
                },
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == fresh_id => match result {
                    CommandValue::Error { message } => fresh_err = Some(message),
                    other => panic!("expected error for FreshBranch checkout, got {other:?}"),
                },
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await;
    outcome.expect("timed out waiting for checkout failures");

    assert_ne!(branch_err, fresh_err, "Branch and FreshBranch should remain distinct intents");
}

#[tokio::test]
async fn follower_mode_flag_is_stored() {
    let config = Arc::new(ConfigStore::with_base(tempfile::tempdir().expect("tempdir").path()));
    let leader = InProcessDaemon::new(vec![], config.clone(), fake_discovery(false), HostName::local()).await;
    assert!(!leader.is_follower(), "default daemon should not be follower");

    let follower = InProcessDaemon::new(vec![], config, fake_discovery(true), HostName::local()).await;
    assert!(follower.is_follower(), "follower daemon should report follower=true");
}

#[tokio::test]
async fn follower_mode_skips_external_providers() {
    // Use a temp dir with a .git directory to guarantee VCS detection
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".git")).unwrap();

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(true), HostName::local()).await;

    assert!(daemon.is_follower());

    // list_repos gives us RepoInfo with provider_names populated from the registry
    let repos = daemon.list_repos().await.expect("list_repos");
    assert_eq!(repos.len(), 1);
    let provider_names = &repos[0].provider_names;

    // VCS should be present (local provider, .git dir exists)
    assert!(provider_names.contains_key("vcs"), "follower should have VCS provider");
    // checkout_manager should also be present (git-based fallback)
    assert!(provider_names.contains_key("checkout_manager"), "follower should have checkout_manager provider");

    // External providers should be absent
    assert!(!provider_names.contains_key("change_request"), "follower should not have change_request provider");
    assert!(!provider_names.contains_key("issue_tracker"), "follower should not have issue_tracker provider");
    // cloud_agent and ai_utility depend on Claude/Codex/Cursor being
    // installed, so they may or may not be present in non-follower mode.
    // In follower mode they should always be absent.
    assert!(!provider_names.contains_key("cloud_agent"), "follower should not have cloud_agent provider");
    assert!(!provider_names.contains_key("ai_utility"), "follower should not have ai_utility provider");
}

#[tokio::test]
async fn add_virtual_repo_emits_repo_tracked_then_snapshot_and_is_queryable() {
    let config = Arc::new(ConfigStore::with_base(tempfile::tempdir().expect("tempdir").path()));
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();

    let synthetic_path = PathBuf::from("<remote>/desktop/home/dev/repo");
    let identity = RepoIdentity { authority: "github.com".into(), path: "owner/remote-only".into() };
    let peer_host = HostName::new("peer-a");
    let peer_checkout_path = PathBuf::from("/srv/peer-a/repo");
    let peers = vec![(peer_host.clone(), ProviderData {
        checkouts: indexmap::IndexMap::from([(HostPath::new(peer_host.clone(), peer_checkout_path.clone()), Checkout {
            branch: "feat-remote".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![CorrelationKey::Branch("feat-remote".into())],
            association_keys: vec![],
            environment_id: None,
        })]),
        ..Default::default()
    })];

    daemon.add_virtual_repo(identity.clone(), synthetic_path.clone(), peers, 0).await.expect("add_virtual_repo should succeed");

    // Collect events: expect RepoTracked followed by a snapshot.
    let events = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        loop {
            match rx.recv().await {
                Ok(e @ DaemonEvent::RepoTracked(_)) => collected.push(e),
                Ok(e @ DaemonEvent::RepoSnapshot(_)) => {
                    collected.push(e);
                    break;
                }
                Ok(e @ DaemonEvent::RepoDelta(_)) => {
                    collected.push(e);
                    break;
                }
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
        collected
    })
    .await
    .expect("timeout waiting for events");

    // RepoTracked must come first.
    assert!(matches!(&events[0], DaemonEvent::RepoTracked(info) if info.identity == identity));
    // Followed by a snapshot (not a delta — there's no previous baseline).
    assert!(matches!(&events[1], DaemonEvent::RepoSnapshot(_)), "second event should be a full snapshot, got {:?}", events[1]);

    // Should appear in list_repos.
    let repos = daemon.list_repos().await.expect("list_repos");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].path, synthetic_path);
    assert!(!repos[0].loading);

    // get_state() should return the peer checkout data immediately.
    let state = daemon.get_state(&RepoSelector::Identity(identity.clone())).await.expect("get_state should succeed");
    assert!(!state.providers.checkouts.is_empty(), "peer checkout should be present in snapshot");
    let has_remote_checkout = state.providers.checkouts.values().any(|co| co.branch == "feat-remote");
    assert!(has_remote_checkout, "snapshot should contain the peer's feat-remote checkout");

    // Work items should include the correlated peer checkout.
    assert!(!state.work_items.is_empty(), "work items should be populated from peer data");
}

#[tokio::test]
async fn add_virtual_repo_is_idempotent() {
    let config = Arc::new(ConfigStore::with_base(tempfile::tempdir().expect("tempdir").path()));
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;

    let synthetic_path = PathBuf::from("<remote>/desktop/home/dev/repo");
    let identity = RepoIdentity { authority: "github.com".into(), path: "owner/remote-only".into() };
    daemon.add_virtual_repo(identity.clone(), synthetic_path.clone(), vec![], 0).await.expect("first add should succeed");

    // Second add with same path should be a no-op
    daemon.add_virtual_repo(identity, synthetic_path.clone(), vec![], 0).await.expect("second add should succeed (idempotent)");

    let repos = daemon.list_repos().await.expect("list_repos");
    assert_eq!(repos.len(), 1, "should still have exactly one repo");
}

#[tokio::test]
async fn get_status_returns_repo_summaries() {
    let (_temp, _repo, daemon) = daemon_for_cwd().await;
    let repo = daemon.list_repos().await.expect("list_repos").into_iter().next().expect("tracked repo").path;
    let mut rx = daemon.subscribe();
    trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let status = daemon.get_status().await.expect("get_status failed");
    assert!(!status.repos.is_empty());
    let summary = &status.repos[0];
    assert!(summary.path.exists());
}

#[tokio::test]
async fn get_repo_work_returns_work_items() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();
    trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let repo_name = repo.file_name().expect("repo should have a file name").to_str().expect("repo name should be valid UTF-8");
    let work = daemon.get_repo_work_internal(&RepoSelector::Query(repo_name.to_string())).await.expect("get_repo_work failed");
    assert_eq!(work.path, repo);
    // Work items may or may not be present depending on repo state, but the call should succeed
}

#[tokio::test]
async fn get_repo_detail_returns_provider_health_and_errors() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(
        vec![repo.clone()],
        config,
        fake_discovery_with_providers(None, Some(Arc::new(FailingChangeRequestTracker)), None),
        HostName::local(),
    )
    .await;
    let mut rx = daemon.subscribe();
    trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let repo_name = repo.file_name().expect("repo should have a file name").to_str().expect("repo name should be valid UTF-8");
    let detail = daemon.get_repo_detail_internal(&RepoSelector::Query(repo_name.to_string())).await.expect("get_repo_detail failed");

    assert_eq!(detail.path, repo);
    let change_request_health = detail.provider_health.get("change_request").expect("change_request health should be present");
    assert!(change_request_health.values().any(|healthy| !healthy), "provider health should reflect refresh errors");
    assert!(
        detail.errors.iter().any(|err| err.category == "PRs" && err.message == "change request listing failed"),
        "should expose refresh errors from the failing provider"
    );
}

#[tokio::test]
async fn get_repo_providers_returns_structured_unmet_requirements_and_discovery() {
    let (_temp, repo, daemon) = daemon_for_plain_dir().await;

    let repo_name = repo.file_name().expect("repo should have a file name").to_str().expect("repo name should be valid UTF-8");
    let providers =
        daemon.get_repo_providers_internal(&RepoSelector::Query(repo_name.to_string())).await.expect("get_repo_providers failed");

    assert_eq!(providers.path, repo);
    assert!(
        providers.host_discovery.iter().any(|entry| entry.kind == "binary_available" && entry.detail.get("name") == Some(&"git".into())),
        "should include host discovery assertions"
    );
    assert!(
        providers
            .unmet_requirements
            .iter()
            .any(|req| { req.factory == "github" && req.kind == "missing_binary" && req.value.as_deref() == Some("gh") }),
        "should expose structured valued unmet requirements"
    );
    assert!(
        providers.unmet_requirements.iter().any(|req| req.factory == "git" && req.kind == "no_vcs_checkout" && req.value.is_none()),
        "should expose valueless unmet requirements without forcing a placeholder string"
    );
}

#[tokio::test]
async fn cancel_nonexistent_command_returns_error() {
    let (_temp, _repo, daemon) = daemon_for_cwd().await;
    let result = daemon.cancel(999).await;
    assert!(result.is_err(), "cancelling a non-existent command should fail");
    assert!(result.unwrap_err().contains("no matching active command"), "error should mention no matching active command");
}

#[tokio::test]
async fn linked_issue_pinning_fetches_and_broadcasts_missing_issues() {
    // --- Arrange ---

    // Create a checkout that references issue #42
    let checkout_manager = Arc::new(FakeCheckoutManager::new());
    checkout_manager
        .add_checkouts(vec![(PathBuf::from("/tmp/repo/feat-branch"), Checkout {
            branch: "feat-branch".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![CorrelationKey::Branch("feat-branch".into())],
            association_keys: vec![AssociationKey::IssueRef("fake-issues".into(), "42".into())],
            environment_id: None,
        })])
        .await;

    // Create an issue tracker that has issue #42 available
    let issue_tracker = Arc::new(FakeIssueTracker::new());
    issue_tracker
        .add_issues(vec![("42".into(), Issue {
            title: "Fix the widget".into(),
            labels: vec!["bug".into()],
            association_keys: vec![AssociationKey::IssueRef("fake-issues".into(), "42".into())],
            provider_name: "fake-issues".into(),
            provider_display_name: "Fake Issues".into(),
        })])
        .await;

    let discovery = fake_discovery_with_providers(
        Some(checkout_manager.clone() as Arc<dyn flotilla_core::providers::vcs::CheckoutManager>),
        None,
        Some(issue_tracker.clone() as Arc<dyn flotilla_core::providers::issue_tracker::IssueTracker>),
    );

    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = Arc::new(flotilla_core::config::ConfigStore::with_base(temp.path().join("config")));
    let daemon =
        flotilla_core::in_process::InProcessDaemon::new(vec![repo.clone()], config, discovery, flotilla_protocol::HostName::local()).await;

    let mut rx = daemon.subscribe();

    // --- Act ---
    // Trigger a refresh. The refresh loop will:
    // 1. Call FakeCheckoutManager::list_checkouts → checkout with IssueRef("42")
    // 2. Broadcast initial snapshot (no issues yet)
    // 3. Call fetch_missing_linked_issues → finds "42" missing → calls fetch_issues_by_id
    // 4. Broadcast updated snapshot with pinned issue
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh should succeed");

    // --- Assert ---
    // Collect snapshot events until we see one containing issue "42".
    // The daemon may send a RepoSnapshot or a RepoDelta depending on
    // whether the delta is smaller than the full snapshot. We accept either.
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::RepoSnapshot(snap)) if snap.repo == repo => {
                    if snap.providers.issues.contains_key("42") {
                        return *snap;
                    }
                }
                Ok(DaemonEvent::RepoDelta(ref delta)) if delta.repo == repo => {
                    // Check if the delta contains an Issue change for "42"
                    let has_issue_42 = delta.changes.iter().any(|c| matches!(c, Change::Issue { key, .. } if key == "42"));
                    if has_issue_42 {
                        // Use replay_since to get the full snapshot with the issue
                        let events = daemon.replay_since(&HashMap::new()).await.expect("replay_since");
                        for event in events {
                            if let DaemonEvent::RepoSnapshot(snap) = event {
                                if snap.repo == repo && snap.providers.issues.contains_key("42") {
                                    return *snap;
                                }
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for snapshot with pinned issue");

    // Verify the issue is present and correct
    let issue = found.providers.issues.get("42").expect("issue 42 should be in snapshot");
    assert_eq!(issue.title, "Fix the widget");
    assert_eq!(issue.labels, vec!["bug".to_string()]);

    // Verify fetch_issues_by_id was actually called (not just paginated)
    let fetched: Vec<Vec<String>> = issue_tracker.fetched_by_id.lock().await.clone();
    assert!(!fetched.is_empty(), "fetch_issues_by_id should have been called");
    assert!(fetched.iter().any(|ids| ids.contains(&"42".to_string())), "fetch_issues_by_id should have been called with id '42'");
}

#[tokio::test]
async fn attachable_set_cascade_deletes_on_checkout_removal() {
    // --- Arrange ---
    // Create a checkout manager with a branch that will be removed.
    let checkout_manager = Arc::new(FakeCheckoutManager::new());
    let checkout_path = PathBuf::from("/tmp/repo/wt-feat-lifecycle");
    let host_path = flotilla_protocol::HostPath::new(HostName::local(), checkout_path.clone());
    checkout_manager
        .add_checkouts(vec![(checkout_path, Checkout {
            branch: "feat-lifecycle".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![CorrelationKey::Branch("feat-lifecycle".into()), CorrelationKey::CheckoutPath(host_path.clone())],
            association_keys: vec![],
            environment_id: None,
        })])
        .await;

    // Create an attachable store with a set anchored to the checkout.
    let attachable_store = shared_in_memory_attachable_store();
    let set_id = {
        let mut store = attachable_store.lock().expect("lock attachable store");
        let set_id = store.ensure_terminal_set(Some(HostName::local()), Some(host_path.clone()));
        store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "fake-terminals",
            "flotilla/feat-lifecycle/shell/0",
            TerminalPurpose { checkout: "feat-lifecycle".into(), role: "shell".into(), index: 0 },
            "bash",
            flotilla_core::path_context::ExecutionEnvironmentPath::new("/tmp/repo/wt-feat-lifecycle"),
            flotilla_protocol::TerminalStatus::Running,
        );
        set_id
    };

    let terminal_pool = Arc::new(FakeTerminalPool::new());
    let discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new()
            .with_checkout_manager(checkout_manager.clone() as Arc<dyn flotilla_core::providers::vcs::CheckoutManager>)
            .with_terminal_pool(terminal_pool)
            .with_attachable_store(Arc::clone(&attachable_store)),
    );

    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = Arc::new(flotilla_core::config::ConfigStore::with_base(temp.path().join("config")));
    let daemon = flotilla_core::in_process::InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;
    let mut rx = daemon.subscribe();

    // --- Act: initial refresh ---
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Verify the attachable set appears in the repo snapshot.
    let snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("get_state");
    assert!(snapshot.providers.attachable_sets.contains_key(&set_id), "attachable set should appear in repo snapshot after refresh");

    // --- Act: remove checkout ---
    let command = Command {
        host: None,
        environment: None,
        context_repo: None,
        action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("feat-lifecycle".into()) },
    };
    let command_id = daemon.execute(command).await.expect("execute RemoveCheckout should succeed");

    // Wait for the command to finish.
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for RemoveCheckout to finish");

    assert!(!matches!(result, CommandValue::Error { .. }), "RemoveCheckout should succeed, got: {result:?}");

    // --- Assert: set removed from store ---
    {
        let store = attachable_store.lock().expect("lock attachable store");
        assert!(store.registry().sets.is_empty(), "attachable set should be cascade-deleted from store");
        assert!(store.registry().attachables.is_empty(), "attachable members should be cascade-deleted from store");
        assert!(store.registry().bindings.is_empty(), "attachable bindings should be cascade-deleted from store");
    }

    // --- Assert: set no longer in snapshot after refresh ---
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("post-removal refresh");
    // Drain events until we get a snapshot/delta.
    let _ = recv_event(&mut rx).await;

    let snapshot_after = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("get_state after removal");
    assert!(
        !snapshot_after.providers.attachable_sets.contains_key(&set_id),
        "attachable set should not appear in snapshot after checkout removal"
    );
}

#[tokio::test]
async fn issue_refresh_escalation_resets_cache_and_refetches() {
    // --- Arrange ---
    // Seed a FakeIssueTracker with 55 initial issues. The `per_page` used by
    // `ensure_issues_cached` is 50, so 55 issues requires two pages. After
    // escalation, the daemon records `prev_count = 55`, resets the cache,
    // fetches page 1 (50 issues), then `ensure_issues_cached` sees
    // `cache.len() (50) < desired_count (55)` and fetches page 2 — proving
    // multi-page continuation works.
    fn make_issue(n: u32) -> (String, Issue) {
        let mut issue = flotilla_protocol::test_support::TestIssue::new(&format!("Issue {n}")).build();
        issue.provider_name = "fake-issues".into();
        issue.provider_display_name = "Fake Issues".into();
        (n.to_string(), issue)
    }

    let issue_tracker = Arc::new(FakeIssueTracker::new());
    let initial_issues: Vec<_> = (1..=55).map(make_issue).collect();
    issue_tracker.add_issues(initial_issues).await;

    let discovery = fake_discovery_with_providers(
        None,
        None,
        Some(issue_tracker.clone() as Arc<dyn flotilla_core::providers::issue_tracker::IssueTracker>),
    );

    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;

    let mut rx = daemon.subscribe();

    // Trigger initial refresh to populate issue cache with all 55 issues.
    // Use FetchMoreIssues with desired_count=60 so it fetches both pages.
    daemon
        .execute(Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::FetchMoreIssues { repo: RepoSelector::Path(repo.clone()), desired_count: 60 },
        })
        .await
        .expect("initial FetchMoreIssues should succeed");

    // Verify initial state: all 55 issues should be cached.
    let initial_snapshot = daemon.get_state(&RepoSelector::Path(repo.clone())).await.expect("get initial state");
    assert_eq!(initial_snapshot.providers.issues.len(), 55, "should have 55 issues initially cached");

    // --- Act ---
    // Add 5 new issues (simulating upstream changes) and enable forced
    // escalation. Total is now 60 issues across two pages (50 + 10).
    let new_issues: Vec<_> = (56..=60)
        .map(|n| {
            (n.to_string(), Issue {
                title: format!("Issue {n}"),
                labels: vec!["new".into()],
                association_keys: vec![],
                provider_name: "fake-issues".into(),
                provider_display_name: "Fake Issues".into(),
            })
        })
        .collect();
    issue_tracker.add_issues(new_issues).await;
    issue_tracker.set_force_escalation(true);

    // Clear pages_fetched so we can observe just the escalation fetches.
    issue_tracker.pages_fetched.lock().await.clear();

    // Set last_refreshed_at to a timestamp far in the past so the
    // MIN_INTERVAL_SECS (30s) guard in refresh_issues_incremental passes.
    daemon.set_issue_cache_refreshed_at_for_test(&repo, "2020-01-01T00:00:00Z").await;

    // Drain any pending events before triggering the escalation.
    while rx.try_recv().is_ok() {}

    // Directly invoke the incremental issue refresh. Since force_escalation
    // is enabled, list_issues_changed_since will return has_more: true,
    // triggering the full re-fetch escalation path.
    daemon.refresh_issues_incremental_for_test().await;

    // --- Assert ---
    // The escalation path should have: reset the cache, fetched page 1
    // (50 issues) via list_issues_page, then ensure_issues_cached should
    // have fetched page 2 (10 issues) because prev_count (55) > page 1
    // count (50), and finally broadcast a snapshot.

    // Verify multi-page fetches occurred: page 1 (escalation) + page 2
    // (ensure_issues_cached continuation).
    let pages = issue_tracker.pages_fetched.lock().await.clone();
    assert!(pages.contains(&1), "escalation should fetch page 1");
    assert!(pages.contains(&2), "ensure_issues_cached should continue to page 2");

    // Wait for the broadcast snapshot containing the new issues.
    let found = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::RepoSnapshot(snap)) if snap.repo == repo => {
                    if snap.providers.issues.contains_key("56") {
                        return *snap;
                    }
                }
                Ok(DaemonEvent::RepoDelta(ref delta)) if delta.repo == repo => {
                    let has_new_issue = delta.changes.iter().any(|c| matches!(c, Change::Issue { key, .. } if key == "56"));
                    if has_new_issue {
                        let events = daemon.replay_since(&HashMap::new()).await.expect("replay_since");
                        for event in events {
                            if let DaemonEvent::RepoSnapshot(snap) = event {
                                if snap.repo == repo && snap.providers.issues.contains_key("56") {
                                    return *snap;
                                }
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for snapshot with escalated issues");

    // The snapshot should contain all 60 issues from both pages after the
    // full re-fetch with multi-page continuation.
    assert_eq!(found.providers.issues.len(), 60, "escalation should re-fetch all 60 issues across two pages");

    // Spot-check issues from page 1 (IDs 1-50) and page 2 (IDs 51-60).
    assert!(found.providers.issues.contains_key("1"), "first issue on page 1 present");
    assert!(found.providers.issues.contains_key("50"), "last issue on page 1 present");
    assert!(found.providers.issues.contains_key("51"), "first issue on page 2 present");
    assert!(found.providers.issues.contains_key("60"), "last issue on page 2 present");

    // Verify the new issues added after initial fetch have expected content.
    let issue_56 = found.providers.issues.get("56").expect("issue 56 in snapshot");
    assert_eq!(issue_56.title, "Issue 56");
    assert_eq!(issue_56.labels, vec!["new".to_string()]);
}

#[tokio::test]
async fn two_commands_can_run_concurrently() {
    // --- Arrange ---
    // Set up a daemon with a SlowCloudAgent that blocks on archive_session.
    // There is no AI utility, so GenerateBranchName falls back immediately.
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let agent = Arc::new(SlowCloudAgent::new());
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, slow_cloud_agent_discovery(Arc::clone(&agent)), HostName::local()).await;
    let mut rx = daemon.subscribe();

    // Refresh so the session appears in providers_data.
    let refresh_event = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;
    match refresh_event {
        DaemonEvent::RepoSnapshot(snap) => assert!(snap.providers.sessions.contains_key("sess-1"), "refresh should expose sess-1"),
        DaemonEvent::RepoDelta(delta) => {
            assert!(delta.work_items.iter().any(|item| item.session_key.as_deref() == Some("sess-1")), "refresh should expose sess-1")
        }
        other => panic!("expected snapshot event, got {other:?}"),
    }

    // --- Act: start first command (blocks inside archive_session) ---
    let archive_cmd = Command {
        host: None,
        environment: None,
        context_repo: Some(RepoSelector::Path(repo.clone())),
        action: CommandAction::ArchiveSession { session_id: "sess-1".into() },
    };
    let archive_id = daemon.execute(archive_cmd).await.expect("execute ArchiveSession should return a command id");

    // Wait for the first command to start.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { command_id: id, .. }) if id == archive_id => break,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for ArchiveSession CommandStarted");

    // Wait until the slow agent is actually inside archive_session.
    agent.wait_for_archive_start().await;

    // --- Act: start second command while first is still blocking ---
    // GenerateBranchName with no AI utility completes immediately with a fallback result.
    let branch_cmd = Command {
        host: None,
        environment: None,
        context_repo: Some(RepoSelector::Path(repo.clone())),
        action: CommandAction::GenerateBranchName { issue_keys: vec![] },
    };
    let branch_id = daemon.execute(branch_cmd).await.expect("execute GenerateBranchName should return a command id");

    // --- Assert: second command completes successfully while first is still blocked ---
    let branch_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == branch_id => break result,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for GenerateBranchName to finish — concurrent execution may be blocked");

    assert!(!matches!(branch_result, CommandValue::Error { .. }), "GenerateBranchName should succeed concurrently, got: {branch_result:?}");

    // --- Cleanup: release the first command and verify it finishes ---
    agent.release_archive();

    let archive_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == archive_id => break result,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for ArchiveSession to finish");

    assert!(
        !matches!(archive_result, CommandValue::Error { .. }),
        "ArchiveSession should complete successfully after release, got: {archive_result:?}"
    );
}
