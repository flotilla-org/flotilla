use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_core::{config::ConfigStore, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery};
use flotilla_daemon::server::test_support::spawn_in_memory_request_topology;
use flotilla_protocol::HostName;
use flotilla_resources::{
    watch_resource_kind_replica_sources, Convoy, ConvoySpec, InMemoryBackend, InputMeta, Project, ProjectSpec, ResourceBackend,
    ResourceProvenance, TerminalSession, TerminalSessionSource, TerminalSessionSpec, Vessel, VesselSpec,
};
use futures::StreamExt;

fn config(path: std::path::PathBuf, machine_id: &str) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&path).expect("create config");
    std::fs::write(path.join("daemon.toml"), format!("machine_id = \"{machine_id}\"\n")).expect("write daemon config");
    Arc::new(ConfigStore::with_base(path))
}

async fn daemon(path: std::path::PathBuf, machine_id: &str, host: &str) -> Arc<InProcessDaemon> {
    InProcessDaemon::new_with_resource_backend(
        vec![],
        config(path, machine_id),
        fake_discovery(false),
        HostName::new(host),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await
}

#[tokio::test]
async fn connected_daemons_replicate_home_bound_runtime_kinds_and_deletes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let remote = feta.resource_backend();
    remote
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder().name("remote-convoy".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create remote convoy");
    remote
        .using::<Vessel>("flotilla")
        .create(&InputMeta::builder().name("remote-vessel".to_string()).build(), &VesselSpec {
            convoy_ref: "remote-convoy".to_string(),
            vessel_name: "work".to_string(),
            placement_policy_ref: "host-direct".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("create remote vessel");
    remote
        .using::<TerminalSession>("flotilla")
        .create(
            &InputMeta::builder().name("remote-session".to_string()).build(),
            &TerminalSessionSpec::builder()
                .env_ref("env".to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Tool { command: "true".to_string() })
                .cwd("/tmp".to_string())
                .pool("passthrough".to_string())
                .build(),
        )
        .await
        .expect("create remote terminal session");

    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect topology");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let convoys = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").list().await.expect("list convoy replicas");
            let vessels = kiwi.resource_backend().including_replicas::<Vessel>("flotilla").list().await.expect("list vessel replicas");
            let sessions =
                kiwi.resource_backend().including_replicas::<TerminalSession>("flotilla").list().await.expect("list session replicas");
            if convoys.items.len() == 1 && vessels.items.len() == 1 && sessions.items.len() == 1 {
                assert!(matches!(convoys.items[0].provenance, ResourceProvenance::Replica { .. }));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replication timed out");
    assert!(
        kiwi.resource_backend().using::<Convoy>("flotilla").list().await.expect("list local convoys").items.is_empty(),
        "default resource reads must remain local-only"
    );

    remote.using::<Convoy>("flotilla").delete("remote-convoy").await.expect("delete remote convoy");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if kiwi
                .resource_backend()
                .including_replicas::<Convoy>("flotilla")
                .list()
                .await
                .expect("list convoy replicas after delete")
                .items
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replicated delete timed out");

    remote
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder().name("last-known-convoy".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create last-known remote convoy");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if kiwi
                .resource_backend()
                .including_replicas::<Convoy>("flotilla")
                .list()
                .await
                .expect("list last-known convoy")
                .items
                .iter()
                .any(|item| item.object.metadata.name == "last-known-convoy")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("last-known replication timed out");

    let synced_before_disconnect = match &kiwi
        .resource_backend()
        .including_replicas::<Convoy>("flotilla")
        .list()
        .await
        .expect("list replicas before disconnect")
        .items
        .iter()
        .find(|item| item.object.metadata.name == "last-known-convoy")
        .expect("last-known replica before disconnect")
        .provenance
    {
        ResourceProvenance::Replica { last_synced_at, .. } => *last_synced_at,
        ResourceProvenance::Local => panic!("expected replica provenance"),
    };

    drop(topology);
    let disconnected = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").list().await.expect("list disconnected replicas");
    assert!(
        disconnected.items.iter().any(|item| item.object.metadata.name == "last-known-convoy"),
        "origin absence must not remove last-known replica rows"
    );

    remote
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder().name("created-offline".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create convoy while disconnected");
    let reconnected = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("reconnect topology");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let listed = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").list().await.expect("list reconnected replicas");
            if listed.items.iter().any(|item| item.object.metadata.name == "created-offline") {
                let retained = listed
                    .items
                    .iter()
                    .find(|item| item.object.metadata.name == "last-known-convoy")
                    .expect("retained replica after reconnect");
                assert!(
                    matches!(
                        retained.provenance,
                        ResourceProvenance::Replica { last_synced_at, .. }
                            if last_synced_at == synced_before_disconnect
                    ),
                    "resume must not full-relist and restamp unchanged rows"
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconnect replication timed out");
    drop(reconnected);
}

fn project_spec(display_name: &str, workflow: &str) -> ProjectSpec {
    ProjectSpec::builder()
        .display_name(display_name.to_string())
        .default_workflow_ref(workflow.to_string())
        .repositories(Vec::new())
        .build()
}

#[tokio::test]
async fn connected_daemons_causally_merge_project_definitions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let meta = InputMeta::builder().name("widgets".to_string()).build();
    kiwi.resource_backend()
        .definitions::<Project>("flotilla")
        .apply(&meta, &project_spec("Widgets", "default"))
        .await
        .expect("create Project on kiwi");

    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect topology");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if feta
                .resource_backend()
                .definitions::<Project>("flotilla")
                .get("widgets")
                .await
                .is_ok_and(|project| project.spec.display_name == "Widgets")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial Project replication timed out");
    drop(topology);

    kiwi.resource_backend()
        .definitions::<Project>("flotilla")
        .apply(&meta, &project_spec("Kiwi Widgets", "default"))
        .await
        .expect("offline edit on kiwi");
    feta.resource_backend()
        .definitions::<Project>("flotilla")
        .apply(&meta, &project_spec("Feta Widgets", "feta-flow"))
        .await
        .expect("offline edit on feta");

    let reconnected = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("reconnect topology");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let kiwi_project = kiwi.resource_backend().definitions::<Project>("flotilla").get("widgets").await;
            let feta_project = feta.resource_backend().definitions::<Project>("flotilla").get("widgets").await;
            if let (Ok(kiwi_project), Ok(feta_project)) = (kiwi_project, feta_project) {
                let kiwi_merge = kiwi_project.metadata.merge.as_ref().expect("kiwi merge metadata");
                let feta_merge = feta_project.metadata.merge.as_ref().expect("feta merge metadata");
                if kiwi_project.spec.default_workflow_ref == "feta-flow"
                    && feta_project.spec.default_workflow_ref == "feta-flow"
                    && kiwi_merge.conflicts.contains_key("spec.display_name")
                    && feta_merge.conflicts.contains_key("spec.display_name")
                {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("concurrent Project replication timed out");
    let listed = kiwi.list_projects_internal().await.expect("list conflicted Projects");
    let widgets = listed.projects.iter().find(|project| project.name == "widgets").expect("widgets Project in list");
    assert_eq!(widgets.conflicts, vec!["spec.display_name"], "Project list should expose a compact conflict badge");

    kiwi.resource_backend()
        .definitions::<Project>("flotilla")
        .apply(&meta, &project_spec("Resolved Widgets", "feta-flow"))
        .await
        .expect("resolve Project conflict");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let resolved = feta.resource_backend().definitions::<Project>("flotilla").get("widgets").await;
            if resolved.is_ok_and(|project| {
                project.spec.display_name == "Resolved Widgets" && project.metadata.merge.is_some_and(|merge| merge.conflicts.is_empty())
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Project conflict resolution replication timed out");
    drop(reconnected);
}

#[tokio::test]
async fn a_peer_relays_another_origins_project_definition() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let gouda = daemon(temp.path().join("gouda"), "gouda-root", "gouda").await;
    kiwi.resource_backend()
        .definitions::<Project>("flotilla")
        .apply(&InputMeta::builder().name("widgets".to_string()).build(), &project_spec("Widgets", "default"))
        .await
        .expect("create Project on kiwi");

    let kiwi_feta = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect kiwi and feta");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if feta.resource_backend().definitions::<Project>("flotilla").get("widgets").await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("kiwi-to-feta replication timed out");
    drop(kiwi_feta);

    let feta_gouda = spawn_in_memory_request_topology(Arc::clone(&feta), Arc::clone(&gouda)).await.expect("connect feta and gouda");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if gouda
                .resource_backend()
                .definitions::<Project>("flotilla")
                .get("widgets")
                .await
                .is_ok_and(|project| project.spec.display_name == "Widgets")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("feta did not relay kiwi Project to gouda");
    assert!(
        gouda.resource_backend().using::<Project>("flotilla").list().await.expect("gouda local Project log").items.is_empty(),
        "relay must preserve the kiwi origin instead of re-authoring on gouda"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut raw_project_watch = watch_resource_kind_replica_sources(&feta.resource_backend(), "flotilla", "projects")
        .await
        .expect("watch raw Project replica sources");
    assert!(
        tokio::time::timeout(Duration::from_millis(150), raw_project_watch.stream.next()).await.is_err(),
        "peers must not echo an unchanged third-origin Project indefinitely"
    );
    drop(feta_gouda);
}

#[tokio::test]
async fn a_peer_relays_another_origins_home_bound_runtime_resource() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let gouda = daemon(temp.path().join("gouda"), "gouda-root", "gouda").await;
    kiwi.resource_backend()
        .using::<TerminalSession>("flotilla")
        .create(
            &InputMeta::builder().name("kiwi-session".to_string()).build(),
            &TerminalSessionSpec::builder()
                .env_ref("env".to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Tool { command: "true".to_string() })
                .cwd("/tmp".to_string())
                .pool("passthrough".to_string())
                .build(),
        )
        .await
        .expect("create TerminalSession on kiwi");

    let kiwi_feta = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect kiwi and feta");
    let kiwi_origin = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let sessions =
                feta.resource_backend().including_replicas::<TerminalSession>("flotilla").list().await.expect("list feta sessions");
            if let Some(session) = sessions.items.into_iter().find(|session| session.object.metadata.name == "kiwi-session") {
                match session.provenance {
                    ResourceProvenance::Replica { origin_root, .. } => break origin_root,
                    ResourceProvenance::Local => panic!("kiwi TerminalSession must be a replica on feta"),
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("kiwi-to-feta TerminalSession replication timed out");
    drop(kiwi_feta);

    let feta_gouda = spawn_in_memory_request_topology(Arc::clone(&feta), Arc::clone(&gouda)).await.expect("connect feta and gouda");
    let relayed_origin = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let sessions =
                gouda.resource_backend().including_replicas::<TerminalSession>("flotilla").list().await.expect("list gouda sessions");
            if let Some(session) = sessions.items.into_iter().find(|session| session.object.metadata.name == "kiwi-session") {
                match session.provenance {
                    ResourceProvenance::Replica { origin_root, .. } => break origin_root,
                    ResourceProvenance::Local => panic!("relayed TerminalSession must remain a replica"),
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("feta did not relay kiwi TerminalSession to gouda");
    assert_eq!(relayed_origin, kiwi_origin, "relay must preserve the first hop's origin");
    assert!(
        gouda
            .resource_backend()
            .using::<TerminalSession>("flotilla")
            .list()
            .await
            .expect("gouda local TerminalSession log")
            .items
            .is_empty(),
        "relay must not re-author the TerminalSession on gouda"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut raw_session_watch = watch_resource_kind_replica_sources(&feta.resource_backend(), "flotilla", "terminalsessions")
        .await
        .expect("watch raw TerminalSession replica sources");
    assert!(
        tokio::time::timeout(Duration::from_millis(150), raw_session_watch.stream.next()).await.is_err(),
        "peers must not echo an unchanged third-origin TerminalSession indefinitely"
    );
    drop(feta_gouda);
}
