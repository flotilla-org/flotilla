use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_core::{config::ConfigStore, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery};
use flotilla_daemon::server::test_support::spawn_in_memory_request_topology;
use flotilla_protocol::HostName;
use flotilla_resources::{
    Convoy, ConvoySpec, InMemoryBackend, InputMeta, ResourceBackend, ResourceProvenance, TerminalSession, TerminalSessionSource,
    TerminalSessionSpec, Vessel, VesselSpec,
};

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

    drop(topology);
    let disconnected = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").list().await.expect("list disconnected replicas");
    assert!(
        disconnected.items.iter().any(|item| item.object.metadata.name == "last-known-convoy"),
        "origin absence must not remove last-known replica rows"
    );
}
