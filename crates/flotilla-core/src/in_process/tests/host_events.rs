use flotilla_store_memory::InMemoryBackend;
use std::sync::Arc;
use std::time::Duration;

use flotilla_config::ConfigStore;
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, HostName};
use flotilla_store::ResourceBackend;
use tokio::sync::broadcast;

use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;
use flotilla_daemon_api::daemon::DaemonHandle;

// Behaviour (#2255): context-free command lifecycle and intervening publications
// reach all subscribers in call order, with their original payloads intact.
// Glue: one call-through sequence covers these synchronous composition-root methods.
#[tokio::test]
async fn event_publication_preserves_context_free_command_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"event-publication-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        ResourceBackend::new(InMemoryBackend::default()),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let mut first = daemon.subscribe();
    let mut second = daemon.subscribe();
    let identity = daemon.start_context_free_command(42, "test publication".into());
    daemon.send_event(DaemonEvent::RepoUntracked { repo_identity: identity.clone(), path: None });
    daemon.finish_context_free_command(42, identity.clone(), CommandValue::Error { message: "expected error".into() });
    for receiver in [&mut first, &mut second] {
        assert!(matches!(receiver.try_recv().expect("started"), DaemonEvent::CommandStarted {
            command_id: 42, repo_identity, repo: None, description, ..
        } if repo_identity == identity && description == "test publication"));
        assert!(matches!(receiver.try_recv().expect("intervening event"), DaemonEvent::RepoUntracked {
            repo_identity, path: None
        } if repo_identity == identity));
        assert!(matches!(receiver.try_recv().expect("finished"), DaemonEvent::CommandFinished {
            command_id: 42, repo_identity, repo: None, result: CommandValue::Error { message }, ..
        } if repo_identity == identity && message == "expected error"));
        assert!(matches!(receiver.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    }
}

// Behaviour (#2255): a spawned watch publishes start before initial/progress
// events and cancellation finish after them, on the same event bus.
#[tokio::test]
async fn event_publication_preserves_spawned_watch_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"event-publication-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        ResourceBackend::new(InMemoryBackend::default()),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let mut events = daemon.subscribe();
    let id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ResourceWatch {
                    namespace: "flotilla".into(),
                    kind: "convoy".into(),
                    name: None,
                    include_replicas: false,
                    replica_sources: false,
                    cursor: None,
                })
                .build(),
        )
        .await
        .expect("start watch");
    tokio::time::timeout(Duration::from_secs(2), async {
        assert!(
            matches!(events.recv().await.expect("started"), DaemonEvent::CommandStarted { command_id, repo: None, .. } if command_id == id)
        );
        for _ in 0..2 {
            assert!(matches!(events.recv().await.expect("watch progress"), DaemonEvent::CommandStepUpdate {
                command_id, repo: None, status: flotilla_protocol::StepStatus::Produced { value }, ..
            } if command_id == id && matches!(*value, CommandValue::ResourceWatchEvent(_))));
        }
        daemon.cancel(id).await.expect("cancel watch");
        assert!(matches!(events.recv().await.expect("finished"), DaemonEvent::CommandFinished {
            command_id, repo: None, result: CommandValue::Cancelled, ..
        } if command_id == id));
        assert!(matches!(events.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    })
    .await
    .expect("watch lifecycle completes");
}
