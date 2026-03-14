use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{
        ChannelLabel, CommandOutput, CommandRunner,
        discovery::{DiscoveryRuntime, EnvVars},
    },
};
use flotilla_protocol::{Command, DaemonEvent, HostName, ProviderData};

#[derive(Default)]
struct StaticEnvVars;

impl EnvVars for StaticEnvVars {
    fn get(&self, _key: &str) -> Option<String> {
        None
    }
}

struct FakeRunner;

#[async_trait]
impl CommandRunner for FakeRunner {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        if cmd == "git" && args == ["--version"] {
            return Ok("git version 2.43.0\n".into());
        }
        Err(format!("fake runner: no response for {cmd} {}", args.join(" ")))
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        match self.run(cmd, args, cwd, label).await {
            Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), success: true }),
            Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, success: false }),
        }
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

async fn daemon_for_cwd() -> (tempfile::TempDir, PathBuf, Arc<InProcessDaemon>) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let discovery = process_discovery(false);
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;
    (temp, repo, daemon)
}

fn process_discovery(follower: bool) -> DiscoveryRuntime {
    let mut runtime = DiscoveryRuntime::for_process(follower);
    runtime.runner = Arc::new(FakeRunner);
    runtime.env = Arc::new(StaticEnvVars);
    runtime
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
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    // Execute a command that goes through the spawned task path.
    // ArchiveSession with a non-existent ID returns immediately with
    // "session not found" — no external API calls, deterministic.
    // We only care about the lifecycle events, not the command result.
    let command = Command::ArchiveSession { session_id: "nonexistent-session".into() };
    let command_id = daemon.execute(&repo, command).await.expect("execute should return a command id");

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
                Ok(DaemonEvent::CommandStarted { command_id: id, repo: ref event_repo, .. }) => {
                    assert_eq!(event_repo, &repo, "CommandStarted repo should match executed repo");
                    started_id = Some(id);
                    got_started = true;
                }
                Ok(DaemonEvent::CommandFinished { command_id: id, repo: ref event_repo, .. }) => {
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
    std::fs::create_dir_all(&repo).unwrap();

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![], config, process_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();

    daemon.add_repo(&repo).await.expect("add_repo should succeed");

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
    assert_eq!(added.path, repo);

    let repos = daemon.list_repos().await.expect("list_repos after add");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].path, repo);

    daemon.remove_repo(&repo).await.expect("remove_repo should succeed");
    let removed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::RepoRemoved { path }) => break path,
                Ok(_) => {}
                Err(e) => panic!("unexpected recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for RepoRemoved");
    assert_eq!(removed, repo);

    let repos = daemon.list_repos().await.expect("list_repos after remove");
    assert!(repos.is_empty());

    let err = daemon.remove_repo(&repo).await.expect_err("removing missing repo should fail");
    assert!(err.contains("repo not tracked"));
}

#[tokio::test]
async fn inline_issue_command_returns_zero_and_skips_lifecycle_events() {
    let (_temp, repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();

    // Wait for initial snapshot event before issuing command.
    let _ = trigger_refresh_and_recv(&daemon, &repo, &mut rx).await;

    let command_id = daemon.execute(&repo, Command::ClearIssueSearch { repo: repo.clone() }).await.expect("inline command should succeed");
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
    let daemon = InProcessDaemon::new(vec![], config, process_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();
    let repo = std::path::PathBuf::from("/tmp/does-not-exist-for-daemon-test");

    let err = daemon.execute(&repo, Command::Refresh).await.expect_err("untracked repo should fail");
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
async fn follower_mode_flag_is_stored() {
    let config = Arc::new(ConfigStore::new());
    let leader = InProcessDaemon::new(vec![], config.clone(), process_discovery(false), HostName::local()).await;
    assert!(!leader.is_follower(), "default daemon should not be follower");

    let follower = InProcessDaemon::new(vec![], config, process_discovery(true), HostName::local()).await;
    assert!(follower.is_follower(), "follower daemon should report follower=true");
}

#[tokio::test]
async fn follower_mode_skips_external_providers() {
    // Use a temp dir with a .git directory to guarantee VCS detection
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".git")).unwrap();

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, process_discovery(true), HostName::local()).await;

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
    let daemon = InProcessDaemon::new(vec![], config, process_discovery(false), HostName::local()).await;
    let mut rx = daemon.subscribe();

    let synthetic_path = PathBuf::from("<remote>/desktop/home/dev/repo");
    daemon.add_virtual_repo(synthetic_path.clone(), ProviderData::default()).await.expect("add_virtual_repo should succeed");

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
    let daemon = InProcessDaemon::new(vec![], config, process_discovery(false), HostName::local()).await;

    let synthetic_path = PathBuf::from("<remote>/desktop/home/dev/repo");
    daemon.add_virtual_repo(synthetic_path.clone(), ProviderData::default()).await.expect("first add should succeed");

    // Second add with same path should be a no-op
    daemon.add_virtual_repo(synthetic_path.clone(), ProviderData::default()).await.expect("second add should succeed (idempotent)");

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
