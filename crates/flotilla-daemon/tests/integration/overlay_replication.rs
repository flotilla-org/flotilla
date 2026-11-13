use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::runtime::DaemonRuntime;
use flotilla_daemon::runtime::RuntimeOptions;
use flotilla_daemon_testkit::server::spawn_in_memory_request_topology;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::{FleetStaleness, HostName, PeerConnectionState, QueryId, Relationship, SubjectKind};
use flotilla_resources::{
    terminal_records::{TerminalLaunchRequest, TerminalLaunchSource},
    Checkout, CheckoutSpec, Convoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, Host, HostSpec, HostStatus, InputMeta,
    ObservedCheckoutSpec, Project, ProjectSpec, RepositoryKey, ResourceProvenance, TerminalSession, TerminalSessionSource,
    TerminalSessionSpec,
};
use flotilla_store::{watch_resource_kind_including_replicas, ResourceBackend};
use futures::StreamExt;

fn config(path: std::path::PathBuf, machine_id: &str) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&path).expect("create config");
    std::fs::write(path.join("daemon.toml"), format!("machine_id = \"{machine_id}\"\n")).expect("write daemon config");
    Arc::new(ConfigStore::with_base(path))
}

async fn daemon(path: std::path::PathBuf, machine_id: &str, host: &str) -> Arc<InProcessDaemon> {
    daemon_with_backend(path, machine_id, host, ResourceBackend::new(InMemoryBackend::default())).await
}

async fn sqlite_daemon(path: std::path::PathBuf, machine_id: &str, host: &str) -> Arc<InProcessDaemon> {
    std::fs::create_dir_all(&path).expect("create sqlite daemon directory");
    let backend = ResourceBackend::new(SqliteBackend::open(path.join("resources.sqlite")).expect("open sqlite backend"));
    daemon_with_backend(path, machine_id, host, backend).await
}

async fn daemon_with_backend(path: std::path::PathBuf, machine_id: &str, host: &str, backend: ResourceBackend) -> Arc<InProcessDaemon> {
    InProcessDaemon::new_with_resource_backend(
        vec![],
        config(path, machine_id),
        fake_discovery(false),
        HostName::new(host),
        backend,
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await
}

/// Waits until `at` holds `home`'s own Host, written at its home and replicated through the
/// engine store's change stream, and returns it.
async fn await_host_replica(at: &InProcessDaemon, home: &InProcessDaemon) -> flotilla_resources::ResourceObject<Host> {
    let name = home.local_host_id().expect("home host id").to_string();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let hosts = at.resource_backend().including_replicas::<Host>("flotilla").list().await.expect("list host sources");
            if let Some(host) = hosts.items.into_iter().find(|host| {
                host.object.metadata.name == name
                    && matches!(&host.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == home.node_id())
            }) {
                break host.object;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} did not replicate its Host to {}", home.host_name(), at.host_name()))
}

// A host's self-report reaches its peers' fleet health. On the engine (ADR 0064) a Host keeps
// what decisions read: its generation, and when its home was last heard as its heartbeat. Its
// version, start time, free disk and memory are display figures and metrics, not recorded there.
#[tokio::test]
async fn sqlite_daemons_expose_remote_host_self_report_in_fleet_health() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = sqlite_daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = sqlite_daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let feta_host_id = feta.local_host_summary().await.environment_id.host_id().expect("feta host id").to_string();
    let reported_at = chrono::Utc::now();
    let feta_hosts = feta.resource_backend().using::<Host>("flotilla");
    let feta_host = feta_hosts
        .create(&InputMeta::builder().name(feta_host_id.clone()).build(), &HostSpec::default())
        .await
        .expect("create feta host self-report");
    feta_hosts
        .update_status(
            &feta_host_id,
            &feta_host.metadata.resource_version,
            &HostStatus {
                heartbeat_at: Some(reported_at),
                ready: true,
                daemon_generation: Some("feta-generation".to_string()),
                ..HostStatus::default()
            },
        )
        .await
        .expect("publish feta host self-report");

    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect sqlite daemons");
    let replicated = await_host_replica(&kiwi, &feta).await;
    assert_eq!(replicated.status.as_ref().and_then(|status| status.daemon_generation.as_deref()), Some("feta-generation"));

    let health = kiwi.fleet_health_internal().await.expect("query kiwi fleet health");
    let row = health.hosts.into_iter().find(|row| row.host == HostName::new("feta")).expect("feta fleet health row");

    assert_eq!(row.daemon_generation.as_deref(), Some("feta-generation"));
    assert!(row.heartbeat_at.is_some_and(|heard| heard >= reported_at), "the heartbeat is when feta was last heard: {row:?}");
    assert_eq!(row.link, PeerConnectionState::Connected);
    drop(topology);
}

#[tokio::test]
async fn fleet_list_and_health_include_replicated_remote_crew_without_snapshot_fetch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    // Each daemon reads the engine's environments and attentions, as its runtime installs them.
    crate::engine_environments(&kiwi);
    crate::engine_environments(&feta);
    kiwi.open_message_primitives().expect("kiwi message path");
    feta.open_message_primitives().expect("feta message path");
    let feta_host_id = feta.local_host_id().expect("feta host id").to_string();
    let feta_hosts = feta.resource_backend().using::<Host>("flotilla");
    let host =
        feta_hosts.create(&InputMeta::builder().name(feta_host_id.clone()).build(), &HostSpec::default()).await.expect("create feta host");
    feta_hosts
        .update_status(
            &feta_host_id,
            &host.metadata.resource_version,
            &HostStatus {
                heartbeat_at: Some(chrono::Utc::now()),
                daemon_generation: Some("feta-generation".to_string()),
                ready: true,
                ..HostStatus::default()
            },
        )
        .await
        .expect("publish feta heartbeat");
    // The convoy is feta's engine launch; the crew's session is feta's launch too, held for feta's
    // launch of the convoy's work.
    let convoy = crate::admit_convoy(
        &feta,
        flotilla_engine_convoys_testkit::convoy(
            "flotilla",
            "feta-convoy",
            ConvoySpec::builder().workflow_ref("workflow".to_string()).role("feta-convoy".to_string()).build(),
            flotilla_engine_convoys_testkit::single_agent("coder"),
        ),
    )
    .await;
    let vessel = crate::engine(&feta).vessels.request(&convoy, "work", &feta_host_id);
    crate::engine(&feta).terminals.request(&TerminalLaunchRequest {
        session: "feta-session".to_string(),
        environment: "feta-environment".to_string(),
        vessel: Some(vessel),
        role: "coder".to_string(),
        pool: "passthrough".to_string(),
        cwd: "/tmp".to_string(),
        env: Default::default(),
        source: TerminalLaunchSource::Tool { command: "true".to_string() },
    });

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
    // The Host travels the engine store's change stream, beside the old store's replication.
    await_host_replica(&kiwi, &feta).await;
    let list = kiwi.fleet_list_internal().await.expect("fleet list");
    let row = list.rows.iter().find(|row| row.session.as_deref() == Some("feta-session")).expect("feta crew row");
    assert_eq!(row.host, HostName::new("feta"));
    assert_eq!(row.crew, "work/coder");
    assert_eq!(row.convoy, convoy);
    assert!(matches!(row.staleness, FleetStaleness::Fresh { .. }));
    let health = kiwi.fleet_health_internal().await.expect("fleet health");
    let feta_health = health.hosts.iter().find(|row| row.host == HostName::new("feta")).expect("feta health");
    assert_eq!(feta_health.crew_count, 1);
    assert_eq!(feta_health.convoy_count, 1);
    assert_eq!(feta_health.replica_generation.as_deref(), Some("feta-generation"));
    // #742: the aliases formerly indexed from snapshot rows remain attachable.
    std::fs::write(kiwi.config_store().base_path().join("hosts.toml"), "[hosts.feta]\nhostname = 'feta.example'\n")
        .expect("attach hop config");
    let crew_path = format!("{convoy}/work/coder");
    for reference in ["feta-session", crew_path.as_str(), "work/coder", "work", "coder", "feta-environment"] {
        let resolved =
            kiwi.resolve_attach_command_on_host_internal(reference, Some(&HostName::new("feta"))).await.expect("remote crew alias");
        let binding = resolved.binding.expect("crew binding");
        assert_eq!(binding.host, HostName::new("feta"));
        assert_eq!(binding.session.as_deref(), Some("feta-session"));
        assert_eq!(binding.convoy.as_deref(), Some(convoy.as_str()));
        assert_eq!(binding.vessel.as_deref(), Some("work"));
        assert_eq!(binding.role.as_deref(), Some("coder"));
    }
    drop(topology);
}

#[tokio::test]
async fn pr_observed_after_admission_becomes_home_subject_and_replica_fact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let repository_url = "https://github.com/flotilla-org/flotilla";
    let _kiwi_runtime = DaemonRuntime::start_with_options(
        Arc::clone(&kiwi),
        kiwi.config_store(),
        None,
        RuntimeOptions { namespace: "flotilla".into(), start_controllers: false, ..RuntimeOptions::default() },
    )
    .await
    .expect("start other host query projection");
    let repository_key = RepositoryKey("repo_flotilla".into());
    let home = feta.resource_backend();
    // The convoy is feta's engine launch, active before any PR exists.
    let convoy = crate::admit_convoy(
        &feta,
        flotilla_engine_convoys_testkit::convoy(
            "flotilla",
            "crew-convoy",
            ConvoySpec::builder()
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
            ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                observed_workflow_ref: Some("workflow".into()),
                ..flotilla_engine_convoys_testkit::single_agent("coder")
            },
        ),
    )
    .await;
    let _feta_runtime = DaemonRuntime::start_with_options(
        Arc::clone(&feta),
        feta.config_store(),
        None,
        RuntimeOptions { namespace: "flotilla".into(), controller_resync_interval: Duration::from_secs(300), ..RuntimeOptions::default() },
    )
    .await
    .expect("start home controller before crew opens PR");
    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect hosts");
    // The forge observes change request 2301 open; the crew's checkout, held for the convoy on
    // the engine at its home, is inspected as associated with it. Neither changes the convoy's
    // phase: the checkout's change wakes the convoy.
    let now = chrono::Utc::now();
    let change_requests = home.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let record_name = flotilla_resources::address::record_name(
        &flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 2301, &[])
            .expect("change request address"),
    )
    .expect("record name");
    let record = change_requests
        .create(
            &InputMeta::builder().name(record_name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(2301)
                .observing_authority("feta".to_string())
                .build(),
        )
        .await
        .expect("forge observes the change request");
    change_requests
        .update_status(
            &record_name,
            &record.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, now),
                head_sha: flotilla_resources::Observation::known("abc".to_string(), now),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, now),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, now),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, now),
            },
        )
        .await
        .expect("change request is open");
    use flotilla_resources::checkout_records::CheckoutRecords as _;
    let engine = flotilla_daemon_testkit::engine::runtime_checkouts(&feta);
    let launch = engine.put_ready(
        "crew-checkout",
        &CheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: "fix/crew-branch".into(),
            path: "/srv/crew/flotilla".into(),
            repo_ref: repository_key,
            host_ref: "feta".into(),
            is_main: false,
        }),
        Some(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(convoy.clone())),
    );
    engine
        .checkouts
        .inspected(&launch, flotilla_resources::checkout_records::Inspection { change_request: Some(2301), ..Default::default() })
        .expect("crew opens PR without changing convoy phase");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if kiwi.resource_backend().including_replicas::<Convoy>("flotilla").get(&convoy).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("other host receives convoy replica");
    kiwi.discover_convoy_branch_subjects("flotilla", &convoy, "fix/crew-branch").await.expect("replica host skips home-only discovery");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let replica = kiwi.resource_backend().including_replicas::<Convoy>("flotilla").get(&convoy).await;
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
                .any(|row| row.resource.name == convoy && row.subjects.iter().any(|entry| entry.subject.id == "2301"))
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
    let mut raw_project_watch = watch_resource_kind_including_replicas(&feta.resource_backend(), "flotilla", "projects")
        .await
        .expect("watch every home's Projects");
    assert!(
        tokio::time::timeout(Duration::from_millis(150), raw_project_watch.stream.next()).await.is_err(),
        "peers must not echo an unchanged third-origin Project indefinitely"
    );
    drop(feta_gouda);
}

// #742: another host's adopted checkouts and independent attach targets reach it by following
// its log, including updates and removals.
#[tokio::test]
async fn remote_checkout_queries_and_independent_attach_targets_follow_their_hosts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    let _runtime = DaemonRuntime::start_with_options(
        Arc::clone(&kiwi),
        kiwi.config_store(),
        None,
        RuntimeOptions { namespace: "flotilla".into(), start_controllers: false, ..RuntimeOptions::default() },
    )
    .await
    .expect("start query projection");
    let checkouts = flotilla_daemon_testkit::engine::runtime_checkouts(&feta);
    let adopted = |branch: &str| {
        CheckoutSpec::Observed(ObservedCheckoutSpec {
            repo_ref: RepositoryKey("widgets".into()),
            path: "/srv/widgets".into(),
            r#ref: branch.into(),
            host_ref: "feta".into(),
            is_main: true,
        })
    };
    let spec = adopted("main");
    let mut launch = checkouts.put_ready("checkout", &spec, None);
    // Identical names on different origins remain distinct rows.
    let CheckoutSpec::Observed(discovered) = spec.clone() else { unreachable!("adopted checkouts are observed") };
    kiwi.discovered_checkouts().replace(
        &discovered.repo_ref.clone(),
        vec![flotilla_resources::discovered_checkouts::DiscoveredCheckout {
            name: "checkout".to_string(),
            repo_slug: "widgets".to_string(),
            spec: discovered,
        }],
    );
    let independent = flotilla_daemon_testkit::engine::run_session(
        &feta,
        "terminal-independent",
        &TerminalSessionSpec::builder()
            .env_ref(feta.local_environment_id().to_string())
            .role("shell".to_string())
            .source(TerminalSessionSource::Tool { command: "sh".into() })
            .cwd("/srv/widgets".to_string())
            .pool("passthrough".to_string())
            .build(),
        &Default::default(),
    );
    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect daemons");
    let state = kiwi.aggregator_projection_state().await;
    for branch in ["main", "feature", "main"] {
        // A checkout's spec is its launch's: the revision is the checkout's next launch.
        checkouts.finish(&launch, flotilla_provisioning_testkit::Ending::Released);
        launch = checkouts.put_ready("checkout", &adopted(branch), None);
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
    std::fs::write(kiwi.config_store().base_path().join("hosts.toml"), "[hosts.feta]\nhostname = 'feta.example'\n")
        .expect("attach hop config");
    let resolved = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(resolved) = kiwi.resolve_attach_command_on_host_internal("terminal-independent", Some(&HostName::new("feta"))).await {
                break resolved;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("remote independent attach");
    assert_eq!(resolved.binding.as_ref().map(|binding| &binding.host), Some(&HostName::new("feta")));
    let checkout_attach =
        kiwi.resolve_transient_attach_command_internal("/srv/widgets", Some(&HostName::new("feta"))).await.expect("remote checkout attach");
    // A replicated checkout keeps its owning host for viewer-side routing,
    // without claiming a durable terminal session or resource identity.
    let binding = checkout_attach.binding.as_ref().expect("checkout routing binding");
    assert_eq!(binding.host, HostName::new("feta"));
    assert_eq!(binding.namespace, "flotilla");
    assert_eq!(binding.session, None);
    assert_eq!(binding.resource_ref(), None);
    assert!(serde_json::to_string(&checkout_attach.plan).expect("attach plan").contains("/srv/widgets"));
    // kiwi reads feta's checkout from the engine, homed on feta; kiwi holds none of its own.
    let sources = kiwi.resource_backend().including_replicas::<Checkout>("flotilla").list().await.expect("checkout sources");
    assert!(!sources.items.is_empty(), "kiwi reads feta's checkout");
    assert!(
        sources
            .items
            .iter()
            .all(|source| matches!(&source.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == feta.node_id())),
        "every checkout kiwi lists is homed on feta"
    );
    assert!(kiwi.resource_backend().using::<Checkout>("flotilla").list().await.expect("local checkouts").items.is_empty());
    checkouts.finish(&launch, flotilla_provisioning_testkit::Ending::Released);
    flotilla_daemon_testkit::engine::engine(&feta).terminals.release(&independent);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let set = state.result_set_for(&QueryId::Checkouts { scope: None }).await.expect("checkout query");
            let rows = set.rows.as_checkouts().expect("checkout rows");
            let detached =
                kiwi.resolve_attach_command_on_host_internal("terminal-independent", Some(&HostName::new("feta"))).await.is_err();
            if rows.len() == 1 && rows[0].host == HostName::new("kiwi") && detached {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("remote observations removed");
    drop(topology);
}

// #4127: attach routes by the host of the crew's TerminalSession, never by the
// Convoy's home. The Convoy is homed on kiwi while its crew runs on feta.
#[tokio::test]
async fn attach_routes_by_session_host_when_convoy_is_homed_elsewhere() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kiwi = daemon(temp.path().join("kiwi"), "kiwi-root", "kiwi").await;
    let feta = daemon(temp.path().join("feta"), "feta-root", "feta").await;
    // Each daemon reads the engine's environments, as its runtime installs them.
    crate::engine_environments(&kiwi);
    let feta_environments = crate::engine_environments(&feta);
    let feta_host_id = feta.local_host_id().expect("feta host id").to_string();
    let feta_hosts = feta.resource_backend().using::<Host>("flotilla");
    let host = feta_hosts.create(&InputMeta::builder().name(feta_host_id.clone()).build(), &HostSpec::default()).await.expect("feta host");
    feta_hosts
        .update_status(
            &feta_host_id,
            &host.metadata.resource_version,
            &HostStatus { heartbeat_at: Some(chrono::Utc::now()), ready: true, ..HostStatus::default() },
        )
        .await
        .expect("feta heartbeat");
    feta_environments.request(
        "feta-environment",
        &flotilla_resources::EnvironmentSpec {
            host_direct: Some(flotilla_resources::HostDirectEnvironmentSpec {
                host_ref: feta_host_id.clone(),
                repo_default_dir: "/tmp".to_string(),
            }),
            ..Default::default()
        },
        None,
    );
    let mut spec = ConvoySpec::builder().workflow_ref("workflow".to_string()).role("coder".to_string()).build();
    spec.project_ref = Some("flotilla".to_string());
    let convoy = crate::admit_convoy(
        &kiwi,
        flotilla_engine_convoys_testkit::convoy(
            "flotilla",
            "convoy-homed-on-kiwi",
            spec,
            flotilla_engine_convoys_testkit::single_agent("coder"),
        ),
    )
    .await;
    // The crew runs on feta: its session is feta's engine launch, held for feta's launch of work
    // `work` in the convoy homed at kiwi.
    let vessel = crate::engine(&feta).vessels.request_for(&flotilla_resources::vessel_records::VesselLaunchRequest {
        convoy: convoy.clone(),
        convoy_home: kiwi.node_id().as_str().to_string(),
        work: "work".to_string(),
        host: feta_host_id.clone(),
    });
    let terminals = &crate::engine(&feta).terminals;
    let launch = terminals.request(&TerminalLaunchRequest {
        session: "crew-session".to_string(),
        environment: "feta-environment".to_string(),
        vessel: Some(vessel),
        role: "coder".to_string(),
        pool: "passthrough".to_string(),
        cwd: "/tmp".to_string(),
        env: Default::default(),
        source: TerminalLaunchSource::Tool { command: "true".to_string() },
    });
    terminals.ready(&launch, "session-id", &Default::default());

    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect daemons");
    std::fs::write(kiwi.config_store().base_path().join("hosts.toml"), "[hosts.feta]\nhostname = 'feta.example'\n").expect("hop config");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let sessions = kiwi.resource_backend().including_replicas::<TerminalSession>("flotilla").list().await.expect("sessions");
            let convoys = feta.resource_backend().including_replicas::<Convoy>("flotilla").list().await.expect("convoys");
            if !sessions.items.is_empty() && !convoys.items.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replication");

    let feta_name = HostName::new("feta");
    for reference in ["coder@flotilla", convoy.as_str()] {
        // From the Convoy's home, with and without an explicit session host.
        for host in [None, Some(&feta_name)] {
            let resolved =
                kiwi.resolve_attach_command_on_host_internal(reference, host).await.unwrap_or_else(|e| panic!("{reference}: {e}"));
            let binding = resolved.binding.expect("binding");
            assert_eq!(binding.host, feta_name, "{reference}");
            assert_eq!(binding.convoy.as_deref(), Some(convoy.as_str()));
        }
        // From the session's own host the session is local.
        let resolved = feta.resolve_attach_command_internal(reference).await.unwrap_or_else(|e| panic!("{reference}: {e}"));
        assert_eq!(resolved.binding.expect("binding").session.as_deref(), Some("crew-session"));
    }
    // The Convoy's home is not an attach host.
    assert!(kiwi.resolve_attach_command_on_host_internal("coder@flotilla", Some(&HostName::new("kiwi"))).await.is_err());
    drop(topology);
}

// Governor relay contract (moved from server::replication::relay_tests, #3108): a workflow
// template authored at feta reaches udder through kiwi with feta as its origin, and admission
// freezes what was relayed. Only feta--kiwi and kiwi--udder are linked.
mod governor_relay {
    use std::collections::BTreeSet;

    use flotilla_daemon_api::daemon::DaemonHandle;
    use flotilla_protocol::{Command, CommandAction, CommandValue, ConvoyAutoAttach, ConvoyStartIntent, DaemonEvent};
    use flotilla_resources::{
        single_agent_workflow_spec, CrewSource, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, ResourceObject,
        WorkflowTemplate, WorkflowTemplateSpec, ENVIRONMENT_PROVIDERS_CAPABILITY, WORKFLOW_SNAPSHOT_ANNOTATION,
    };

    use super::*;

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
        // An ended launch stays on the engine, so the governor is the one live launch.
        let mut live = convoys.items.into_iter().filter(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal()));
        let convoy = live.next().expect("admitted governor");
        assert!(live.next().is_none(), "one live governor");
        let snapshot = backend
            .definitions::<WorkflowTemplate>("flotilla")
            .get(&convoy.metadata.annotations[WORKFLOW_SNAPSHOT_ANNOTATION])
            .await
            .expect("admission's immutable workflow snapshot");
        (convoy, snapshot.spec)
    }

    async fn register_host_direct_kind(daemon: &InProcessDaemon) {
        let backend = daemon.resource_backend();
        let host_id = daemon.local_host_id().expect("local host id").to_string();
        let hosts = backend.using::<Host>("flotilla");
        let host = hosts.create(&InputMeta::builder().name(host_id.clone()).build(), &HostSpec::default()).await.expect("admission host");
        hosts
            .update_status(
                &host_id,
                &host.metadata.resource_version,
                &HostStatus {
                    capabilities: BTreeMap::from([(
                        ENVIRONMENT_PROVIDERS_CAPABILITY.to_string(),
                        serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                    )]),
                    heartbeat_at: Some(chrono::Utc::now()),
                    ready: true,
                    disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
                    admission_free_space_floor_bytes: Some(0),
                    ..HostStatus::default()
                },
            )
            .await
            .expect("ready admission host");
        backend
            .using::<FulfilmentKind>("flotilla")
            .create(
                &InputMeta::builder().name(format!("host-direct-{host_id}")).build(),
                &FulfilmentKindSpec::builder()
                    .host_ref(host_id)
                    .pool("cleat".to_string())
                    .grants(BTreeSet::from([FulfilmentGrant::platform("linux".to_string()), FulfilmentGrant::host_account_reach()]))
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
            )
            .await
            .expect("admission fulfilment kind");
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
        let mut roots = Vec::new();
        for host in ["feta", "kiwi", "udder"] {
            let config_path = temp.path().join(host);
            std::fs::create_dir_all(&config_path).expect("config directory");
            std::fs::write(config_path.join("daemon.toml"), format!("machine_id = \"{host}\"\n[admission]\nfree_space_floor_gib = 0\n"))
                .expect("machine identity");
            let config = Arc::new(ConfigStore::with_base(config_path));
            let backend = match storage {
                TestBackend::Sqlite => {
                    ResourceBackend::new(SqliteBackend::open(temp.path().join(format!("{host}.sqlite"))).expect("open store"))
                }
                TestBackend::InMemory => ResourceBackend::new(InMemoryBackend::default()),
            };
            let daemon = InProcessDaemon::new_with_resource_backend(
                vec![],
                config,
                fake_discovery(false),
                HostName::new(host),
                backend,
                std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
            )
            .await;
            roots.push((daemon, ()));
        }
        let feta = &roots[0].0;
        let kiwi = &roots[1].0;
        let udder = &roots[2].0;
        // Each root reads its convoys from the engine, as its runtime installs the readings.
        for (root, ()) in &roots {
            crate::engine(root);
        }
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
        register_host_direct_kind(udder).await;

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

        // Only A--B and B--C exist: feta's records reach udder only as kiwi passes them on.
        let _line = flotilla_daemon_testkit::server::spawn_in_memory_request_line(vec![
            Arc::clone(&roots[0].0),
            Arc::clone(&roots[1].0),
            Arc::clone(&roots[2].0),
        ])
        .await
        .expect("connect feta--kiwi--udder");
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
        // A convoy ends on the engine and is never deleted: the first admission is retired by abandoning it.
        udder
            .convoy_engine_path()
            .get()
            .expect("udder's Convoy path")
            .abandon(&first_convoy, "operator", "retire first admission")
            .await
            .expect("retire first admission");
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
}
