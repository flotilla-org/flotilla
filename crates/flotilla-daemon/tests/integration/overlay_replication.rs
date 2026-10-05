use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_core::{config::ConfigStore, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery};
use flotilla_daemon::{
    runtime::{DaemonRuntime, RuntimeOptions},
    server::test_support::spawn_in_memory_request_topology,
};
use flotilla_protocol::{FleetStaleness, HostName, PeerConnectionState, QueryId, Relationship, SubjectKind};
use flotilla_resources::{
    watch_resource_kind_replica_sources, ChangeRequestMergeability, ChangeRequestObservation, ChangeRequestState, Checkout, CheckoutPhase,
    CheckoutSpec, CheckoutStatus, ConditionValue, Convoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, Host, HostSpec, HostStatus,
    InMemoryBackend, InputMeta, IntegrationCondition, ObservedCheckoutSpec, Project, ProjectSpec, RepositoryKey, ResourceBackend,
    ResourceProvenance, SqliteBackend, TerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec,
    TerminalSessionStatus, Vessel, VesselSpec, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use futures::StreamExt;

fn config(path: std::path::PathBuf, machine_id: &str) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&path).expect("create config");
    std::fs::write(path.join("daemon.toml"), format!("machine_id = \"{machine_id}\"\n")).expect("write daemon config");
    Arc::new(ConfigStore::with_base(path))
}

async fn daemon(path: std::path::PathBuf, machine_id: &str, host: &str) -> Arc<InProcessDaemon> {
    daemon_with_backend(path, machine_id, host, ResourceBackend::InMemory(InMemoryBackend::default())).await
}

async fn sqlite_daemon(path: std::path::PathBuf, machine_id: &str, host: &str) -> Arc<InProcessDaemon> {
    std::fs::create_dir_all(&path).expect("create sqlite daemon directory");
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(path.join("resources.sqlite")).expect("open sqlite backend"));
    daemon_with_backend(path, machine_id, host, backend).await
}

async fn daemon_with_backend(path: std::path::PathBuf, machine_id: &str, host: &str, backend: ResourceBackend) -> Arc<InProcessDaemon> {
    InProcessDaemon::new_with_resource_backend(vec![], config(path, machine_id), fake_discovery(false), HostName::new(host), backend).await
}

#[tokio::test]
async fn sqlite_daemons_expose_remote_host_self_report_in_fleet_health() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = sqlite_daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = sqlite_daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let feta_host_id = feta.local_host_summary().await.environment_id.host_id().expect("feta host id").to_string();
    let started_at = chrono::Utc::now() - chrono::Duration::hours(2);
    let heartbeat_at = chrono::Utc::now();
    let feta_hosts = feta.resource_backend().using::<Host>("flotilla");
    let feta_host = feta_hosts
        .create(&InputMeta::builder().name(feta_host_id.clone()).build(), &HostSpec::default())
        .await
        .expect("create feta host self-report");
    feta_hosts
        .update_status(&feta_host_id, &feta_host.metadata.resource_version, &HostStatus {
            heartbeat_at: Some(heartbeat_at),
            ready: true,
            daemon_generation: Some("feta-generation".to_string()),
            daemon_version: Some("0.1.0".to_string()),
            daemon_started_at: Some(started_at),
            disk_free_bytes: Some(459_371_896_832),
            daemon_rss_bytes: Some(123_456_789),
            ..HostStatus::default()
        })
        .await
        .expect("publish feta host self-report");

    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect sqlite daemons");

    let replicated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let hosts = kiwi.resource_backend().including_replicas::<Host>("flotilla").list().await.expect("list kiwi host sources");
            if let Some(host) = hosts.items.into_iter().find(|host| {
                matches!(host.provenance, ResourceProvenance::Replica { .. })
                    && host.object.status.as_ref().and_then(|status| status.daemon_version.as_deref()) == Some("0.1.0")
            }) {
                break host;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("feta host self-report did not replicate to kiwi");
    assert!(matches!(
        replicated.provenance,
        ResourceProvenance::Replica { ref origin_root, .. } if origin_root == feta.node_id()
    ));

    let health = kiwi.fleet_health_internal().await.expect("query kiwi fleet health");
    let row = health.hosts.into_iter().find(|row| row.host == HostName::new("feta")).expect("feta fleet health row");

    assert_eq!(row.daemon_version.as_deref(), Some("0.1.0"));
    assert_eq!(row.daemon_generation.as_deref(), Some("feta-generation"));
    assert_eq!(row.heartbeat_at, Some(heartbeat_at));
    assert_eq!(row.link, PeerConnectionState::Connected);
    assert_eq!(row.disk_free_bytes, Some(459_371_896_832));
    assert_eq!(row.daemon_rss_bytes, Some(123_456_789));
    assert!(row.daemon_uptime_seconds.is_some_and(|uptime| uptime >= 2 * 60 * 60));
    drop(topology);
}

#[tokio::test]
async fn fleet_list_and_health_include_replicated_remote_crew_without_snapshot_fetch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let feta_host_id = feta.local_host_id().expect("feta host id").to_string();
    let feta_hosts = feta.resource_backend().using::<Host>("flotilla");
    let host =
        feta_hosts.create(&InputMeta::builder().name(feta_host_id.clone()).build(), &HostSpec::default()).await.expect("create feta host");
    feta_hosts
        .update_status(&feta_host_id, &host.metadata.resource_version, &HostStatus {
            heartbeat_at: Some(chrono::Utc::now()),
            daemon_generation: Some("feta-generation".to_string()),
            ready: true,
            ..HostStatus::default()
        })
        .await
        .expect("publish feta heartbeat");
    feta.resource_backend()
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder().name("feta-convoy".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).role("feta-convoy".to_string()).build(),
        )
        .await
        .expect("create feta convoy");
    feta.resource_backend()
        .using::<TerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("feta-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "feta-convoy".to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (ROLE_LABEL.to_string(), "coder".to_string()),
                ]))
                .build(),
            &TerminalSessionSpec::builder()
                .env_ref("feta-environment".to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Tool { command: "true".to_string() })
                .cwd("/tmp".to_string())
                .pool("passthrough".to_string())
                .build(),
        )
        .await
        .expect("create feta session");

    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect daemons");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let list = kiwi.fleet_list_internal().await.expect("fleet list");
            if list.rows.iter().any(|row| row.session.as_deref() == Some("feta-session")) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("remote crew did not appear in fleet list");
    let list = kiwi.fleet_list_internal().await.expect("fleet list");
    let row = list.rows.iter().find(|row| row.session.as_deref() == Some("feta-session")).expect("feta crew row");
    assert_eq!(row.host, HostName::new("feta"));
    assert_eq!(row.crew, "work/coder");
    assert_eq!(row.convoy, "feta-convoy");
    assert!(matches!(row.staleness, FleetStaleness::Fresh { .. }));
    let health = kiwi.fleet_health_internal().await.expect("fleet health");
    let feta_health = health.hosts.iter().find(|row| row.host == HostName::new("feta")).expect("feta health");
    assert_eq!(feta_health.crew_count, 1);
    assert_eq!(feta_health.convoy_count, 1);
    assert_eq!(feta_health.replica_generation.as_deref(), Some("feta-generation"));
    // #742: the aliases formerly indexed from snapshot rows remain attachable.
    std::fs::write(kiwi.config_store().base_path().join("hosts.toml"), "[hosts.feta]\nhostname = 'feta.example'\n")
        .expect("attach hop config");
    for reference in ["feta-session", "feta-convoy/work/coder", "work/coder", "work", "coder", "feta-environment"] {
        let resolved =
            kiwi.resolve_attach_command_on_host_internal(reference, Some(&HostName::new("feta"))).await.expect("remote crew alias");
        let binding = resolved.binding.expect("crew binding");
        assert_eq!(binding.host, HostName::new("feta"));
        assert_eq!(binding.session.as_deref(), Some("feta-session"));
        assert_eq!(binding.convoy.as_deref(), Some("feta-convoy"));
        assert_eq!(binding.vessel.as_deref(), Some("work"));
        assert_eq!(binding.role.as_deref(), Some("coder"));
    }
    drop(topology);
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

#[tokio::test]
async fn lost_authority_record_tombstone_converges_through_two_peers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let gouda = daemon(temp.path().join("gouda"), "gouda-root", "gouda").await;
    let origin = kiwi.node_id().clone();

    let fixture_backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let fixture = fixture_backend.using::<TerminalSession>("flotilla");
    fixture
        .create(
            &InputMeta::builder().name("lost-at-authority".to_string()).build(),
            &TerminalSessionSpec::builder()
                .env_ref("env".to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Tool { command: "true".to_string() })
                .cwd("/tmp".to_string())
                .pool("passthrough".to_string())
                .build(),
        )
        .await
        .expect("create stale session fixture");
    let stale = fixture.list().await.expect("list stale session fixture");
    let stale_synced_at = chrono::Utc::now() - chrono::Duration::minutes(1);
    for daemon in [&kiwi, &feta, &gouda] {
        daemon
            .resource_backend()
            .replica_writer::<TerminalSession>(origin.clone(), "flotilla")
            .replace(&stale, stale_synced_at)
            .await
            .expect("seed stale origin replica");
    }

    let deleted = flotilla_resources::delete_resource_kind(&kiwi.resource_backend(), "flotilla", "terminalsessions", "lost-at-authority")
        .await
        .expect("tombstone lost authority record");
    assert!(!deleted.already_deleted);
    assert_eq!(deleted.object.value["metadata"]["resourceVersion"], "2", "the tombstone must advance beyond the recovered replica cursor");

    let kiwi_feta = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect authority to first peer");
    let feta_gouda = spawn_in_memory_request_topology(Arc::clone(&feta), Arc::clone(&gouda)).await.expect("connect relay to second peer");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut converged = true;
            for daemon in [&kiwi, &feta, &gouda] {
                converged &= daemon
                    .resource_backend()
                    .including_replicas::<TerminalSession>("flotilla")
                    .list()
                    .await
                    .expect("list converging session replicas")
                    .items
                    .is_empty();
            }
            if converged {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("authority tombstone did not converge through both peers");

    tokio::time::sleep(Duration::from_millis(100)).await;
    for daemon in [&kiwi, &feta, &gouda] {
        assert!(
            daemon
                .resource_backend()
                .including_replicas::<TerminalSession>("flotilla")
                .list()
                .await
                .expect("list settled session replicas")
                .items
                .is_empty(),
            "the stale replica must not resurrect after relay cycles"
        );
    }

    let repeated = flotilla_resources::delete_resource_kind(&kiwi.resource_backend(), "flotilla", "terminalsessions", "lost-at-authority")
        .await
        .expect("repeat lost authority delete");
    assert!(repeated.already_deleted);
    drop(feta_gouda);
    drop(kiwi_feta);
}

#[tokio::test]
async fn connected_in_process_daemons_replicate_checkout_settlement_evidence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let feta_checkouts = feta.resource_backend().using::<Checkout>("flotilla");
    let checkout = feta_checkouts
        .create(
            &InputMeta::builder().name("remote-checkout".to_string()).build(),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: "feature/remote".to_string(),
                path: "/srv/remote/repo".to_string(),
                repo_ref: RepositoryKey("remote-repo".to_string()),
                host_ref: "feta".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("create checkout on vessel host");
    feta_checkouts
        .update_status(&checkout.metadata.name, &checkout.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some("/srv/remote/repo".to_string()),
            integration: flotilla_resources::CheckoutIntegrationStatus {
                landed: IntegrationCondition::builder().value(ConditionValue::True).build(),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("publish checkout settlement evidence on vessel host");

    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect topology");

    let replicated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let checkouts =
                kiwi.resource_backend().including_replicas::<Checkout>("flotilla").list().await.expect("list federated checkouts");
            if let Some(checkout) = checkouts.items.into_iter().find(|checkout| checkout.object.metadata.name == "remote-checkout") {
                break checkout;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("checkout did not replicate to authority host");

    assert!(matches!(replicated.provenance, ResourceProvenance::Replica { .. }));
    assert_eq!(
        replicated.object.status.as_ref().map(|status| status.integration.landed.value),
        Some(ConditionValue::True),
        "checkout settlement evidence must replicate with the resource"
    );
    assert!(
        kiwi.resource_backend().using::<Checkout>("flotilla").list().await.expect("list authority-local checkouts").items.is_empty(),
        "replication must not re-author the vessel-host checkout on the authority"
    );
    drop(topology);
}

#[tokio::test]
async fn pr_observed_after_admission_becomes_home_subject_and_replica_fact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let repository_url = "https://github.com/flotilla-org/flotilla";
    let _kiwi_runtime = DaemonRuntime::start_with_options(Arc::clone(&kiwi), kiwi.config_store(), None, RuntimeOptions {
        namespace: "flotilla".into(),
        start_controllers: false,
        ..RuntimeOptions::default()
    })
    .await
    .expect("start other host query projection");
    let repository_key = RepositoryKey("repo_flotilla".into());
    let home = feta.resource_backend();
    let convoy = home
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder().name("crew-convoy".to_string()).build(),
            &ConvoySpec::builder()
                .workflow_ref("workflow".to_string())
                .r#ref("fix/crew-branch".to_string())
                .repositories(vec![ConvoyRepositorySpec {
                    url: repository_url.into(),
                    repo_ref: repository_key.clone(),
                    source_ref: "main".into(),
                    target_ref: "main".into(),
                    workspace_slug: "flotilla".into(),
                    subpaths: Vec::new(),
                }])
                .adopted_checkout_refs(BTreeMap::from([(repository_key.clone(), "crew-checkout".to_string())]))
                .build(),
        )
        .await
        .expect("admit convoy before PR exists");
    home.using::<Convoy>("flotilla")
        .update_status("crew-convoy", &convoy.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Active,
            observed_workflow_ref: Some("workflow".into()),
            ..Default::default()
        })
        .await
        .expect("convoy is active before PR exists");
    let _feta_runtime = DaemonRuntime::start_with_options(Arc::clone(&feta), feta.config_store(), None, RuntimeOptions {
        namespace: "flotilla".into(),
        controller_resync_interval: Duration::from_secs(300),
        ..RuntimeOptions::default()
    })
    .await
    .expect("start home controller before crew opens PR");
    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect hosts");
    let kiwi_checkouts = kiwi.resource_backend().using::<Checkout>("flotilla");
    let checkout = kiwi_checkouts
        .create(
            &InputMeta::builder()
                .name("crew-checkout".to_string())
                .labels(BTreeMap::from([("flotilla.work/convoy".to_string(), "crew-convoy".to_string())]))
                .build(),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: "fix/crew-branch".into(),
                path: "/srv/crew/flotilla".into(),
                repo_ref: repository_key,
                host_ref: "kiwi".into(),
                is_main: false,
            }),
        )
        .await
        .expect("create checkout after admission");
    kiwi_checkouts
        .update_status(&checkout.metadata.name, &checkout.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            integration: flotilla_resources::CheckoutIntegrationStatus {
                change_request: Some(ChangeRequestObservation {
                    id: "2301".into(),
                    state: ChangeRequestState::Open,
                    mergeability: ChangeRequestMergeability::Unknown,
                    target_ref: Some("main".into()),
                    observed_at: chrono::Utc::now().to_rfc3339(),
                }),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("crew opens PR without changing convoy phase");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if home
                .including_replicas::<Checkout>("flotilla")
                .get("crew-checkout")
                .await
                .ok()
                .and_then(|checkout| checkout.object.status)
                .is_some_and(|status| status.integration.change_request.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("home receives checkout observation");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if kiwi.resource_backend().including_replicas::<Convoy>("flotilla").get("crew-convoy").await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("other host receives convoy replica");
    kiwi.discover_convoy_branch_subjects("flotilla", "crew-convoy", "fix/crew-branch")
        .await
        .expect("replica host skips home-only discovery");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let replica = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").get("crew-convoy").await;
            if replica.ok().and_then(|record| record.object.status).is_some_and(|status| {
                status.subjects.iter().any(|entry| {
                    entry.subject.kind == SubjectKind::ChangeRequest
                        && entry.subject.id == "2301"
                        && entry.relationship == Relationship::Produces
                })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("other host sees replicated PR subject");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = kiwi.aggregator_projection_state().await.result_set().await;
            if result
                .rows
                .as_convoys()
                .expect("convoy rows")
                .iter()
                .any(|row| row.resource.name == "crew-convoy" && row.subjects.iter().any(|entry| entry.subject.id == "2301"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("other host row shows replicated PR subject");
    drop(topology);
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

// #742: remote checkout queries and independent attach targets retain their
// content through resource replication, including updates and removals.
#[tokio::test]
async fn observed_resources_replicate_checkout_queries_and_independent_attach_targets() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&kiwi), kiwi.config_store(), None, RuntimeOptions {
        namespace: "flotilla".into(),
        start_controllers: false,
        ..RuntimeOptions::default()
    })
    .await
    .expect("start query projection");
    let checkouts = feta.observed_resource_backend().using::<Checkout>("flotilla");
    let sessions = feta.observed_resource_backend().using::<TerminalSession>("flotilla");
    let spec = CheckoutSpec::Observed(ObservedCheckoutSpec {
        repo_ref: RepositoryKey("widgets".into()),
        path: "/srv/widgets".into(),
        r#ref: "main".into(),
        host_ref: "feta".into(),
        is_main: true,
    });
    checkouts.create(&InputMeta::builder().name("checkout".to_string()).build(), &spec).await.expect("create remote checkout");
    // Identical names on different origins remain distinct rows.
    kiwi.observed_resource_backend()
        .using::<Checkout>("flotilla")
        .create(&InputMeta::builder().name("checkout".to_string()).build(), &spec)
        .await
        .expect("create local checkout");
    let session = sessions
        .create(
            &InputMeta::builder().name("terminal-independent".to_string()).build(),
            &TerminalSessionSpec::builder()
                .env_ref("feta-env".to_string())
                .role("shell".to_string())
                .source(TerminalSessionSource::Tool { command: "sh".into() })
                .cwd("/srv/widgets".to_string())
                .pool("passthrough".to_string())
                .build(),
        )
        .await
        .expect("create independent");
    sessions
        .update_status(&session.metadata.name, &session.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some("terminal-independent".into()),
            ..Default::default()
        })
        .await
        .expect("running independent");
    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect daemons");
    let state = kiwi.aggregator_projection_state().await;
    for branch in ["main", "feature", "main"] {
        let current = checkouts.get("checkout").await.expect("checkout");
        let CheckoutSpec::Observed(mut updated) = current.spec else { unreachable!() };
        updated.r#ref = branch.into();
        checkouts
            .update(
                &InputMeta::builder().name("checkout".to_string()).build(),
                &current.metadata.resource_version,
                &CheckoutSpec::Observed(updated),
            )
            .await
            .expect("update branch");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let set = state.result_set_for(&QueryId::Checkouts { scope: None }).await.expect("checkout query");
                let rows = set.rows.as_checkouts().expect("checkout rows");
                if rows.len() == 2 && rows.iter().any(|row| row.host == HostName::new("feta") && row.branch == branch) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("remote checkout branch update");
        let set = state.result_set_for(&QueryId::Checkouts { scope: None }).await.expect("checkout query");
        let rows = set.rows.as_checkouts().expect("checkout rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.path == "/srv/widgets" && row.resource.host.as_ref() == Some(&row.host)));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if kiwi
                .observed_resource_backend()
                .including_replicas::<TerminalSession>("flotilla")
                .list()
                .await
                .expect("session replicas")
                .items
                .iter()
                .any(|source| source.object.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("independent replication");
    std::fs::write(kiwi.config_store().base_path().join("hosts.toml"), "[hosts.feta]\nhostname = 'feta.example'\n")
        .expect("attach hop config");
    let resolved = kiwi
        .resolve_attach_command_on_host_internal("terminal-independent", Some(&HostName::new("feta")))
        .await
        .expect("remote independent attach");
    assert_eq!(resolved.binding.as_ref().map(|binding| &binding.host), Some(&HostName::new("feta")));
    let checkout_attach =
        kiwi.resolve_transient_attach_command_internal("/srv/widgets", Some(&HostName::new("feta"))).await.expect("remote checkout attach");
    assert!(checkout_attach.binding.is_none());
    assert!(serde_json::to_string(&checkout_attach.plan).expect("attach plan").contains("/srv/widgets"));
    assert!(kiwi.resource_backend().including_replicas::<Checkout>("flotilla").list().await.expect("durable checkouts").items.is_empty());
    checkouts.delete("checkout").await.expect("delete remote checkout");
    sessions.delete("terminal-independent").await.expect("delete independent");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let set = state.result_set_for(&QueryId::Checkouts { scope: None }).await.expect("checkout query");
            let rows = set.rows.as_checkouts().expect("checkout rows");
            let remote_sessions =
                kiwi.observed_resource_backend().including_replicas::<TerminalSession>("flotilla").list().await.expect("sessions");
            if rows.len() == 1 && rows[0].host == HostName::new("kiwi") && remote_sessions.items.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("remote observations removed");
    assert!(kiwi.resolve_attach_command_on_host_internal("terminal-independent", Some(&HostName::new("feta"))).await.is_err());
    drop(topology);
}

// #2636: a missed deletion followed by a later event advances the cursor past
// the tombstone. A successful authoritative snapshot must repair the object set.
#[tokio::test]
async fn reconnect_repairs_a_delete_missing_before_the_cursor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let authority = feta.resource_backend().using::<Convoy>("flotilla");
    authority
        .create(&InputMeta::builder().name("gone".to_string()).build(), &ConvoySpec::builder().workflow_ref("workflow".to_string()).build())
        .await
        .expect("create");
    let writer = kiwi.resource_backend().replica_writer::<Convoy>(feta.node_id().clone(), "flotilla");
    writer.replace(&authority.list().await.expect("snapshot"), chrono::Utc::now()).await.expect("seed replica");
    authority.delete("gone").await.expect("delete");
    let retained = authority
        .create(
            &InputMeta::builder().name("retained".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("later create");
    writer.apply(flotilla_resources::WatchEvent::Added(retained), chrono::Utc::now()).await.expect("deliver later event only");
    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("reconnect");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let listed = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").list().await.expect("replicas");
            if listed.items.len() == 1 && listed.items[0].object.metadata.name == "retained" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("resync must remove the missed deletion and retain live objects");
    drop(topology);
}

async fn await_replica_names(daemon: &InProcessDaemon, expected: &[&str]) {
    // Allow bounded retry backoff (five virtual seconds): a sequence gap must repair now,
    // failed snapshot requests must retry without dropping the previous set.
    for _ in 0..500 {
        let mut names: Vec<_> = daemon
            .resource_backend()
            .including_replicas::<Convoy>("flotilla")
            .list()
            .await
            .expect("replicas")
            .items
            .into_iter()
            .map(|source| source.object.metadata.name)
            .collect();
        names.sort();
        if names == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "replicas did not converge to {expected:?}: {:?}",
        daemon.resource_backend().including_replicas::<Convoy>("flotilla").list().await.expect("replicas")
    );
}

async fn missed_delete_scenario(fail_resync: bool) {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use flotilla_daemon::server::test_support::spawn_in_memory_request_mesh_with_filter;
    use flotilla_protocol::{
        CommandAction, CommandPeerEvent, CommandValue, PeerWireMessage, ResourceRecordType, RoutedPeerMessage, StepStatus,
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let authority = feta.resource_backend().using::<Convoy>("flotilla");
    let spec = ConvoySpec::builder().workflow_ref("workflow".to_string()).build();
    for name in ["gone", "retained"] {
        authority.create(&InputMeta::builder().name(name.to_string()).build(), &spec).await.expect("create");
    }
    let failing = Arc::new(AtomicBool::new(false));
    let failures = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let successful_snapshots = Arc::new(AtomicUsize::new(0));
    // Stands in for the peer network boundary, dropping deletion envelopes and
    // making snapshot requests fail at the real authoritative dispatcher.
    let filter = {
        let failing = Arc::clone(&failing);
        let failures = Arc::clone(&failures);
        let dropped = Arc::clone(&dropped);
        let successful_snapshots = Arc::clone(&successful_snapshots);
        let origin = feta.node_id().clone();
        Arc::new(move |mut message: PeerWireMessage| {
            if let PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest { target_node_id, command, .. }) = &mut message {
                if let CommandAction::ResourceWatch { kind, cursor, replica_sources: false, .. } = &mut command.action {
                    if target_node_id == &origin && kind == "convoys" && cursor.is_none() && !failing.load(Ordering::SeqCst) {
                        successful_snapshots.fetch_add(1, Ordering::SeqCst);
                    }
                    if kind == "convoys" && failing.load(Ordering::SeqCst) {
                        *kind = "unavailable-test-kind".to_string();
                        failures.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            if let PeerWireMessage::Routed(RoutedPeerMessage::CommandEvent { event, .. }) = &message {
                if let CommandPeerEvent::StepUpdate { status: StepStatus::Produced { value }, .. } = event.as_ref() {
                    if let CommandValue::ResourceWatchEvent(response) = value.as_ref() {
                        if response.resource_kind == "Convoy"
                            && response.records.iter().any(|record| record.record_type == ResourceRecordType::Deleted)
                        {
                            dropped.fetch_add(1, Ordering::SeqCst);
                            return None;
                        }
                    }
                }
            }
            Some(message)
        })
    };
    let mesh = spawn_in_memory_request_mesh_with_filter(vec![Arc::clone(&kiwi), Arc::clone(&feta)], Some(&["Convoy"]), filter)
        .await
        .expect("connect mesh");
    await_replica_names(&kiwi, &["gone", "retained"]).await;
    assert!(successful_snapshots.swap(0, Ordering::SeqCst) >= 1, "initial snapshot delivered");
    authority.delete("gone").await.expect("delete at authority");
    for _ in 0..20_000 {
        if dropped.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(dropped.load(Ordering::SeqCst) > 0, "test must actually drop the deletion");
    failing.store(fail_resync, Ordering::SeqCst);
    {
        let current = authority.get("retained").await.expect("live object");
        authority
            .update(
                &InputMeta::builder().name("retained".to_string()).build(),
                &current.metadata.resource_version,
                &ConvoySpec::builder().workflow_ref("updated-workflow".to_string()).build(),
            )
            .await
            .expect("deliver event after gap");
    }
    if fail_resync {
        for _ in 0..500 {
            if failures.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(failures.load(Ordering::SeqCst) > 0, "test must exercise a failed listing");
        await_replica_names(&kiwi, &["gone", "retained"]).await;
        assert!(
            kiwi.resource_backend()
                .replica_writer::<Convoy>(feta.node_id().clone(), "flotilla")
                .cursor()
                .await
                .expect("cursor after failed repair")
                .is_none(),
            "gap repair must retry a snapshot, not the old resume cursor"
        );
        failing.store(false, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(31)).await;
    }
    await_replica_names(&kiwi, &["retained"]).await;
    assert_eq!(successful_snapshots.load(Ordering::SeqCst), 1, "gap requires exactly one successful resnapshot");
    drop(mesh);
}

// #2636: a hole in the origin's event sequence forces a complete snapshot
// before accepting subsequent events, preserving live objects.
#[tokio::test(start_paused = true)]
async fn sequence_gap_repairs_a_missed_delete() {
    missed_delete_scenario(false).await;
}

// #2636: a sequence gap forces repair, but a failed listing must drop nothing.
#[tokio::test(start_paused = true)]
async fn gap_resnapshot_preserves_replicas_on_listing_failure_then_repairs() {
    missed_delete_scenario(true).await;
}

// Generated scenarios cover transient snapshot failure versus success.
#[hegel::test]
fn generated_missed_delete_resync(tc: hegel::TestCase) {
    let failure = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime")
        .block_on(missed_delete_scenario(failure));
}

// #2640 review: unfiltered origin watches use a dense numeric sequence scoped
// to (kind, namespace). Other namespaces must not cause gap-repair snapshots.
#[tokio::test(start_paused = true)]
async fn other_namespace_writes_do_not_force_replication_resnapshots() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_daemon::server::test_support::spawn_in_memory_request_mesh_with_filter;
    use flotilla_protocol::{CommandAction, PeerWireMessage, RoutedPeerMessage};

    for sqlite in [false, true] {
        let temp = tempfile::tempdir().expect("tempdir");
        let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
        let feta = if sqlite {
            sqlite_daemon(temp.path().join("feta"), "feta-root", "feta").await
        } else {
            daemon(temp.path().join("feta"), "feta-root", "feta").await
        };
        let origin = feta.node_id().clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let filter = {
            let requests = Arc::clone(&requests);
            Arc::new(move |message: PeerWireMessage| {
                if let PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest { target_node_id, command, .. }) = &message {
                    if target_node_id == &origin
                        && matches!(&command.action,
                        CommandAction::ResourceWatch { kind, replica_sources: false, .. } if kind == "convoys")
                    {
                        requests.fetch_add(1, Ordering::SeqCst);
                    }
                }
                Some(message)
            })
        };
        let authority = feta.resource_backend().using::<Convoy>("flotilla");
        let meta = InputMeta::builder().name("retained".to_string()).build();
        let spec = ConvoySpec::builder().workflow_ref("workflow".to_string()).build();
        let created = authority.create(&meta, &spec).await.expect("create");
        let mesh = spawn_in_memory_request_mesh_with_filter(vec![Arc::clone(&kiwi), Arc::clone(&feta)], Some(&["Convoy"]), filter)
            .await
            .expect("connect mesh");
        await_replica_names(&kiwi, &["retained"]).await;
        let snapshots = requests.load(Ordering::SeqCst);
        let other = feta.resource_backend().using::<Convoy>("other-namespace");
        other.create(&meta, &spec).await.expect("create in other namespace");
        other.delete("retained").await.expect("delete in other namespace");
        let updated = authority
            .update(&meta, &created.metadata.resource_version, &ConvoySpec::builder().workflow_ref("updated-workflow".to_string()).build())
            .await
            .expect("update origin");
        assert_eq!(
            updated.metadata.resource_version.parse::<u64>().expect("numeric version"),
            created.metadata.resource_version.parse::<u64>().expect("numeric version") + 1
        );
        let mut converged = false;
        for _ in 0..500 {
            let record = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").get("retained").await.expect("replica");
            if record.object.metadata.resource_version == updated.metadata.resource_version {
                converged = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(converged, "live update must arrive");
        assert_eq!(requests.load(Ordering::SeqCst), snapshots, "other namespaces must not force resnapshot");
        drop(mesh);
    }
}

// Resume a proven prefix without data transfer; only an expired horizon or a
// quarantined log hole authorizes a single complete origin snapshot.
#[tokio::test]
async fn reconnect_uses_log_until_horizon_or_quarantine_requires_one_snapshot() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_daemon::server::test_support::spawn_in_memory_request_mesh_with_filter;
    use flotilla_protocol::{CommandAction, PeerWireMessage, RoutedPeerMessage};
    use flotilla_resources::EventRetention;

    for repair in ["valid", "horizon", "quarantine"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
        let origin_path = temp.path().join("origin.sqlite");
        let backend = ResourceBackend::Sqlite(
            SqliteBackend::open_with_event_retention(&origin_path, EventRetention::new(2).expect("retention")).expect("origin store"),
        );
        let feta = daemon_with_backend(temp.path().join("feta"), "feta-root", "feta", backend).await;
        let authority = feta.resource_backend().using::<Convoy>("flotilla");
        let spec = ConvoySpec::builder().workflow_ref("workflow".to_string()).build();
        for name in ["gone", "retained"] {
            authority.create(&InputMeta::builder().name(name.to_string()).build(), &spec).await.expect("create");
        }
        let writer = kiwi.resource_backend().replica_writer::<Convoy>(feta.node_id().clone(), "flotilla");
        let listed = authority.list().await.expect("snapshot");
        writer.replace(&listed, chrono::Utc::now()).await.expect("establish complete prefix");
        authority.delete("gone").await.expect("delete while disconnected");
        if repair == "horizon" {
            for workflow in ["second", "third"] {
                let current = authority.get("retained").await.expect("retained");
                authority
                    .update(
                        &InputMeta::builder().name("retained".to_string()).build(),
                        &current.metadata.resource_version,
                        &ConvoySpec::builder().workflow_ref(workflow.to_string()).build(),
                    )
                    .await
                    .expect("advance horizon");
            }
        } else if repair == "quarantine" {
            let connection = rusqlite::Connection::open(&origin_path).expect("raw store");
            let body: String = connection
                .query_row("SELECT body_json FROM resource_events WHERE kind = 'Convoy' AND event_version = 3", [], |row| row.get(0))
                .expect("delete event");
            let mut body: serde_json::Value = serde_json::from_str(&body).expect("event JSON");
            body["spec"].as_object_mut().expect("deleted spec").remove("workflow_ref");
            connection
                .execute("UPDATE resource_events SET body_json = ?1 WHERE kind = 'Convoy' AND event_version = 3", [serde_json::to_string(
                    &body,
                )
                .expect("encode")])
                .expect("poison final delete");
        }
        let snapshots = Arc::new(AtomicUsize::new(0));
        let origin = feta.node_id().clone();
        let filter = {
            let snapshots = Arc::clone(&snapshots);
            Arc::new(move |message: PeerWireMessage| {
                if let PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest { target_node_id, command, .. }) = &message {
                    if target_node_id == &origin
                        && matches!(&command.action,
                        CommandAction::ResourceWatch { kind, replica_sources: false, cursor: None, .. } if kind == "convoys")
                    {
                        snapshots.fetch_add(1, Ordering::SeqCst);
                    }
                }
                Some(message)
            })
        };
        let mesh = spawn_in_memory_request_mesh_with_filter(vec![Arc::clone(&kiwi), Arc::clone(&feta)], Some(&["Convoy"]), filter)
            .await
            .expect("reconnect");
        await_replica_names(&kiwi, &["retained"]).await;
        assert_eq!(snapshots.load(Ordering::SeqCst), usize::from(repair != "valid"), "{repair}: full snapshots are exceptional");
        assert_eq!(
            writer.cursor().await.expect("cursor").expect("prefix").resource_version,
            authority.list().await.expect("current authority").resource_version
        );
        if repair == "quarantine" {
            assert_eq!(
                feta.resource_backend().diagnostics().await.expect("diagnostics").expect("SQLite").event_decode_quarantines.len(),
                1
            );
        }
        drop(mesh);
    }
}

async fn assert_digest_names(daemon: &InProcessDaemon, expected: &[&str]) {
    let mut names = daemon
        .resource_backend()
        .including_replicas::<Convoy>("flotilla")
        .list()
        .await
        .expect("replicas")
        .items
        .into_iter()
        .map(|item| item.object.metadata.name)
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, expected, "completed digest round has the expected replicas");
}

// #2638: periodic digests repair a dropped final delete through the real
// replicator over in-memory sessions. A match sends no bodies or writes.
async fn digest_session_scenario(sqlite: bool, fail_snapshot: bool, advanced_prefix: bool) {
    tokio::time::timeout(Duration::from_secs(30), run_digest_session_scenario(sqlite, fail_snapshot, advanced_prefix))
        .await
        .expect("digest session scenario timed out");
}

async fn run_digest_session_scenario(sqlite: bool, fail_snapshot: bool, advanced_prefix: bool) {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    };

    use flotilla_daemon::server::test_support::spawn_in_memory_request_mesh_with_digest_driver;
    use flotilla_protocol::{
        CommandAction, CommandPeerEvent, CommandValue, PeerWireMessage, ResourceRecordType, RoutedPeerMessage, StepStatus,
    };
    use flotilla_resources::{digest_bucket, DigestQuery};
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = if sqlite {
        sqlite_daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await
    } else {
        daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await
    };
    let feta = if sqlite {
        sqlite_daemon(temp.path().join("feta"), "feta-root", "feta").await
    } else {
        daemon(temp.path().join("feta"), "feta-root", "feta").await
    };
    let authority = feta.resource_backend().using::<Convoy>("flotilla");
    let spec = ConvoySpec::builder().workflow_ref("workflow".to_string()).build();
    assert_ne!(digest_bucket("gone"), digest_bucket("retained"), "fixtures exercise separate buckets");
    for name in ["gone", "retained"] {
        authority.create(&InputMeta::builder().name(name.to_string()).build(), &spec).await.expect("create");
    }
    let resumed = Arc::new(AtomicUsize::new(0));
    let roots = Arc::new(AtomicUsize::new(0));
    let children = Arc::new(AtomicUsize::new(0));
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let whole_snapshots = Arc::new(AtomicUsize::new(0));
    let (dropped, mut dropped_events) = tokio::sync::watch::channel(0usize);
    let failing = Arc::new(AtomicBool::new(false));
    // This is the network boundary: drop only delete events, or refuse a bucket
    // read at the real command dispatcher. No storage collaborator is mocked.
    let filter = {
        let (roots, children, snapshots, whole_snapshots, dropped, failing) = (
            Arc::clone(&roots),
            Arc::clone(&children),
            Arc::clone(&snapshots),
            Arc::clone(&whole_snapshots),
            dropped.clone(),
            Arc::clone(&failing),
        );
        let resumed = Arc::clone(&resumed);
        let origin = feta.node_id().clone();
        Arc::new(move |mut message: PeerWireMessage| {
            if let PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest { target_node_id, command, .. }) = &mut message {
                if target_node_id == &origin {
                    match &mut command.action {
                        CommandAction::QueryResourceDigest { kind, query, .. } if kind == "convoys" => match query.clone() {
                            DigestQuery::Root => {
                                roots.fetch_add(1, Ordering::SeqCst);
                            }
                            DigestQuery::Children { .. } => {
                                children.fetch_add(1, Ordering::SeqCst);
                            }
                            DigestQuery::Snapshot { bucket, .. } => {
                                snapshots.lock().expect("snapshots").push(bucket);
                                if failing.load(Ordering::SeqCst) {
                                    *kind = "unavailable-test-kind".into();
                                }
                            }
                        },
                        CommandAction::ResourceWatch { kind, cursor: None, replica_sources: false, .. } if kind == "convoys" => {
                            whole_snapshots.fetch_add(1, Ordering::SeqCst);
                        }
                        CommandAction::ResourceWatch { kind, cursor: Some(_), replica_sources: false, .. } if kind == "convoys" => {
                            resumed.fetch_add(1, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
            }
            if let PeerWireMessage::Routed(RoutedPeerMessage::CommandEvent { event, .. }) = &message {
                if let CommandPeerEvent::StepUpdate { status: StepStatus::Produced { value }, .. } = event.as_ref() {
                    if let CommandValue::ResourceWatchEvent(response) = value.as_ref() {
                        if response.resource_kind == "Convoy"
                            && response.records.iter().any(|record| record.record_type == ResourceRecordType::Deleted)
                        {
                            dropped.send_modify(|count| *count += 1);
                            return None;
                        }
                    }
                }
            }
            Some(message)
        })
    };
    let mesh = spawn_in_memory_request_mesh_with_digest_driver(vec![Arc::clone(&kiwi), Arc::clone(&feta)], Some(&["Convoy"]), filter)
        .await
        .expect("mesh");
    let driver = mesh.digest_driver.as_ref().expect("explicit digest driver");
    driver.watch_ready(kiwi.node_id(), feta.node_id(), "convoys").await.expect("initial watch ready");
    assert_digest_names(&kiwi, &["gone", "retained"]).await;

    let full = whole_snapshots.load(Ordering::SeqCst);
    let before = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").list().await.expect("replicas");
    assert!(!driver.round(kiwi.node_id(), feta.node_id(), "convoys").await.expect("matching round"));
    assert_eq!(roots.load(Ordering::SeqCst), 1, "explicit root exchange completed");
    assert_eq!(children.load(Ordering::SeqCst), 0, "a matching digest never drills down");
    assert!(snapshots.lock().expect("snapshots").is_empty(), "a match transfers no resource bodies");
    let after = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").list().await.expect("replicas");
    let timestamps =
        |list: flotilla_resources::ReadResourceList<Convoy>| list.items.into_iter().map(|item| item.provenance).collect::<Vec<_>>();
    assert_eq!(timestamps(before), timestamps(after), "matching digests do not write replicas");
    failing.store(fail_snapshot, Ordering::SeqCst);
    authority.delete("gone").await.expect("final delete");
    dropped_events.wait_for(|count| *count > 0).await.expect("final deletion actually dropped");
    if advanced_prefix {
        // Reproduce an older holder's relay-advanced cursor: its supposedly
        // complete prefix includes a deletion that its object set still lacks.
        let objects = kiwi
            .resource_backend()
            .including_replicas::<Convoy>("flotilla")
            .list()
            .await
            .expect("stale set")
            .items
            .into_iter()
            .map(|item| item.object)
            .collect();
        let position = authority.current_position().await.expect("advanced position");
        kiwi.resource_backend()
            .replica_writer::<Convoy>(feta.node_id().clone(), "flotilla")
            .replace(
                &flotilla_resources::ResourceList {
                    items: objects,
                    resource_version: position.resource_version,
                    generation: position.generation,
                },
                chrono::Utc::now(),
            )
            .await
            .expect("seed legacy advanced prefix");
    }
    let writer = kiwi.resource_backend().replica_writer::<Convoy>(feta.node_id().clone(), "flotilla");
    let previous_cursor = writer.cursor().await.expect("cursor before proof");
    let result = driver.round(kiwi.node_id(), feta.node_id(), "convoys").await;
    if fail_snapshot {
        assert!(result.is_err(), "failed proof reports completion without repair");
        assert_digest_names(&kiwi, &["gone", "retained"]).await;
        assert_eq!(writer.cursor().await.expect("cursor after failed proof"), previous_cursor, "failed proof preserves cursor");
        assert!(!snapshots.lock().expect("snapshots").is_empty(), "failed snapshot attempted");
        failing.store(false, Ordering::SeqCst);
        snapshots.lock().expect("snapshots").clear();
        assert!(driver.round(kiwi.node_id(), feta.node_id(), "convoys").await.expect("retry repair"));
    } else {
        assert!(result.expect("repair round"));
    }
    assert_digest_names(&kiwi, &["retained"]).await;
    assert_eq!(*snapshots.lock().expect("snapshots"), vec![digest_bucket("gone")], "only the diverged partition is re-snapshotted");
    assert_eq!(whole_snapshots.load(Ordering::SeqCst), full, "periodic safety net never requests a full snapshot");
    // Repair proves the origin's cut and resumes log sync without a full list.
    driver.watch_ready(kiwi.node_id(), feta.node_id(), "convoys").await.expect("repaired watch ready");
    assert!(resumed.load(Ordering::SeqCst) > 0, "repair resumes the primary log from its proven cut");
    let writer = kiwi.resource_backend().replica_writer::<Convoy>(feta.node_id().clone(), "flotilla");
    assert_eq!(
        writer.cursor().await.expect("cursor").expect("complete prefix").resource_version,
        authority.current_position().await.expect("authority position").resource_version
    );
    let mut live = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").watch().await.expect("replica watch");
    let current = authority.get("retained").await.expect("retained");
    authority
        .update(
            &InputMeta::builder().name("retained".into()).build(),
            &current.metadata.resource_version,
            &ConvoySpec::builder().workflow_ref("after-repair".into()).build(),
        )
        .await
        .expect("next live event");
    while let Some(event) = live.next().await {
        if matches!(event.expect("live event"), flotilla_resources::ReadWatchEvent::Modified(item) if item.object.spec.workflow_ref == "after-repair")
        {
            break;
        }
    }
    assert_eq!(
        kiwi.resource_backend().including_replicas::<Convoy>("flotilla").get("retained").await.expect("replica").object.spec.workflow_ref,
        "after-repair"
    );
    assert_eq!(whole_snapshots.load(Ordering::SeqCst), full, "next live event needs no redundant full snapshot");
    // Empty partitions also converge by a bucket snapshot.
    snapshots.lock().expect("snapshots").clear();
    let previous_drops = *dropped_events.borrow_and_update();
    authority.delete("retained").await.expect("delete last key");
    dropped_events.wait_for(|count| *count > previous_drops).await.expect("last deletion dropped");
    assert!(driver.round(kiwi.node_id(), feta.node_id(), "convoys").await.expect("empty partition repair"));
    assert_digest_names(&kiwi, &[]).await;
    assert_eq!(*snapshots.lock().expect("snapshots"), vec![digest_bucket("retained")]);
    drop(mesh);
}

#[tokio::test]
async fn in_memory_digest_match_and_partition_repair() {
    digest_session_scenario(false, false, false).await;
}
#[tokio::test]
async fn sqlite_digest_match_and_partition_repair() {
    digest_session_scenario(true, false, false).await;
}
#[tokio::test]
async fn failed_digest_snapshot_preserves_replicas() {
    digest_session_scenario(false, true, false).await;
}

// Generated scenarios span the two real backends and transient refusal versus
// successful repair, checking convergence and the transfer boundary each time.
#[hegel::test]
fn generated_digest_session_repair(tc: hegel::TestCase) {
    let sqlite = tc.draw(hegel::generators::booleans());
    let failure = tc.draw(hegel::generators::booleans());
    let advanced_prefix = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(digest_session_scenario(
        sqlite,
        failure,
        advanced_prefix,
    ));
}

// A falsely advanced legacy prefix cannot hide a missing final tombstone from
// the digest safety net, even though reconnect can legitimately log-resume.
#[tokio::test]
async fn digest_repairs_a_relay_advanced_prefix() {
    digest_session_scenario(false, false, true).await;
}
