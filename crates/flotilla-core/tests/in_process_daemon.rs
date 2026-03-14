use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{
        ai_utility::AiUtility,
        coding_agent::CloudAgentService,
        discovery::{
            test_support::{fake_discovery, fake_discovery_with_providers, git_process_discovery, FakeCheckoutManager, FakeIssueTracker},
            DiscoveryRuntime, EnvironmentBag, Factory, ProviderDescriptor, UnmetRequirement,
        },
        types::{CloudAgentSession, RepoCriteria, SessionStatus},
    },
};
use flotilla_protocol::{
    AssociationKey, Change, Checkout, CheckoutSelector, CheckoutTarget, Command, CommandAction, CommandResult, CorrelationKey, DaemonEvent,
    HostName, Issue, ProviderData, RepoIdentity, RepoSelector,
};
use tokio::sync::Notify;

fn init_git_repo_with_remote(path: &Path, remote: &str) {
    std::fs::create_dir_all(path).expect("create repo dir");

    let status = ProcessCommand::new("git").args(["init", "--initial-branch=main"]).arg(path).status().expect("git init");
    assert!(status.success(), "git init should succeed");

    let path_str = path.to_str().expect("repo path utf8");
    let status =
        ProcessCommand::new("git").args(["-C", path_str, "config", "user.name", "Flotilla Tests"]).status().expect("git config name");
    assert!(status.success(), "git config user.name should succeed");

    let status = ProcessCommand::new("git")
        .args(["-C", path_str, "config", "user.email", "flotilla@example.com"])
        .status()
        .expect("git config email");
    assert!(status.success(), "git config user.email should succeed");

    let status =
        ProcessCommand::new("git").args(["-C", path_str, "remote", "add", "origin", remote]).status().expect("git remote add origin");
    assert!(status.success(), "git remote add origin should succeed");
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
        ProviderDescriptor::labeled("slow-agent", "Slow Agent", "AG", "Sessions", "session")
    }

    async fn probe(
        &self,
        _: &EnvironmentBag,
        _: &ConfigStore,
        _: &Path,
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
        ProviderDescriptor::named("slow-ai")
    }

    async fn probe(
        &self,
        _: &EnvironmentBag,
        _: &ConfigStore,
        _: &Path,
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

async fn daemon_for_cwd() -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::local()).await;
    (temp, repo, daemon)
}

async fn daemon_for_git_repo(remote: &str) -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>, RepoIdentity) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    init_git_repo_with_remote(&repo, remote);
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, git_process_discovery(false), HostName::local()).await;
    let identity = daemon.find_identity_for_path(&repo).await.expect("repo identity should be detected");
    (temp, repo, daemon, identity)
}

async fn recv_event(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>) -> DaemonEvent {
    tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await.expect("timeout waiting for event").expect("recv error")
}

async fn trigger_refresh_and_recv(
    daemon: &Arc<InProcessDaemon>,
    repo: &Path,
    rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
) -> DaemonEvent {
    daemon.refresh(repo).await.expect("refresh should succeed");
    recv_event(rx).await
}

#[tokio::test]
async fn daemon_broadcasts_snapshots() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    let event = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    match event {
        DaemonEvent::SnapshotFull(snap) => {
            assert_eq!(snap.repo, repo);
            assert!(snap.seq > 0);
        }
        DaemonEvent::SnapshotDelta(delta) => {
            assert_eq!(delta.repo, repo);
            assert!(delta.seq > 0);
        }
        other => panic!("expected SnapshotFull or SnapshotDelta, got {:?}", other),
    }
}

#[tokio::test]
async fn execute_broadcasts_lifecycle_events() {
    let (_temp, repo, daemon, identity) = daemon_for_git_repo("git@github.com:owner/repo.git").await;
    let mut rx = daemon.subscribe();

    // Execute a command that goes through the spawned task path.
    // ArchiveSession with a non-existent ID returns immediately with
    // "session not found" — no external API calls, deterministic.
    // We only care about the lifecycle events, not the command result.
    let command = Command {
        host: None,
        context_repo: Some(RepoSelector::Identity(identity.clone())),
        action: CommandAction::ArchiveSession { session_id: "nonexistent-session".into() },
    };
    let command_id = daemon.execute(command).await.expect("execute should return a command id");

    // Collect CommandStarted and CommandFinished events, skipping any
    // Snapshot events that arrive from the background refresh loop.
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
    let (_temp, _repo, daemon, identity) = daemon_for_git_repo("git@github.com:owner/repo.git").await;
    let mut rx = daemon.subscribe();

    let command = Command {
        host: None,
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

    assert!(
        matches!(result, CommandResult::CheckoutStatus(_)),
        "expected checkout status result via identity context repo, got {result:?}"
    );
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
        DaemonEvent::SnapshotFull(snap) => assert!(snap.providers.sessions.contains_key("sess-1"), "refresh should expose sess-1"),
        DaemonEvent::SnapshotDelta(delta) => {
            assert!(delta.work_items.iter().any(|item| item.session_key.as_deref() == Some("sess-1")), "refresh should expose sess-1")
        }
        other => panic!("expected snapshot event, got {other:?}"),
    }

    let command = Command {
        host: None,
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

    assert_eq!(result, CommandResult::Cancelled);
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

    daemon.refresh(&repo).await.expect("refresh should succeed");

    let command = Command {
        host: None,
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

    assert_eq!(result, CommandResult::Cancelled);
}

#[tokio::test]
async fn replay_since_returns_full_snapshot_for_unknown_seq() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;

    // Wait for at least one broadcast so the daemon has state
    let mut rx = daemon.subscribe();
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    // Request replay with a seq that won't be in the delta log
    let last_seen = HashMap::from([(repo.clone(), 999999)]);
    let events = daemon.replay_since(&last_seen).await.expect("replay_since");

    assert_eq!(events.len(), 1, "should get exactly one event");
    match &events[0] {
        DaemonEvent::SnapshotFull(snap) => {
            assert_eq!(snap.repo, repo);
        }
        other => panic!("expected SnapshotFull, got {:?}", other),
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

    assert_eq!(events.len(), 1, "should get one event per tracked repo");
    match &events[0] {
        DaemonEvent::SnapshotFull(snap) => {
            assert_eq!(snap.repo, repo);
        }
        other => panic!("expected SnapshotFull, got {:?}", other),
    }
}

#[tokio::test]
async fn replay_since_returns_empty_when_up_to_date() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;

    // Wait for the first snapshot to get the current seq
    let mut rx = daemon.subscribe();
    let event = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let current_seq = match event {
        DaemonEvent::SnapshotFull(snap) => snap.seq,
        DaemonEvent::SnapshotDelta(delta) => delta.seq,
        other => panic!("expected snapshot event, got {:?}", other),
    };

    // Request replay at current seq — should return nothing
    let last_seen = HashMap::from([(repo.clone(), current_seq)]);
    let events = daemon.replay_since(&last_seen).await.expect("replay_since");

    assert!(events.is_empty(), "should be empty when up to date");
}

#[tokio::test]
async fn add_and_remove_repo_updates_state_and_emits_events() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("new-repo");
    init_git_repo_with_remote(&repo, "git@github.com:owner/new-repo.git");

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();

    let add_id = daemon
        .execute(Command { host: None, context_repo: None, action: CommandAction::AddRepo { path: repo.clone() } })
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
                Ok(DaemonEvent::RepoAdded(info)) => added = Some(*info),
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
    assert!(matches!(finished_result, CommandResult::RepoAdded { path } if path == repo));
    assert_eq!(finished_identity, added.identity, "CommandFinished should use the tracked repo identity");
    assert_eq!(started_add, added.identity, "CommandStarted should use the tracked repo identity");
    assert_eq!(added.path, repo);

    let repos = daemon.list_repos().await.expect("list_repos after add");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].path, repo);

    let remove_id = daemon
        .execute(Command {
            host: None,
            context_repo: None,
            action: CommandAction::RemoveRepo { repo: RepoSelector::Query("new-repo".into()) },
        })
        .await
        .expect("remove_repo command should return an id");
    let (finished_remove, removed) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut finished = None;
        let mut removed = None;
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == remove_id => finished = Some(result),
                Ok(DaemonEvent::RepoRemoved { path, .. }) => removed = Some(path),
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
    assert!(matches!(finished_remove, CommandResult::RepoRemoved { path } if path == repo));
    assert_eq!(removed, repo);

    let repos = daemon.list_repos().await.expect("list_repos after remove");
    assert!(repos.is_empty());
}

#[tokio::test]
async fn inline_issue_command_returns_zero_and_skips_lifecycle_events() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    // Wait for initial snapshot event before issuing command.
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let command_id = daemon
        .execute(Command { host: None, context_repo: None, action: CommandAction::ClearIssueSearch { repo: repo.clone() } })
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
    let config = Arc::new(ConfigStore::new());
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();
    let repo = std::path::PathBuf::from("/tmp/does-not-exist-for-daemon-test");

    let err = daemon
        .execute(Command {
            host: None,
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
        .execute(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } })
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

    assert!(matches!(finished, CommandResult::Refreshed { repos } if repos.len() == 2));
}

#[tokio::test]
async fn remove_checkout_command_accepts_selector_queries() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let err = daemon
        .execute(Command {
            host: None,
            context_repo: None,
            action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("does-not-exist".into()), terminal_keys: vec![] },
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

    assert!(matches!(result, CommandResult::CheckoutStatus(_)), "expected checkout status result via context repo, got {result:?}");
}

#[tokio::test]
async fn checkout_target_branch_and_fresh_branch_are_distinct_errors() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    let branch_id = daemon
        .execute(Command {
            host: None,
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
                    CommandResult::Error { message } => branch_err = Some(message),
                    other => panic!("expected error for Branch checkout, got {other:?}"),
                },
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == fresh_id => match result {
                    CommandResult::Error { message } => fresh_err = Some(message),
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
    let config = Arc::new(ConfigStore::new());
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
    assert!(!provider_names.contains_key("code_review"), "follower should not have code_review provider");
    assert!(!provider_names.contains_key("issue_tracker"), "follower should not have issue_tracker provider");
    // cloud_agent and ai_utility depend on Claude/Codex/Cursor being
    // installed, so they may or may not be present in non-follower mode.
    // In follower mode they should always be absent.
    assert!(!provider_names.contains_key("cloud_agent"), "follower should not have cloud_agent provider");
    assert!(!provider_names.contains_key("ai_utility"), "follower should not have ai_utility provider");
}

#[tokio::test]
async fn add_virtual_repo_emits_repo_added_and_appears_in_list() {
    let config = Arc::new(ConfigStore::new());
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();

    let synthetic_path = PathBuf::from("<remote>/desktop/home/dev/repo");
    let identity = RepoIdentity { authority: "github.com".into(), path: "owner/remote-only".into() };
    daemon
        .add_virtual_repo(identity.clone(), synthetic_path.clone(), ProviderData::default())
        .await
        .expect("add_virtual_repo should succeed");

    // Should receive a RepoAdded event
    let added = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::RepoAdded(info)) => break *info,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for RepoAdded");
    assert_eq!(added.identity, identity);
    assert_eq!(added.path, synthetic_path);
    assert!(!added.loading, "virtual repos should not be in loading state");

    // Should appear in list_repos
    let repos = daemon.list_repos().await.expect("list_repos");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].path, synthetic_path);
    assert!(!repos[0].loading);
}

#[tokio::test]
async fn add_virtual_repo_is_idempotent() {
    let config = Arc::new(ConfigStore::new());
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;

    let synthetic_path = PathBuf::from("<remote>/desktop/home/dev/repo");
    let identity = RepoIdentity { authority: "github.com".into(), path: "owner/remote-only".into() };
    daemon.add_virtual_repo(identity.clone(), synthetic_path.clone(), ProviderData::default()).await.expect("first add should succeed");

    // Second add with same path should be a no-op
    daemon
        .add_virtual_repo(identity, synthetic_path.clone(), ProviderData::default())
        .await
        .expect("second add should succeed (idempotent)");

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
    let work = daemon.get_repo_work(repo_name).await.expect("get_repo_work failed");
    assert_eq!(work.path, repo);
    // Work items may or may not be present depending on repo state, but the call should succeed
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
    daemon.refresh(&repo).await.expect("refresh should succeed");

    // --- Assert ---
    // Collect snapshot events until we see one containing issue "42".
    // The daemon may send a SnapshotFull or a SnapshotDelta depending on
    // whether the delta is smaller than the full snapshot. We accept either.
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::SnapshotFull(snap)) if snap.repo == repo => {
                    if snap.providers.issues.contains_key("42") {
                        return *snap;
                    }
                }
                Ok(DaemonEvent::SnapshotDelta(ref delta)) if delta.repo == repo => {
                    // Check if the delta contains an Issue change for "42"
                    let has_issue_42 = delta.changes.iter().any(|c| matches!(c, Change::Issue { key, .. } if key == "42"));
                    if has_issue_42 {
                        // Use replay_since to get the full snapshot with the issue
                        let events = daemon.replay_since(&HashMap::new()).await.expect("replay_since");
                        for event in events {
                            if let DaemonEvent::SnapshotFull(snap) = event {
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
