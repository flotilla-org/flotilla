use flotilla_resources::convoy_address::convoy_record_name;
use flotilla_resources::parse_role_address;
use flotilla_store_memory::InMemoryBackend;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_protocol::{HostName, PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    CapabilityNeed, Convoy as ResourceConvoy, ConvoyPhase, ConvoySpec, ConvoyStatus, FulfilmentFacts, FulfilmentGrant, FulfilmentKind,
    FulfilmentKindSpec, FulfilmentRealisation, HarnessFacts, Host as ResourceHost, HostSpec, HostStatus, InputMeta, WorkflowTemplateSpec,
    GENERATION_LABEL, PROJECT_LABEL, ROLE_LABEL,
};
use flotilla_store::ResourceBackend;

use super::support::{create_docker_placement, create_identity_convoy, host_direct_kind, test_meta};
use crate::in_process::{placement_target_host, InProcessDaemon};
use crate::testkits::discovery::fake_discovery;
use flotilla_admission::convoy_admission::allocate_convoy_generation;
use flotilla_crew::crew_ops::resolve_local_convoy_name;

#[test]
fn convoy_role_addresses_reject_malformed_values() {
    assert_eq!(parse_role_address("reviewer"), Ok(("reviewer", None)));
    assert_eq!(parse_role_address("reviewer@flotilla"), Ok(("reviewer", Some("flotilla"))));
    assert_eq!(parse_role_address("reviewer@"), Ok(("reviewer", Some(""))));
    for value in ["@project", "a@b@c"] {
        assert_eq!(parse_role_address(value), Err(format!("invalid convoy address `{value}`: expected role@project")));
    }
    assert_eq!(parse_role_address(""), Err("convoy role cannot be empty".to_string()));
}

#[tokio::test]
async fn convoy_role_resolution_can_disambiguate_a_projectless_convoy() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    create_identity_convoy(&backend, "convoy-one", "reviewer", None).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("beta")).await;

    assert_eq!(
        resolve_local_convoy_name(&backend, "flotilla", "reviewer").await,
        Err("convoy role `reviewer` is ambiguous; use one of: reviewer@, reviewer@beta".to_string())
    );
    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "reviewer@").await, Ok("convoy-one".to_string()));
    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "reviewer@beta").await, Ok("convoy-two".to_string()));
}

#[tokio::test]
async fn convoy_resolution_falls_back_to_a_unique_terminal_generation() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    create_identity_convoy(&backend, "convoy-one", "reviewer", Some("flotilla")).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys.get("convoy-one").await.expect("terminal convoy");
    convoys
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() },
        )
        .await
        .expect("mark convoy terminal");

    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "reviewer@flotilla").await, Ok("convoy-one".to_string()));
}

#[tokio::test]
async fn convoy_resolution_refuses_multiple_terminal_generations() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    create_identity_convoy(&backend, "convoy-one", "reviewer", Some("flotilla")).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("flotilla")).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    for name in ["convoy-one", "convoy-two"] {
        let created = convoys.get(name).await.expect("terminal convoy");
        convoys
            .update_status(
                &created.metadata.name,
                &created.metadata.resource_version,
                &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() },
            )
            .await
            .expect("mark convoy terminal");
    }

    assert_eq!(
        resolve_local_convoy_name(&backend, "flotilla", "reviewer@flotilla").await,
        Err("convoy address `reviewer@flotilla` matches multiple terminal records; use an exact record name: convoy-one, convoy-two"
            .to_string())
    );
}

#[tokio::test]
async fn convoy_resolution_prefers_an_exact_unlabelled_record_name() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("pre-identity-record".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
        )
        .await
        .expect("pre-identity convoy");
    convoys
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() },
        )
        .await
        .expect("mark convoy terminal");

    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "pre-identity-record").await, Ok("pre-identity-record".to_string()));
}

#[tokio::test]
async fn projectless_convoys_do_not_share_an_identity_bucket_with_a_project_named_standalone() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let record = convoy_record_name();
    let generation = allocate_convoy_generation(&backend, "flotilla", None, "worker").await.expect("projectless identity");
    let labels = BTreeMap::from([
        (PROJECT_LABEL.to_string(), String::new()),
        (ROLE_LABEL.to_string(), "worker".to_string()),
        (GENERATION_LABEL.to_string(), generation.to_string()),
    ]);
    let spec = ConvoySpec::builder().workflow_ref("work".to_string()).role("worker".to_string()).generation(generation).build();
    convoys.create(&InputMeta::builder().name(record).labels(labels).build(), &spec).await.expect("projectless convoy");

    assert!(allocate_convoy_generation(&backend, "flotilla", Some("standalone"), "worker").await.is_ok());
}

#[tokio::test]
async fn capability_admission_resolves_display_name_kind_and_policy_host_refs() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"canonical-admission-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("udder"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let host_id = daemon.local_host_id().expect("host id").to_string();
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta(&host_id), &HostSpec { display_name: "udder".into(), connection: Default::default(), ..HostSpec::default() })
        .await
        .expect("host");
    hosts
        .update_status(
            &host_id,
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: [(
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                )]
                .into_iter()
                .collect(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                fulfilment_facts: BTreeMap::from([(
                    "udder-kind".into(),
                    FulfilmentFacts { gui_session_logged_in: true, observed_at: Utc::now(), ..Default::default() },
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("host facts");
    for name in ["collision-a", "collision-b"] {
        hosts
            .create(&test_meta(name), &HostSpec { display_name: "collision".into(), connection: Default::default(), ..HostSpec::default() })
            .await
            .expect("ambiguous host");
    }
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta("a-collision"),
            &FulfilmentKindSpec::builder()
                .host_ref("collision".to_string())
                .pool("passthrough".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::gui_session()]))
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("ambiguous kind");
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta("udder-kind"),
            &FulfilmentKindSpec::builder()
                .host_ref("udder".to_string())
                .pool("passthrough".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::gui_session()]))
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("legacy kind");

    let (placement, _) = daemon
        .resolve_capability_placement(
            "flotilla",
            "project",
            &[],
            &WorkflowTemplateSpec::builder().vessels(Vec::new()).build(),
            &BTreeSet::from([CapabilityNeed::GuiSession]),
            &flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".to_string()).build(),
        )
        .await
        .expect("display-name kind should admit");
    let allocation = placement.allocation.expect("allocation");
    assert_eq!(allocation.candidates[0].host, host_id);
    assert!(allocation.candidates[0].host_ready);
}

#[tokio::test]
async fn fulfilment_list_joins_host_facts_and_fleet_health_shows_local_kinds() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"fulfilment-join\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host ID").to_string();
    let kinds = backend.clone().using::<FulfilmentKind>("flotilla");
    for (name, host_ref) in [("local-kind", host_id.as_str()), ("unmatched-kind", "absent-host")] {
        kinds
            .create(
                &test_meta(name),
                &FulfilmentKindSpec::builder()
                    .host_ref(host_ref.to_string())
                    .pool("cleat".to_string())
                    .grants(BTreeSet::from([FulfilmentGrant::platform("linux".to_string())]))
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
            )
            .await
            .expect("create kind");
    }
    let before = daemon.fulfilment_list_internal().await.expect("list before heartbeat");
    assert_eq!(before.kinds.len(), 2);
    assert!(before.kinds.iter().all(|kind| kind.harnesses.is_empty() && kind.gui_session_logged_in.is_none()));

    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta(&host_id),
            &HostSpec { display_name: "local-host".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("create host");
    hosts
        .update_status(
            &host_id,
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: [(
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                )]
                .into_iter()
                .collect(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                fulfilment_facts: BTreeMap::from([(
                    "local-kind".to_string(),
                    FulfilmentFacts {
                        harnesses: BTreeMap::from([(
                            "claude-code".to_string(),
                            HarnessFacts { version: "2.1.282".to_string(), ..Default::default() },
                        )]),
                        gui_session_logged_in: true,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("publish facts");
    let listed = daemon.fulfilment_list_internal().await.expect("list facts");
    assert_eq!(listed.kinds.iter().find(|kind| kind.name == "local-kind").expect("local kind").harnesses["claude-code"].version, "2.1.282");
    assert!(listed.kinds.iter().find(|kind| kind.name == "unmatched-kind").expect("unmatched kind").harnesses.is_empty());
    let fleet = daemon.fleet_health_internal().await.expect("fleet health");
    let local = fleet.hosts.iter().find(|host| host.host == HostName::new("local-host")).expect("local host row");
    assert_eq!(local.fulfilments.len(), 1);
    assert_eq!(local.fulfilments[0].name, "local-kind");
    assert_eq!(local.fulfilments[0].gui_session_logged_in, Some(true));
}

#[tokio::test]
async fn self_targeted_admission_uses_live_local_host_over_stale_self_origin_replica() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host identity").to_string();

    let stale_source = ResourceBackend::new(InMemoryBackend::default());
    stale_source
        .using::<ResourceHost>("flotilla")
        .create(
            &test_meta(&host_id),
            &HostSpec { display_name: "local-host".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("stale self-origin host");
    backend
        .replica_writer::<ResourceHost>(daemon.node_id.clone(), "flotilla")
        .replace(&stale_source.using::<ResourceHost>("flotilla").list().await.expect("stale host list"), Utc::now())
        .await
        .expect("seed stale self-origin replica");

    let hosts = backend.using::<ResourceHost>("flotilla");
    let local = hosts
        .create(
            &test_meta(&host_id),
            &HostSpec { display_name: "local-host".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("authoritative local host");
    hosts
        .update_status(
            &host_id,
            &local.metadata.resource_version,
            &HostStatus {
                capabilities: [(
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                )]
                .into_iter()
                .collect(),
                disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
                admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
                ..HostStatus::default()
            },
        )
        .await
        .expect("publish live local capacity");
    host_direct_kind(&backend, "self-targeted", &host_id).await;

    daemon
        .check_remote_placement_free_space_floor(
            "flotilla",
            Some(&PlacementDecision {
                provider_instance: None,
                provider_workspace_root: None,
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                kind: "self-targeted".to_string(),
                target_host: PlacementTargetHost {
                    reference: flotilla_protocol::CanonicalHostId::resolved(host_id),
                    display_name: "local-host".to_string(),
                },
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            }),
        )
        .await
        .expect("healthy authoritative local capacity should admit self-targeted placement");
}

#[tokio::test]
async fn resource_host_routing_refuses_unresolved_host_ref() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        ResourceBackend::new(InMemoryBackend::default()),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;

    let error = daemon
        .target_host_for_resource_ref("flotilla", "unregistered-host-id")
        .await
        .expect_err("unknown host refs must not cross the canonical identity boundary");

    assert_eq!(error, "references unknown host `unregistered-host-id`");
}

#[tokio::test]
async fn self_targeted_admission_resolves_display_name_policy_to_live_local_host() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host identity").to_string();
    let hosts = backend.using::<ResourceHost>("flotilla");
    let local = hosts
        .create(
            &test_meta(&host_id),
            &HostSpec { display_name: "local-host".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("authoritative local host");
    hosts
        .update_status(
            &host_id,
            &local.metadata.resource_version,
            &HostStatus {
                capabilities: [(
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                )]
                .into_iter()
                .collect(),
                disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
                admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
                ..HostStatus::default()
            },
        )
        .await
        .expect("publish live local capacity");
    let policy = host_direct_kind(&backend, "self-targeted", "local-host").await;

    let target = placement_target_host(&backend, "flotilla", &policy).await.expect("resolve display-name host reference");
    assert_eq!(target.reference.as_str(), host_id);
    assert_eq!(daemon.remote_placement_host("flotilla", Some("self-targeted")).await.expect("resolve host-direct routing"), None);
    daemon
        .check_remote_placement_free_space_floor(
            "flotilla",
            Some(&PlacementDecision {
                provider_instance: None,
                provider_workspace_root: None,
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                kind: "self-targeted".to_string(),
                target_host: target,
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            }),
        )
        .await
        .expect("healthy authoritative local capacity should admit self-targeted placement");
}

#[tokio::test]
async fn capability_admission_places_on_ssh_hosts_only_when_pinned() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"ssh-host-owner\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("udder"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta("ssh-host"),
            &HostSpec {
                display_name: "beaufort".into(),
                connection: flotilla_resources::HostConnection::SshHost {
                    owning_daemon: "ssh-host-owner".into(),
                    destination: "crew@beaufort.example".into(),
                },
                ..HostSpec::default()
            },
        )
        .await
        .expect("SSH host");
    hosts
        .update_status(
            "ssh-host",
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: [(
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                )]
                .into_iter()
                .collect(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                fulfilment_facts: BTreeMap::from([(
                    "host-direct-ssh-host".into(),
                    FulfilmentFacts { observed_at: Utc::now(), ..Default::default() },
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("SSH host facts");
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta("host-direct-ssh-host"),
            &FulfilmentKindSpec::builder()
                .host_ref("ssh-host".to_string())
                .pool("cleat".to_string())
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("SSH host kind");
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let unpinned = flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".to_string()).build();
    let pinned = flotilla_protocol::ConvoyStartIntent::builder()
        .project_ref("project".to_string())
        .placement_policy("host-direct-ssh-host".to_string())
        .build();

    let refused = match daemon.resolve_capability_placement("flotilla", "project", &[], &workflow, &BTreeSet::new(), &unpinned).await {
        Ok((placement, _)) => {
            assert!(placement.selected.is_none(), "an unpinned convoy must not land on an SSH host");
            placement.refused_candidates.into_iter().map(|candidate| candidate.reason).collect::<Vec<_>>().join("\n")
        }
        Err(error) => error,
    };
    assert!(refused.contains("SSH host takes only convoys that pin its placement"), "{refused}");

    let (placement, _) = daemon
        .resolve_capability_placement("flotilla", "project", &[], &workflow, &BTreeSet::new(), &pinned)
        .await
        .expect("a pinned convoy is admitted");
    assert_eq!(placement.selected.expect("pinned placement").metadata.name, "host-direct-ssh-host");

    let refused = daemon
        .resolve_capability_placement(
            "flotilla",
            "project",
            &[],
            &flotilla_resources::single_agent_workflow_spec(),
            &BTreeSet::new(),
            &pinned,
        )
        .await
        .expect_err("pinning does not let an SSH host run an agent it lacks");
    assert!(refused.contains("agent adapter `codex`"), "{refused}");
}

struct NearlyFullDisk;

impl flotilla_provider_runtime::available_space::AvailableSpaceProbe for NearlyFullDisk {
    fn measure(&self, _path: &std::path::Path) -> Option<u64> {
        Some(1024 * 1024 * 1024)
    }
}

#[tokio::test]
async fn admitting_host_floor_gates_only_work_that_lands_on_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"nearly-full-admitter\"\n").expect("daemon config");
    let mut discovery = fake_discovery(false);
    discovery.available_space_probe = Arc::new(NearlyFullDisk);
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::new("udder"),
        ResourceBackend::new(InMemoryBackend::default()),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let local = daemon.local_host_id().expect("host id").to_string();
    let decision = |host: &str| PlacementDecision {
        provider_instance: None,
        provider_workspace_root: None,
        kind: format!("host-direct-{host}"),
        target_host: PlacementTargetHost { reference: flotilla_protocol::CanonicalHostId::resolved(host), display_name: host.into() },
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: None,
    };
    let remote = decision("b1");
    let here = decision(&local);

    daemon.admission.check_admitting_host_floor([&remote]).await.expect("remote work is judged by its target's floor");
    let refused = daemon.admission.check_admitting_host_floor([&remote, &here]).await.expect_err("a vessel lands on the admitter");
    assert!(refused.contains("below the 20.0 GiB floor"), "{refused}");
    daemon.admission.check_admitting_host_floor([]).await.expect_err("work without a placement decision defaults to the admitter");
}

// Health uses the event-maintained Aggregator projection, never Convoy bodies.
// Background watchers are excluded: this counts this request's reads only.
#[tokio::test]
async fn fleet_health_does_not_materialize_convoy_inventory() {
    use flotilla_store_testkit::{measure_reads, ReadCountsBackendExt, ReadOperation};
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"health-cost\"\n").unwrap();
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
    let remote = ResourceBackend::new(InMemoryBackend::default());
    for index in 0..320 {
        let records = remote.using::<ResourceConvoy>("flotilla");
        let name = format!("remote-{index}");
        let object = records.create(&test_meta(&name), &ConvoySpec::builder().workflow_ref("scratch".into()).build()).await.unwrap();
        records
            .update_status(
                &name,
                &object.metadata.resource_version,
                &ConvoyStatus { message: Some("x".repeat(30_000)), ..Default::default() },
            )
            .await
            .unwrap();
    }
    backend
        .replica_writer::<ResourceConvoy>(flotilla_protocol::NodeId::new("remote"), "flotilla")
        .replace(&remote.using::<ResourceConvoy>("flotilla").list().await.unwrap(), Utc::now())
        .await
        .unwrap();
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
        Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    for _ in 0..2 {
        let (response, reads) = measure_reads(daemon.fleet_health_internal()).await;
        response.unwrap();
        let reads = reads.of(&backend);
        for operation in [ReadOperation::Get, ReadOperation::List, ReadOperation::Query] {
            assert_eq!(reads.calls("Convoy", operation), 0, "health must not read Convoys: {reads}");
        }
    }
}

// Independent in-process daemon harnesses must not compete for the production
// process-wide eight-call admission budget. Hold nine real checks in-flight
// until every injected measurement has started, then release them together.
#[tokio::test]
async fn parallel_daemons_have_independent_free_space_probe_capacity() {
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    struct HeldSpaceProbe {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }
    impl flotilla_provider_runtime::available_space::AvailableSpaceProbe for HeldSpaceProbe {
        fn measure(&self, _path: &std::path::Path) -> Option<u64> {
            self.entered.send(()).unwrap();
            let (lock, ready) = &*self.gate;
            let released = lock.lock().unwrap();
            // Bound cleanup even if a regression prevents all callers entering.
            let _guard = ready.wait_timeout_while(released, Duration::from_secs(10), |released| !*released).unwrap();
            Some(100 * 1024 * 1024 * 1024)
        }
    }

    let (entered, mut started) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let mut daemons = Vec::new();
    let mut dirs = Vec::new();
    for index in 0..9 {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("daemon.toml"), format!("machine_id = \"probe-host-{index}\"\n")).unwrap();
        let mut discovery = if index % 2 == 0 {
            fake_discovery(false)
        } else {
            flotilla_discovery_testkit::fake_discovery_with_provider_set(Default::default())
        };
        discovery.available_space_probe = Arc::new(HeldSpaceProbe { entered: entered.clone(), gate: Arc::clone(&gate) });
        daemons.push(Arc::new(
            InProcessDaemon::new_with_resource_backend(
                Vec::new(),
                Arc::new(ConfigStore::with_base(temp.path())),
                discovery,
                HostName::new(format!("host-{index}")),
                ResourceBackend::new(InMemoryBackend::default()),
                Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
            )
            .await,
        ));
        dirs.push(temp);
    }
    // Cover both admission routes and both shared harness constructors.
    for report_only in [false, true] {
        *gate.0.lock().unwrap() = false;
        let mut checks = tokio::task::JoinSet::new();
        for daemon in &daemons {
            let daemon = Arc::clone(daemon);
            checks.spawn(async move {
                if report_only {
                    daemon.admission.free_space_bytes().await.map(|bytes| {
                        assert_eq!(bytes, Some(100 * 1024 * 1024 * 1024));
                    })
                } else {
                    daemon.admission.check_local_free_space_floor().await
                }
            });
        }
        let all_started = tokio::time::timeout(Duration::from_secs(4), async {
            for _ in 0..9 {
                started.recv().await.unwrap();
            }
        })
        .await;
        let (lock, ready) = &*gate;
        *lock.lock().unwrap() = true;
        ready.notify_all();
        let mut results = Vec::new();
        while let Some(result) = checks.join_next().await {
            results.push(result.unwrap());
        }
        assert!(all_started.is_ok(), "all nine independent probes must start (report_only={report_only}): {results:?}");
        assert!(results.iter().all(Result::is_ok), "{results:?}");
    }
}

/// An SSH host, owned by `ssh-host-owner`, with its policy and
/// host-direct kind, optionally declaring a vessel cap (#3793).
async fn ssh_host(backend: &ResourceBackend, name: &str, max_vessels: Option<u32>) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let kind = format!("host-direct-{name}");
    let host = hosts
        .create(
            &test_meta(name),
            &HostSpec {
                display_name: name.into(),
                connection: flotilla_resources::HostConnection::SshHost {
                    owning_daemon: "ssh-host-owner".into(),
                    destination: format!("crew@{name}.example"),
                },
                max_vessels,
                ..HostSpec::default()
            },
        )
        .await
        .expect("SSH host");
    hosts
        .update_status(
            name,
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: [(
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                )]
                .into_iter()
                .collect(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                fulfilment_facts: BTreeMap::from([(kind.clone(), FulfilmentFacts { observed_at: Utc::now(), ..Default::default() })]),
                ..Default::default()
            },
        )
        .await
        .expect("SSH host facts");
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta(&kind),
            &FulfilmentKindSpec::builder()
                .host_ref(name.to_string())
                .pool("cleat".to_string())
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("SSH host kind");
}

async fn ssh_host_owner(backend: &ResourceBackend, temp: &tempfile::TempDir) -> Arc<InProcessDaemon> {
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"ssh-host-owner\"\n").expect("daemon config");
    InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("udder"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await
}

// #3793 acceptance: three SSH hosts that each declare `max_vessels = 1`
// take unpinned convoys, one per host. A fourth waits with "no free vessel
// slots", and only a finished teardown of a vessel on its host lets it start:
// a vessel still tearing down keeps its slot.
#[tokio::test]
async fn capped_ssh_hosts_spread_unpinned_convoys_and_queue_beyond_their_cap() {
    let temp = tempfile::tempdir().expect("tempdir");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = ssh_host_owner(&backend, &temp).await;
    for name in ["b1", "b2", "b3"] {
        ssh_host(&backend, name, Some(1)).await;
    }
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let unpinned = flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".to_string()).build();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let vessels = backend.clone().using::<flotilla_resources::Vessel>("flotilla");
    let vessel_spec = |convoy: &str, policy: &str| flotilla_resources::VesselSpec {
        convoy_ref: convoy.into(),
        vessel_name: "work".into(),
        fulfilment_kind_ref: policy.into(),
        adopted_checkout_refs: BTreeMap::new(),
    };
    let mut placed: Vec<(String, PlacementDecision)> = Vec::new();
    for index in 0..4 {
        if index == 3 {
            // The first three provision: their vessels, not their admissions,
            // now hold the slots.
            for (name, decision) in &placed {
                vessels
                    .create(
                        &InputMeta::builder()
                            .name(format!("{name}-work"))
                            .finalizers(vec!["flotilla.work/vessel-workspace-teardown".into()])
                            .build(),
                        &vessel_spec(name, &decision.kind),
                    )
                    .await
                    .expect("provisioned vessel");
            }
        }
        let (placement, _) = daemon
            .resolve_capability_placement("flotilla", "project", &[], &workflow, &BTreeSet::new(), &unpinned)
            .await
            .expect("a capped SSH host takes an unpinned convoy");
        let selected = placement.selected.expect("unpinned placement");
        // Admission records the decision on the convoy it creates.
        let decision = PlacementDecision {
            provider_instance: None,
            minimal_alternatives: Vec::new(),
            escalation_reason: None,
            kind: selected.metadata.name.clone(),
            target_host: placement_target_host(&backend, "flotilla", &selected).await.expect("target host"),
            refused_candidates: placement.refused_candidates,
            viable_not_selected: placement.viable_not_selected,
            allocation: placement.allocation,
            provider_workspace_root: None,
        };
        let name = format!("convoy-{index}");
        let convoy =
            convoys.create(&test_meta(&name), &ConvoySpec::builder().workflow_ref("missing".to_string()).build()).await.expect("convoy");
        convoys
            .update_status(
                &name,
                &convoy.metadata.resource_version,
                &ConvoyStatus { placement_decision: Some(decision.clone()), ..Default::default() },
            )
            .await
            .expect("admitted placement");
        placed.push((name, decision));
    }
    let hosts = placed.iter().map(|(_, decision)| decision.target_host.reference.to_string()).collect::<Vec<_>>();
    assert_eq!(hosts[..3].iter().collect::<BTreeSet<_>>().len(), 3, "the first three spread over three hosts: {hosts:?}");
    let allocation = placed[3].1.allocation.as_ref().expect("fourth allocation");
    let chosen = allocation.candidates.iter().find(|candidate| candidate.kind == allocation.chosen_kind).expect("chosen candidate");
    assert_eq!((chosen.free_vessel_slots, chosen.max_vessels, chosen.available), (Some(0), Some(1), false), "{allocation:?}");

    let reconciler = flotilla_crew::convoy::ConvoyReconciler::new(backend.definitions::<flotilla_resources::WorkflowTemplate>("flotilla"))
        .with_hosts(backend.including_replicas::<ResourceHost>("flotilla"))
        .with_convoys(backend.including_replicas::<ResourceConvoy>("flotilla"))
        .with_federated_vessels(backend.including_replicas::<flotilla_resources::Vessel>("flotilla"));
    let capacity_stall = |outcome: flotilla_store::controller::ReconcileOutcome<ResourceConvoy>| match outcome.patch {
        Some(flotilla_resources::ConvoyStatusPatch::SetStalled { condition: Some(condition) })
            if condition.cause == Some(flotilla_resources::StallCause::Capacity) =>
        {
            Some(condition.evidence)
        }
        _ => None,
    };
    let gate = |name: String| {
        let reconciler = &reconciler;
        let convoys = convoys.clone();
        async move {
            let convoy = convoys.get(&name).await.expect("convoy");
            let prepared = flotilla_store::controller::Reconciler::prepare(reconciler, &convoy).await.expect("prepare");
            capacity_stall(flotilla_store::controller::Reconciler::reconcile(reconciler, &convoy, &prepared, Utc::now()))
        }
    };
    for (name, _) in &placed[..3] {
        assert_eq!(gate(name.clone()).await, None, "{name} holds its host's slot");
    }
    let waiting = gate(placed[3].0.clone()).await.expect("the fourth convoy waits");
    assert!(waiting.contains("no free vessel slots"), "{waiting}");

    // Tear down the convoy on the fourth's host: its vessel holds the slot
    // until its teardown finishes.
    let (holder, _) = placed[..3].iter().find(|(_, decision)| decision.target_host == placed[3].1.target_host).expect("holder");
    let vessel_spec = vessel_spec(holder, &placed[3].1.kind);
    let holder_convoy = convoys.get(holder).await.expect("holder");
    let mut landed = holder_convoy.status.clone().expect("holder status");
    landed.phase = ConvoyPhase::Landed;
    convoys.update_status(holder, &holder_convoy.metadata.resource_version, &landed).await.expect("holder landed");
    vessels.delete(&format!("{holder}-work")).await.expect("start teardown");
    assert!(
        vessels.get(&format!("{holder}-work")).await.expect("vessel tearing down").metadata.deletion_timestamp.is_some(),
        "the finalizer holds the vessel mid-teardown"
    );
    assert!(gate(placed[3].0.clone()).await.is_some(), "a vessel mid-teardown still holds its slot");

    let tearing = vessels.get(&format!("{holder}-work")).await.expect("vessel tearing down");
    vessels
        .update(
            &InputMeta::from(&tearing.metadata).without_finalizer("flotilla.work/vessel-workspace-teardown"),
            &tearing.metadata.resource_version,
            &vessel_spec,
        )
        .await
        .expect("teardown finished");
    let gone = vessels.get(&format!("{holder}-work")).await;
    assert!(gone.is_err(), "the vessel is gone once its finalizer is released: {gone:?}");
    assert_eq!(gate(placed[3].0.clone()).await, None, "a finished teardown admits the waiting convoy");
}

// #3793: a host without `max_vessels` keeps the pin-only rule, even beside a
// capped host that takes the unpinned convoy.
#[tokio::test]
async fn uncapped_ssh_host_still_refuses_unpinned_convoys() {
    let temp = tempfile::tempdir().expect("tempdir");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = ssh_host_owner(&backend, &temp).await;
    ssh_host(&backend, "b1", Some(1)).await;
    ssh_host(&backend, "b9", None).await;
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let unpinned = flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".to_string()).build();
    let (placement, _) = daemon
        .resolve_capability_placement("flotilla", "project", &[], &workflow, &BTreeSet::new(), &unpinned)
        .await
        .expect("the capped host takes the convoy");
    assert_eq!(placement.selected.expect("placement").metadata.name, "host-direct-b1");
    let refusal = placement.refused_candidates.iter().find(|refusal| refusal.kind == "host-direct-b9").expect("b9 refused");
    assert_eq!(refusal.reason, "SSH host takes only convoys that pin its placement");
}

// #3793: among otherwise equal hosts, placement prefers the one with the most
// free slots, so two convoys on two hosts of cap 2 land one on each.
#[tokio::test]
async fn placement_prefers_the_capped_host_with_most_free_slots() {
    let temp = tempfile::tempdir().expect("tempdir");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = ssh_host_owner(&backend, &temp).await;
    ssh_host(&backend, "b1", Some(2)).await;
    ssh_host(&backend, "b2", Some(2)).await;
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let unpinned = flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".to_string()).build();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let mut hosts = Vec::new();
    for index in 0..2 {
        let (placement, _) = daemon
            .resolve_capability_placement("flotilla", "project", &[], &workflow, &BTreeSet::new(), &unpinned)
            .await
            .expect("placement");
        let selected = placement.selected.expect("unpinned placement");
        let target_host = placement_target_host(&backend, "flotilla", &selected).await.expect("target host");
        hosts.push(target_host.reference.to_string());
        let name = format!("convoy-{index}");
        let convoy =
            convoys.create(&test_meta(&name), &ConvoySpec::builder().workflow_ref("missing".to_string()).build()).await.expect("convoy");
        let decision = PlacementDecision {
            provider_instance: None,
            minimal_alternatives: Vec::new(),
            escalation_reason: None,
            kind: selected.metadata.name.clone(),
            target_host,
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
            allocation: placement.allocation,
            provider_workspace_root: None,
        };
        convoys
            .update_status(
                &name,
                &convoy.metadata.resource_version,
                &ConvoyStatus { placement_decision: Some(decision), ..Default::default() },
            )
            .await
            .expect("admitted placement");
    }
    assert_eq!(hosts, ["b1", "b2"], "the second convoy goes to the host with more free slots");
}

// #3793: a convoy waits until all its vessels on a host fit, so a host whose
// cap is smaller than the vessels it would hold is refused rather than left
// to wait forever.
#[tokio::test]
async fn capped_ssh_host_refuses_a_convoy_with_more_vessels_than_its_cap() {
    let temp = tempfile::tempdir().expect("tempdir");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = ssh_host_owner(&backend, &temp).await;
    ssh_host(&backend, "b1", Some(1)).await;
    let vessel = |name: &str| flotilla_resources::VesselRequirement::builder().name(name.to_string()).crew(Vec::new()).build();
    let workflow = WorkflowTemplateSpec::builder().vessels(vec![vessel("first"), vessel("second")]).build();
    let unpinned = flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".to_string()).build();
    let refused = daemon
        .resolve_capability_placement("flotilla", "project", &[], &workflow, &BTreeSet::new(), &unpinned)
        .await
        .expect_err("two vessels cannot fit a cap of one");
    assert!(refused.contains("convoy needs 2 vessels here, more than the host's cap of 1"), "{refused}");
}

// #3793: admission prepares outside its guard, so two concurrent admissions
// can both pick the one free slot on a host. Both are admitted; the gate lets
// the older provision and holds the newer until that slot frees, so the cap
// holds.
#[tokio::test]
async fn concurrent_admissions_onto_one_free_slot_provision_in_admission_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = ssh_host_owner(&backend, &temp).await;
    ssh_host(&backend, "b1", Some(1)).await;
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let unpinned = flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".to_string()).build();
    // Both placements are decided before either convoy exists.
    let mut decisions = Vec::new();
    for _ in 0..2 {
        let (placement, _) = daemon
            .resolve_capability_placement("flotilla", "project", &[], &workflow, &BTreeSet::new(), &unpinned)
            .await
            .expect("placement");
        let selected = placement.selected.expect("unpinned placement");
        decisions.push(PlacementDecision {
            provider_instance: None,
            minimal_alternatives: Vec::new(),
            escalation_reason: None,
            kind: selected.metadata.name.clone(),
            target_host: placement_target_host(&backend, "flotilla", &selected).await.expect("target host"),
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
            allocation: placement.allocation,
            provider_workspace_root: None,
        });
    }
    assert!(decisions.iter().all(|decision| decision.target_host.reference.as_str() == "b1"), "both raced onto b1");
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    for (index, decision) in decisions.into_iter().enumerate() {
        let name = format!("convoy-{index}");
        let convoy =
            convoys.create(&test_meta(&name), &ConvoySpec::builder().workflow_ref("missing".to_string()).build()).await.expect("convoy");
        convoys
            .update_status(
                &name,
                &convoy.metadata.resource_version,
                &ConvoyStatus { placement_decision: Some(decision), ..Default::default() },
            )
            .await
            .expect("admitted placement");
    }
    let reconciler = flotilla_crew::convoy::ConvoyReconciler::new(backend.definitions::<flotilla_resources::WorkflowTemplate>("flotilla"))
        .with_hosts(backend.including_replicas::<ResourceHost>("flotilla"))
        .with_convoys(backend.including_replicas::<ResourceConvoy>("flotilla"))
        .with_federated_vessels(backend.including_replicas::<flotilla_resources::Vessel>("flotilla"));
    let mut waits = Vec::new();
    for name in ["convoy-0", "convoy-1"] {
        let convoy = convoys.get(name).await.expect("convoy");
        let prepared = flotilla_store::controller::Reconciler::prepare(&reconciler, &convoy).await.expect("prepare");
        let outcome = flotilla_store::controller::Reconciler::reconcile(&reconciler, &convoy, &prepared, Utc::now());
        waits.push(matches!(
            outcome.patch,
            Some(flotilla_resources::ConvoyStatusPatch::SetStalled { condition: Some(condition) })
                if condition.cause == Some(flotilla_resources::StallCause::Capacity) && condition.evidence.contains("no free vessel slots")
        ));
    }
    assert_eq!(waits, [false, true], "the older admission provisions; the newer waits for the slot");
}

/// A daemon host holding its Docker crew image, with the Docker kind derived
/// as feta's is, optionally declaring a Docker vessel cap (#4002).
async fn docker_host(backend: &ResourceBackend, name: &str, max_vessels: Option<u32>) {
    let kind = format!("docker-{name}");
    create_docker_placement(backend, &kind, name, BTreeSet::new()).await;
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts.get(name).await.expect("Docker host");
    hosts
        .update(&InputMeta::from(&host.metadata), &host.metadata.resource_version, &HostSpec { max_vessels, ..host.spec.clone() })
        .await
        .expect("Docker host cap");
    let host = hosts.get(name).await.expect("Docker host");
    let mut status = host.status.expect("Docker host status");
    status.capabilities.insert(flotilla_resources::IMAGE_DIGESTS_CAPABILITY.into(), serde_json::json!({"fixture-1": ["crew:latest"]}));
    hosts.update_status(name, &host.metadata.resource_version, &status).await.expect("held crew image");
}

/// A capped SSH host box whose host-direct kind is derived as b1's is,
/// with or without the single-tenant declaration (#4002).
async fn dedicated_box(backend: &ResourceBackend, name: &str, max_vessels: u32, single_tenant: bool) {
    ssh_host(backend, name, Some(max_vessels)).await;
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts.get(name).await.expect("box");
    hosts
        .update(&InputMeta::from(&host.metadata), &host.metadata.resource_version, &HostSpec { single_tenant, ..host.spec.clone() })
        .await
        .expect("box tenancy");
    let kinds = backend.clone().using::<FulfilmentKind>("flotilla");
    let kind = kinds.get(&format!("host-direct-{name}")).await.expect("box kind");
    kinds
        .update(
            &InputMeta::from(&kind.metadata),
            &kind.metadata.resource_version,
            &FulfilmentKindSpec {
                grants: BTreeSet::from([
                    FulfilmentGrant::platform("linux".to_string()),
                    FulfilmentGrant::host_account_reach(),
                    FulfilmentGrant::network("host".to_string()),
                ]),
                ..kind.spec.clone()
            },
        )
        .await
        .expect("box grants");
}

/// Admit one convoy, pinned or not, and record its decision, as admission does.
async fn admit(daemon: &InProcessDaemon, backend: &ResourceBackend, name: &str, pin: Option<&str>) -> PlacementDecision {
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let intent = flotilla_protocol::ConvoyStartIntent::builder()
        .project_ref("project".to_string())
        .maybe_placement_policy(pin.map(str::to_string))
        .build();
    let (placement, alternatives) = daemon
        .resolve_capability_placement("flotilla", "project", &[], &workflow, &BTreeSet::new(), &intent)
        .await
        .expect("the convoy is admitted without escalation");
    let selected = placement.selected.expect("unpinned placement");
    let decision = PlacementDecision {
        provider_instance: None,
        minimal_alternatives: alternatives,
        escalation_reason: None,
        kind: selected.metadata.name.clone(),
        target_host: placement_target_host(backend, "flotilla", &selected).await.expect("target host"),
        refused_candidates: placement.refused_candidates,
        viable_not_selected: placement.viable_not_selected,
        allocation: placement.allocation,
        provider_workspace_root: None,
    };
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy =
        convoys.create(&test_meta(name), &ConvoySpec::builder().workflow_ref("missing".to_string()).build()).await.expect("convoy");
    convoys
        .update_status(
            name,
            &convoy.metadata.resource_version,
            &ConvoyStatus { placement_decision: Some(decision.clone()), ..Default::default() },
        )
        .await
        .expect("admitted placement");
    decision
}

fn allocation_candidate<'a>(decision: &'a PlacementDecision, kind: &str) -> &'a flotilla_protocol::FulfilmentAllocationCandidate {
    let allocation = decision.allocation.as_ref().expect("allocation");
    allocation.candidates.iter().find(|candidate| candidate.kind == kind).unwrap_or_else(|| panic!("{kind} in {allocation:?}"))
}

// #4002: a box declared single-tenant ranks as contained, so it is a minimal
// alternative beside the Docker kind. An undeclared box keeps its honest host
// reach and stays a strict superset.
#[tokio::test]
async fn declared_single_tenant_box_is_a_minimal_alternative_and_an_undeclared_one_is_not() {
    let temp = tempfile::tempdir().expect("tempdir");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = ssh_host_owner(&backend, &temp).await;
    docker_host(&backend, "feta", None).await;
    dedicated_box(&backend, "b1", 1, true).await;
    dedicated_box(&backend, "b2", 1, false).await;
    let decision = admit(&daemon, &backend, "convoy", None).await;
    let feta = allocation_candidate(&decision, "docker-feta");
    let declared = allocation_candidate(&decision, "host-direct-b1");
    let undeclared = allocation_candidate(&decision, "host-direct-b2");
    assert_eq!((feta.minimal, feta.single_tenant), (true, false));
    assert_eq!((declared.minimal, declared.single_tenant), (true, true));
    assert_eq!((undeclared.minimal, undeclared.single_tenant), (false, false));
    let kind = backend.clone().using::<FulfilmentKind>("flotilla").get("host-direct-b1").await.expect("b1 kind");
    assert!(kind.spec.grants.contains(&FulfilmentGrant::host_account_reach()), "the grants stay honest: {:?}", kind.spec.grants);
}

// #4002: at its cap, a capped Docker host overflows unpinned work to a
// single-tenant box with a free slot.
#[tokio::test]
async fn capped_docker_host_at_its_cap_overflows_to_a_box() {
    let temp = tempfile::tempdir().expect("tempdir");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = ssh_host_owner(&backend, &temp).await;
    docker_host(&backend, "feta", Some(1)).await;
    dedicated_box(&backend, "b1", 1, true).await;
    // The first convoy fills feta's one slot.
    admit(&daemon, &backend, "first", Some("docker-feta")).await;
    let second = admit(&daemon, &backend, "second", None).await;
    assert_eq!(second.kind, "host-direct-b1");
    let feta = allocation_candidate(&second, "docker-feta");
    assert_eq!((feta.free_vessel_slots, feta.max_vessels, feta.available), (Some(0), Some(1), false));
}

// #4002: when every contained candidate is at its cap, the convoy is still
// admitted and waits at the capacity gate, as for a full SSH host.
#[tokio::test]
async fn all_candidates_full_means_the_convoy_waits() {
    let temp = tempfile::tempdir().expect("tempdir");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = ssh_host_owner(&backend, &temp).await;
    docker_host(&backend, "feta", Some(1)).await;
    dedicated_box(&backend, "b1", 1, true).await;
    for (name, pin) in [("first", "docker-feta"), ("second", "host-direct-b1")] {
        admit(&daemon, &backend, name, Some(pin)).await;
    }
    let third = admit(&daemon, &backend, "third", None).await;
    let allocation = third.allocation.as_ref().expect("allocation");
    assert!(allocation.candidates.iter().all(|candidate| !candidate.available), "{allocation:?}");
    // Equal candidates break ties by name, so the Docker host's gate is the one under test.
    assert_eq!(third.kind, "docker-feta");
    let reconciler = flotilla_crew::convoy::ConvoyReconciler::new(backend.definitions::<flotilla_resources::WorkflowTemplate>("flotilla"))
        .with_hosts(backend.including_replicas::<ResourceHost>("flotilla"))
        .with_convoys(backend.including_replicas::<ResourceConvoy>("flotilla"))
        .with_federated_vessels(backend.including_replicas::<flotilla_resources::Vessel>("flotilla"));
    let convoy = backend.clone().using::<ResourceConvoy>("flotilla").get("third").await.expect("third convoy");
    let prepared = flotilla_store::controller::Reconciler::prepare(&reconciler, &convoy).await.expect("prepare");
    let outcome = flotilla_store::controller::Reconciler::reconcile(&reconciler, &convoy, &prepared, Utc::now());
    assert!(
        matches!(
            outcome.patch,
            Some(flotilla_resources::ConvoyStatusPatch::SetStalled { condition: Some(ref condition) })
                if condition.cause == Some(flotilla_resources::StallCause::Capacity) && condition.evidence.contains("no free vessel slots")
        ),
        "{:?}",
        outcome.patch
    );
}
