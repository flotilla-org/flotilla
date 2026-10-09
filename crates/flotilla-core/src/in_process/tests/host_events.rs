use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use flotilla_protocol::{
    Command, CommandAction, CommandValue, DaemonEvent, EnvironmentId, HostName, HostProviderStatus, HostSummary, NodeId, NodeInfo,
    PeerConnectionState, StreamKey,
};
use flotilla_resources::{Host as ResourceHost, HostSpec, HostStatus, InMemoryBackend, ResourceBackend, AGENT_ADAPTERS_CAPABILITY};
use tokio::sync::broadcast;

use super::support::test_meta;
use crate::config::ConfigStore;
use crate::daemon::DaemonHandle;
use crate::in_process::InProcessDaemon;
use crate::providers::discovery::test_support::{
    fake_discovery, fake_discovery_with_provider_set, FakeChangeRequest, FakeDiscoveryProviders,
};

// UTC-to-monotonic conversion must preserve a future header deadline and keep
// expired/zero deadlines from causing a burst of concurrent cache misses.
#[tokio::test(start_paused = true)]
async fn observation_cache_deadline_crosses_both_clocks() {
    let wall = chrono::DateTime::parse_from_rfc3339("2026-10-03T12:00:00Z").expect("fixed UTC timestamp").with_timezone(&chrono::Utc);
    let retry = wall + chrono::Duration::seconds(60);
    let expires = tokio::time::Instant::now() + super::observation_cache_delay(Some(retry), wall);
    tokio::time::advance(std::time::Duration::from_secs(59)).await;
    assert!(tokio::time::Instant::now() < expires);
    assert_eq!(super::observation_cache_delay(Some(retry), wall + chrono::Duration::seconds(59)), std::time::Duration::from_secs(1));
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert_eq!(tokio::time::Instant::now(), expires);
    for now in [retry, retry + chrono::Duration::seconds(1)] {
        assert_eq!(super::observation_cache_delay(Some(retry), now), std::time::Duration::from_secs(9));
    }
}

// #1496 operator follow-up: host provider observations and CLI summaries are
// independent of tracked roots; the published status remains the query authority.
#[hegel::test]
fn host_provider_summary_survives_root_membership_changes(tc: hegel::TestCase) {
    use hegel::generators as gs;

    use crate::providers::discovery::test_support::FakeTerminalPool;

    // Cover no roots, multiple roots, both discovery outcomes, repeated removals,
    // and either root removal order. Factories stand in for process providers.
    let root_count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let available = tc.draw(gs::booleans());
    let reverse = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("config directory");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"host-provider-observer\"\n").expect("daemon config");
        let mut roots = (0..root_count).map(|index| temp.path().join(format!("repo-{index}"))).collect::<Vec<_>>();
        for root in &roots {
            std::fs::create_dir(root).expect("root directory");
        }
        let mut providers = FakeDiscoveryProviders::new().with_change_request(Arc::new(FakeChangeRequest::new()));
        if available {
            providers = providers.with_terminal_pool(Arc::new(FakeTerminalPool::new()));
        }
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            roots.clone(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery_with_provider_set(providers),
            HostName::new("observer"),
            backend.clone(),
        )
        .await;
        let environment = daemon.local_host_identity().environment_id;
        // Host-direct adoption is always available; terminal discovery
        // remains optional, and neither depends on repository membership.
        let mut expected = vec![HostProviderStatus {
            category: "environment_provider".into(),
            name: "host-direct".into(),
            implementation: "host-direct".into(),
            healthy: true,
            disabled_reason: None,
        }];
        if available {
            expected.push(HostProviderStatus {
                category: "terminal_pool".into(),
                name: "Fake Terminals".into(),
                implementation: "fake-terminals".into(),
                healthy: true,
                disabled_reason: None,
            });
        }
        let mut description = daemon.local_host_description().await;
        assert_eq!(description.providers, expected, "host discovery cannot depend on a tracked repository");
        assert_eq!(daemon.get_host_providers_internal(&environment).await.expect("bootstrap providers").summary.providers, expected,);

        // A stored health observation can differ from this process's discovery;
        // queries must present that status even while roots are removed.
        let published = vec![HostProviderStatus::disabled("environment_provider", "published", "offline probe")];
        description.providers = published.clone();
        let hosts = backend.using::<ResourceHost>("flotilla");
        let name = daemon.local_host_id().expect("host id").to_string();
        let host = hosts.create(&test_meta(&name), &HostSpec::default()).await.expect("Host");
        hosts
            .update_status(&name, &host.metadata.resource_version, &HostStatus { description: Some(description), ..Default::default() })
            .await
            .expect("published description");
        if reverse {
            roots.reverse();
        }
        for root in std::iter::once(None).chain(roots.iter().flat_map(|root| [Some(root), Some(root)])) {
            if let Some(root) = root {
                daemon.remove_repo(root).await.expect("remove root or repeat removal");
            }
            assert_eq!(daemon.local_host_description().await.providers, expected, "heartbeat discovery survives root removal");
            assert_eq!(
                daemon.get_host_providers_internal(&environment).await.expect("resource providers").summary.providers,
                published,
                "provider CLI reads stored health, independent of tracked roots",
            );
        }
    });
}

// #1496: resource descriptions survive link changes and observer restart; link
// state alone determines connectivity. Repeated projection must not bump cursors.
#[hegel::test]
fn resource_host_descriptions_survive_transport_changes(tc: hegel::TestCase) {
    use flotilla_protocol::qualified_path::HostId;
    use hegel::generators as gs;
    // Short generated sequences cover duplicate updates, online/offline transitions,
    // empty inventories, and description changes while the peer is disconnected.
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let operations = (0..steps).map(|_| (tc.draw(gs::booleans()), tc.draw(gs::booleans()))).collect::<Vec<_>>();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("config directory");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"host-description-observer\"\n").expect("daemon config");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let make_daemon = || {
            InProcessDaemon::new_with_resource_backend(
                Vec::new(),
                Arc::new(ConfigStore::with_base(temp.path())),
                fake_discovery(false),
                HostName::new("observer"),
                backend.clone(),
            )
        };
        let daemon = make_daemon().await;
        // A published local description is also authoritative: the host environment
        // identity differs from the daemon's direct execution environment id.
        let local_identity = daemon.local_host_identity();
        let local_environment = local_identity.environment_id.clone();
        let local_summary = HostSummary::builder()
            .environment_id(local_environment.clone())
            .node(local_identity.node)
            .system(flotilla_protocol::SystemInfo { os: Some("published-os".into()), ..Default::default() })
            .build();
        let local_hosts = backend.clone().using::<ResourceHost>("flotilla");
        let local_name = daemon.local_host_id().expect("local host").to_string();
        let local = local_hosts.create(&test_meta(&local_name), &HostSpec::default()).await.expect("local Host");
        local_hosts
            .update_status(
                &local_name,
                &local.metadata.resource_version,
                &HostStatus { description: Some(local_summary.clone()), heartbeat_at: Some(Utc::now()), ..Default::default() },
            )
            .await
            .expect("publish local description");
        let local = daemon.get_host_status_internal(&local_environment).await.expect("local resource description");
        assert_eq!(local.summary, Some(local_summary));
        let remote = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("remote-root"));
        let hosts = remote.using::<ResourceHost>("flotilla");
        hosts.create(&test_meta("remote-host"), &HostSpec::default()).await.expect("host");
        let writer = backend.replica_writer::<ResourceHost>(NodeId::new("remote-root"), "flotilla");
        let environment_id = EnvironmentId::host(HostId::new("remote-host"));
        let node = NodeInfo::new(NodeId::new("remote-node"), "remote");
        let mut expected = None;
        for (step, (connected, with_inventory)) in operations.into_iter().enumerate() {
            let summary = HostSummary::builder()
                .environment_id(environment_id.clone())
                .host_name(HostName::new("remote"))
                .node(node.clone())
                .system(flotilla_protocol::SystemInfo { cpu_count: Some(step as u16), ..Default::default() })
                .inventory(flotilla_protocol::ToolInventory {
                    binaries: if with_inventory {
                        vec![flotilla_protocol::DiscoveryFact { name: "git".into(), detail: vec![] }]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                })
                .providers(vec![HostProviderStatus::disabled("vcs", "git", "probe failed")])
                .environments(vec![flotilla_protocol::EnvironmentInfo::Direct {
                    id: environment_id.clone(),
                    host_id: Some(HostId::new("remote-host")),
                    display_name: None,
                    status: flotilla_protocol::EnvironmentStatus::Running,
                }])
                .build();
            let host = hosts.get("remote-host").await.expect("host");
            let status = flotilla_resources::HostStatus {
                description: Some(summary),
                heartbeat_at: Some(Utc::now()),
                blob_sync: Some(flotilla_protocol::BlobSyncStatus { pending_count: step, last_error: None }),
                capabilities: BTreeMap::from([(AGENT_ADAPTERS_CAPABILITY.into(), serde_json::json!(["codex"]))]),
                ..Default::default()
            };
            let expected_environments = status.description.as_ref().expect("description").environments.clone();
            expected = status.host_summary();
            hosts.update_status("remote-host", &host.metadata.resource_version, &status).await.expect("publish description");
            writer.replace(&hosts.list().await.expect("list"), Utc::now()).await.expect("replicate");
            let identity = flotilla_protocol::HostIdentity {
                environment_id: environment_id.clone(),
                host_name: Some(HostName::new("live-name")),
                node: NodeInfo::new(node.node_id.clone(), "live-name"),
            };
            daemon.set_peer_host_identities(HashMap::from([(environment_id.clone(), identity.clone())])).await;
            let connectivity = if connected { PeerConnectionState::Connected } else { PeerConnectionState::Disconnected };
            daemon.publish_peer_connection_status(&node, connectivity.clone()).await;
            if !connected {
                daemon.set_peer_host_identities(HashMap::new()).await;
            }
            let mut presented = expected.clone().expect("description");
            if connected {
                presented.node = identity.node;
                presented.host_name = identity.host_name;
            }
            let response = daemon.get_host_status_internal(&environment_id).await.expect("resource backed host");
            assert_eq!(response.summary, Some(presented.clone()), "live transport identity wins over resource identity");
            assert_eq!(response.connection_status, connectivity);
            assert_eq!(response.blob_sync, status.blob_sync);
            let providers = daemon.get_host_providers_internal(&environment_id).await.expect("providers");
            assert_eq!(providers.summary, presented);
            assert_eq!(providers.visible_environments, expected_environments);
            assert_eq!(response.visible_environments, expected_environments);
            let replay = daemon.replay_since(&HashMap::new()).await.expect("replay");
            let seq = replay
                .iter()
                .find_map(|event| match event {
                    DaemonEvent::HostSnapshot(snapshot) if snapshot.environment_id == environment_id => Some(snapshot.seq),
                    _ => None,
                })
                .expect("snapshot");
            let replay = daemon
                .replay_since(&HashMap::from([(StreamKey::Host { environment_id: environment_id.clone() }, seq)]))
                .await
                .expect("duplicate projection");
            assert!(!replay
                .iter()
                .any(|event| matches!(event, DaemonEvent::HostSnapshot(snapshot) if snapshot.environment_id == environment_id)));
        }
        drop(daemon);
        let restarted = make_daemon().await;
        let response = restarted.get_host_status_internal(&environment_id).await.expect("offline replica after restart");
        assert_eq!(response.summary, expected);
        assert_eq!(response.connection_status, PeerConnectionState::Disconnected);
        let listed = restarted.list_hosts_internal().await.expect("hosts");
        assert!(listed.hosts.iter().any(|host| host.environment_id.as_ref() == Some(&environment_id) && host.has_summary));
        // Deleting one resource must rehome the node's environment mapping to
        // another described environment, rather than leaving a removed mapping.
        let original = hosts.get("remote-host").await.expect("remote host").status.expect("status");
        let alternate_environment = EnvironmentId::host(HostId::new("remote-alternate"));
        let alternate = hosts.create(&test_meta("remote-alternate"), &HostSpec::default()).await.expect("alternate Host");
        let mut alternate_status = original.clone();
        alternate_status.description.as_mut().expect("description").environment_id = alternate_environment.clone();
        hosts.update_status("remote-alternate", &alternate.metadata.resource_version, &alternate_status).await.expect("alternate status");
        writer.replace(&hosts.list().await.expect("two hosts"), Utc::now()).await.expect("replicate alternate");
        restarted.refresh_resource_host_summaries().await.expect("project alternate");
        hosts.delete("remote-host").await.expect("delete host");
        writer.replace(&hosts.list().await.expect("remaining host"), Utc::now()).await.expect("replicate deletion");
        assert!(restarted.get_host_status_internal(&environment_id).await.is_err());
        assert_eq!(restarted.host_registry.environment_id_for_node(&node.node_id).await, Some(alternate_environment.clone()));
        hosts.delete("remote-alternate").await.expect("delete alternate");
        writer.replace(&hosts.list().await.expect("empty list"), Utc::now()).await.expect("replicate final deletion");
        restarted.refresh_resource_host_summaries().await.expect("project deletion");
        assert_eq!(restarted.host_registry.environment_id_for_node(&node.node_id).await, None);

        // A live identity keeps its identity-only presentation when a Host loses
        // its description or is deleted. No description is invented from link state.
        let created = hosts.create(&test_meta("remote-host"), &HostSpec::default()).await.expect("recreate Host");
        hosts.update_status("remote-host", &created.metadata.resource_version, &original).await.expect("restore description");
        writer.replace(&hosts.list().await.expect("restored list"), Utc::now()).await.expect("replicate restored Host");
        let identity = flotilla_protocol::HostIdentity {
            environment_id: environment_id.clone(),
            host_name: Some(HostName::new("live-name")),
            node: NodeInfo::new(node.node_id.clone(), "live-name"),
        };
        restarted.set_peer_host_identities(HashMap::from([(environment_id.clone(), identity.clone())])).await;
        restarted.publish_peer_connection_status(&node, PeerConnectionState::Connected).await;
        let current = hosts.get("remote-host").await.expect("current Host");
        let empty_status =
            HostStatus { blob_sync: Some(flotilla_protocol::BlobSyncStatus { pending_count: 7, last_error: None }), ..Default::default() };
        hosts.update_status("remote-host", &current.metadata.resource_version, &empty_status).await.expect("clear description");
        writer.replace(&hosts.list().await.expect("empty status"), Utc::now()).await.expect("replicate empty status");
        for deleted in [false, true] {
            if deleted {
                hosts.delete("remote-host").await.expect("delete connected Host");
                writer.replace(&hosts.list().await.expect("empty list"), Utc::now()).await.expect("replicate deletion");
            }
            let response = restarted.get_host_status_internal(&environment_id).await.expect("live identity survives missing description");
            assert_eq!(response.summary, Some(identity.clone().into()));
            assert_eq!(response.connection_status, PeerConnectionState::Connected);
            assert!(response.visible_environments.is_empty());
            assert_eq!(response.blob_sync, if deleted { None } else { empty_status.blob_sync.clone() });
        }
        restarted.set_peer_host_identities(HashMap::new()).await;
        assert!(restarted.get_host_status_internal(&environment_id).await.is_err());
    });
}

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
        ResourceBackend::InMemory(InMemoryBackend::default()),
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
        ResourceBackend::InMemory(InMemoryBackend::default()),
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
