use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_core::{
    config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery,
};
use flotilla_protocol::{Command, CommandAction, CommandValue, ConvoyAutoAttach, ConvoyStartIntent, DaemonEvent, HostName};
use flotilla_resources::{
    single_agent_trusted_workflow_spec, Convoy, CrewSource, HttpBackend, InMemoryBackend, InputMeta, Project, ProjectSpec, ResourceBackend,
    ResourceObject, ResourceProvenance, SqliteBackend, WorkflowTemplate, WorkflowTemplateSpec, WORKFLOW_SNAPSHOT_ANNOTATION,
};
use flotilla_test_support::TestSocketDir;
use tokio::{io::AsyncReadExt, net::UnixListener, task::JoinSet};

use super::{replicate_kind_over_http, replicate_relay_over_http};
use crate::server::resource_http::serve_resource_http;

async fn await_template(daemon: &InProcessDaemon, name: &str, expected: &WorkflowTemplateSpec) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if daemon
                .resource_backend()
                .definitions::<WorkflowTemplate>("flotilla")
                .get(name)
                .await
                .is_ok_and(|object| object.spec == *expected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("feta template must converge through kiwi to udder within seconds");
}

async fn admit_governor(daemon: &InProcessDaemon) -> CommandValue {
    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(
                        ConvoyStartIntent::builder()
                            .project_ref("andamento".to_string())
                            .name("governor".to_string())
                            .branch("governor".to_string())
                            .workflow_ref("governor".to_string())
                            .auto_attach(ConvoyAutoAttach::Never)
                            .build(),
                    ),
                })
                .build(),
        )
        .await
        .expect("submit governor admission");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonEvent::CommandFinished { command_id: id, result, .. } = events.recv().await.expect("admission event") {
                if id == command_id {
                    return result;
                }
            }
        }
    })
    .await
    .expect("admission completes")
}

async fn governor_snapshot(daemon: &InProcessDaemon) -> (ResourceObject<Convoy>, WorkflowTemplateSpec) {
    let backend = daemon.resource_backend();
    let convoys = backend.using::<Convoy>("flotilla").list().await.expect("list admitted convoys");
    assert_eq!(convoys.items.len(), 1);
    let convoy = convoys.items.into_iter().next().expect("admitted governor");
    let snapshot = backend
        .definitions::<WorkflowTemplate>("flotilla")
        .get(&convoy.metadata.annotations[WORKFLOW_SNAPSHOT_ANNOTATION])
        .await
        .expect("admission's immutable workflow snapshot");
    (convoy, snapshot.spec)
}

#[tokio::test]
async fn in_memory_http_relays_governor_template_across_three_roots() {
    governor_relay_contract(TestBackend::InMemory).await;
}

#[tokio::test]
async fn sqlite_http_relays_governor_template_across_three_roots() {
    governor_relay_contract(TestBackend::Sqlite).await;
}

enum TestBackend {
    InMemory,
    Sqlite,
}

async fn governor_relay_contract(storage: TestBackend) {
    let temp = tempfile::tempdir().expect("temporary stores");
    let sockets = TestSocketDir::new();
    let mut roots = Vec::new();
    let mut tasks = JoinSet::new();
    for host in ["feta", "kiwi", "udder"] {
        let config_path = temp.path().join(host);
        std::fs::create_dir_all(&config_path).expect("config directory");
        std::fs::write(config_path.join("daemon.toml"), format!("machine_id = \"{host}\"\n[admission]\nfree_space_floor_gib = 0\n"))
            .expect("machine identity");
        let config = Arc::new(ConfigStore::with_base(config_path));
        let backend = match storage {
            TestBackend::Sqlite => {
                ResourceBackend::Sqlite(SqliteBackend::open(temp.path().join(format!("{host}.sqlite"))).expect("open store"))
            }
            TestBackend::InMemory => ResourceBackend::InMemory(InMemoryBackend::default()),
        };
        let daemon = InProcessDaemon::new_with_resource_backend(vec![], config, fake_discovery(false), HostName::new(host), backend).await;
        let path = sockets.socket_path(&format!("{host}.sock"));
        let listener = UnixListener::bind(&path).expect("bind resource API");
        let backend = daemon.resource_backend();
        tasks.spawn(async move {
            let mut requests = JoinSet::new();
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept resource request");
                let backend = backend.clone();
                requests.spawn(async move {
                    if let Ok(first) = stream.read_u8().await {
                        // Client cancellation can close a watch mid-write.
                        let _ = serve_resource_http(stream, first, backend).await;
                    }
                });
            }
        });
        roots.push((daemon, path));
    }
    let feta = &roots[0].0;
    let kiwi = &roots[1].0;
    let udder = &roots[2].0;
    udder
        .resource_backend()
        .definitions::<Project>("flotilla")
        .apply(
            &InputMeta::builder().name("andamento".to_string()).build(),
            &ProjectSpec::builder().display_name("Andamento".to_string()).default_workflow_ref("governor".to_string()).build(),
        )
        .await
        .expect("create admission project");
    assert!(matches!(admit_governor(udder).await, CommandValue::Error { message } if message.contains("workflow template governor")));

    let mut workflow = single_agent_trusted_workflow_spec();
    workflow.exit = None;
    // No processes are launched: this tests admission independently of adapters.
    workflow.vessels[0].crew[0].source = CrewSource::Tool { command: "true".to_string() };
    let templates = feta.resource_backend().definitions::<WorkflowTemplate>("flotilla");
    let metadata = InputMeta::builder().name("governor".to_string()).build();
    templates.apply(&metadata, &workflow).await.expect("author feta governor");

    // Copied manifests can retain read-view annotations. A local source must
    // not poison the relay stream for unrelated third-origin definitions.
    kiwi.resource_backend()
        .definitions::<WorkflowTemplate>("flotilla")
        .apply(
            &InputMeta::builder()
                .name("a-local-template".to_string())
                .annotations(BTreeMap::from([("flotilla.work/origin-root".to_string(), "old-root".to_string())]))
                .build(),
            &workflow,
        )
        .await
        .expect("author local template with retained annotation");

    // Only A--B and B--C exist. Both directions use production HTTP code;
    // neither the routed test transport nor a direct A--C edge can mask bugs.
    for (holder, source) in [(1, 0), (0, 1), (2, 1), (1, 2)] {
        let daemon = Arc::clone(&roots[holder].0);
        let peer = roots[source].0.node_id().clone();
        let path = roots[source].1.clone();
        tasks.spawn(async move {
            let direct =
                replicate_kind_over_http::<WorkflowTemplate>(HttpBackend::from_unix_socket(&path).expect("HTTP client"), &daemon, &peer);
            let relay =
                replicate_relay_over_http::<WorkflowTemplate>(HttpBackend::from_unix_socket(&path).expect("HTTP client"), &daemon, &peer);
            tokio::select! {
                result = direct => panic!("direct replication ended: {result:?}"),
                result = relay => panic!("relay ended: {result:?}"),
            }
        });
    }
    for root in [kiwi, udder] {
        await_template(root, "governor", &workflow).await;
        let backend = root.resource_backend();
        assert!(backend.using::<WorkflowTemplate>("flotilla").get("governor").await.is_err(), "relay must not author locally");
        let sources = backend.including_replicas::<WorkflowTemplate>("flotilla").list_replica_sources().await.expect("raw sources");
        let source = sources.items.iter().find(|source| source.object.metadata.name == "governor").expect("governor replica");
        assert!(matches!(&source.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == feta.node_id()));
    }
    assert!(matches!(admit_governor(udder).await, CommandValue::ConvoyStarted { .. }));
    let (first_convoy, first_snapshot) = governor_snapshot(udder).await;
    assert_eq!(first_snapshot, workflow);

    workflow.vessels[0].crew[0].source = CrewSource::Tool { command: "echo revised-governor".to_string() };
    templates.apply(&metadata, &workflow).await.expect("revise feta template during live watches");
    for root in [kiwi, udder] {
        await_template(root, "governor", &workflow).await;
    }
    assert_eq!(governor_snapshot(udder).await.1, first_snapshot, "relay must not rewrite an admitted snapshot");
    udder.resource_backend().using::<Convoy>("flotilla").delete(&first_convoy.metadata.name).await.expect("retire first admission");
    let result = admit_governor(udder).await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "re-admission failed: {result:?}");
    assert_eq!(governor_snapshot(udder).await.1, workflow, "re-admission must capture the newly relayed merge view");

    // Also cover a record first authored after all watches are already live.
    templates.apply(&InputMeta::builder().name("late-governor".to_string()).build(), &workflow).await.expect("author late template");
    await_template(udder, "late-governor", &workflow).await;
}
