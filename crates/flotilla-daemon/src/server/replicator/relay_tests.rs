use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_core::config::ConfigStore;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::{Command, CommandAction, CommandValue, ConvoyAutoAttach, ConvoyStartIntent, DaemonEvent, HostName};
use flotilla_resources::{
    single_agent_workflow_spec, Convoy, CrewSource, HttpBackend, InMemoryBackend, InputMeta, Project, ProjectSpec, ResourceBackend,
    ResourceObject, ResourceProvenance, SqliteBackend, WorkflowTemplate, WorkflowTemplateSpec, WORKFLOW_SNAPSHOT_ANNOTATION,
};
use flotilla_test_support::TestSocketDir;
use tokio::{io::AsyncReadExt, net::UnixListener, task::JoinSet};

use super::{replicate_kind_over_http, replicate_relay_over_http, ReplicationStore};
use crate::server::resource_http::{serve_resource_http, serve_resource_http_with_daemon};

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

    let mut workflow = single_agent_workflow_spec();
    workflow.exit = None;
    // No processes are launched: this tests admission independently of adapters.
    workflow.vessels[0].crew[0].source = CrewSource::Tool { command: "true".to_string() };
    workflow.turn_delivery.clear();
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
            let direct = replicate_kind_over_http::<WorkflowTemplate>(
                HttpBackend::from_unix_socket(&path).expect("HTTP client"),
                &daemon,
                &peer,
                ReplicationStore::Durable,
            );
            let relay = replicate_relay_over_http::<WorkflowTemplate>(
                HttpBackend::from_unix_socket(&path).expect("HTTP client"),
                &daemon,
                &peer,
                ReplicationStore::Durable,
            );
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
    let admitted = admit_governor(udder).await;
    assert!(matches!(admitted, CommandValue::ConvoyStarted { .. }), "{admitted:?}");
    let (first_convoy, first_snapshot) = governor_snapshot(udder).await;
    assert_eq!(first_snapshot.allocation.len(), 1);
    let mut expected_snapshot = workflow.clone();
    expected_snapshot.allocation = first_snapshot.allocation.clone();
    // Admission freezes local cascade inputs alongside the relayed workflow.
    let cascade = first_snapshot.cascade.as_ref().expect("frozen cascade");
    assert_eq!(cascade.project_chain, ["andamento"]);
    assert_eq!(cascade.settings["workflow"].value, "governor");
    assert_eq!(cascade.settings["workflow"].layer, "dispatch");
    expected_snapshot.cascade = first_snapshot.cascade.clone();
    assert_eq!(first_snapshot, expected_snapshot);

    workflow.vessels[0].crew[0].source = CrewSource::Tool { command: "echo revised-governor".to_string() };
    templates.apply(&metadata, &workflow).await.expect("revise feta template during live watches");
    for root in [kiwi, udder] {
        await_template(root, "governor", &workflow).await;
    }
    assert_eq!(governor_snapshot(udder).await.1, first_snapshot, "relay must not rewrite an admitted snapshot");
    udder.resource_backend().using::<Convoy>("flotilla").delete(&first_convoy.metadata.name).await.expect("retire first admission");
    let result = admit_governor(udder).await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "re-admission failed: {result:?}");
    expected_snapshot = workflow.clone();
    expected_snapshot.cascade = first_snapshot.cascade.clone();
    expected_snapshot.allocation = first_snapshot.allocation;
    assert_eq!(governor_snapshot(udder).await.1, expected_snapshot, "re-admission must capture the newly relayed merge view");

    // Also cover a record first authored after all watches are already live.
    templates.apply(&InputMeta::builder().name("late-governor".to_string()).build(), &workflow).await.expect("author late template");
    await_template(udder, "late-governor", &workflow).await;
}

// #742: observation replication uses the production HTTP path, retains its
// origin across a relay, and removes deleted facts without persisting them.
#[tokio::test]
async fn observed_checkout_http_relay_keeps_ephemeral_origin_and_deletes() {
    use flotilla_resources::{Checkout, CheckoutSpec, ObservedCheckoutSpec, RepositoryKey};

    let temp = tempfile::tempdir().expect("temporary configs");
    let sockets = TestSocketDir::new();
    let mut roots = Vec::new();
    let mut tasks = JoinSet::new();
    for host in ["feta", "kiwi", "udder"] {
        let config_path = temp.path().join(host);
        std::fs::create_dir_all(&config_path).expect("config directory");
        std::fs::write(config_path.join("daemon.toml"), format!("machine_id = \"{host}\"\n")).expect("machine identity");
        let daemon =
            InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(config_path)), fake_discovery(false), HostName::new(host)).await;
        let path = sockets.socket_path(&format!("{host}.sock"));
        let listener = UnixListener::bind(&path).expect("bind resource API");
        let server_daemon = Arc::clone(&daemon);
        tasks.spawn(async move {
            let mut requests = JoinSet::new();
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept resource request");
                let daemon = Arc::clone(&server_daemon);
                requests.spawn(async move {
                    if let Ok(first) = stream.read_u8().await {
                        let _ = serve_resource_http_with_daemon(stream, first, daemon.resource_backend(), Some(daemon)).await;
                    }
                });
            }
        });
        roots.push((daemon, path));
    }
    let origin = &roots[0].0;
    let checkouts = origin.observed_resource_backend().using::<Checkout>("flotilla");
    let spec = CheckoutSpec::Observed(ObservedCheckoutSpec {
        path: "/srv/widgets".into(),
        r#ref: "main".into(),
        repo_ref: RepositoryKey("widgets".into()),
        host_ref: "feta".into(),
        is_main: true,
    });
    checkouts.create(&InputMeta::builder().name("remote-checkout".to_string()).build(), &spec).await.expect("publish checkout");
    for (holder, source) in [(1, 0), (2, 1)] {
        let daemon = Arc::clone(&roots[holder].0);
        let peer = roots[source].0.node_id().clone();
        let path = roots[source].1.clone();
        tasks.spawn(async move {
            let direct = replicate_kind_over_http::<Checkout>(
                HttpBackend::from_unix_socket(&path).expect("HTTP client").with_path_prefix("observed"),
                &daemon,
                &peer,
                ReplicationStore::Observed,
            );
            let relay = replicate_relay_over_http::<Checkout>(
                HttpBackend::from_unix_socket(&path).expect("HTTP client").with_path_prefix("observed"),
                &daemon,
                &peer,
                ReplicationStore::Observed,
            );
            tokio::select! {
                result = direct => panic!("direct observation replication ended: {result:?}"),
                result = relay => panic!("observation relay ended: {result:?}"),
            }
        });
    }
    let consumer = &roots[2].0;
    let reads = consumer.observed_resource_backend().including_replicas::<Checkout>("flotilla");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = reads.list().await.expect("observation replicas");
            if let Some(source) = rows.items.first() {
                assert_eq!(source.object.spec, spec);
                assert!(matches!(&source.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == origin.node_id()));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observation reaches third host");
    assert!(consumer
        .resource_backend()
        .including_replicas::<Checkout>("flotilla")
        .list()
        .await
        .expect("durable checkouts")
        .items
        .is_empty());
    checkouts.delete("remote-checkout").await.expect("delete observation");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if reads.list().await.expect("observation replicas after delete").items.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observation delete reaches third host");
}

// HTTP digest requests use the same authoritative handler as production. A
// percent-encoded query or read-view listing must not accidentally replace it.
#[tokio::test]
async fn http_digest_drill_down_reads_only_the_requested_partition() {
    use flotilla_resources::{digest_bucket, DigestQuery};
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let local = backend.using::<WorkflowTemplate>("flotilla");
    local.create(&InputMeta::builder().name("retained".into()).build(), &single_agent_workflow_spec()).await.expect("create");
    let sockets = TestSocketDir::new();
    let path = sockets.socket_path("digest.sock");
    let listener = UnixListener::bind(&path).expect("bind resource API");
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.expect("accept digest request");
            let first = stream.read_u8().await.expect("request byte");
            serve_resource_http(stream, first, backend.clone()).await.expect("serve digest");
        }
    });
    let remote = ResourceBackend::Http(HttpBackend::from_unix_socket(&path).expect("HTTP client")).using::<WorkflowTemplate>("flotilla");
    let root = remote.digest(&DigestQuery::Root).await.expect("remote root");
    assert!(root.items.is_none() && root.children.is_none(), "roots transfer no bodies or child hashes");
    let children = remote.digest(&DigestQuery::Children { expected_root: root.root.clone() }).await.expect("remote children");
    children.validate_tree::<WorkflowTemplate>().expect("complete hierarchy");
    let bucket = digest_bucket("retained");
    let snapshot = remote.digest(&DigestQuery::Snapshot { expected_root: root.root.clone(), bucket }).await.expect("remote bucket");
    let listed = snapshot.snapshot::<WorkflowTemplate>(&children, bucket).expect("validated bucket");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].metadata.name, "retained");
    server.await.expect("server");
}
