use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use flotilla_protocol::{HostName, PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    CapabilityNeed, Convoy as ResourceConvoy, ConvoyPhase, ConvoySpec, ConvoyStatus, FulfilmentFacts, FulfilmentGrant, FulfilmentKind,
    FulfilmentKindSpec, FulfilmentRealisation, HarnessFacts, Host as ResourceHost, HostDirectPlacementPolicyCheckout,
    HostDirectPlacementPolicySpec, HostSpec, HostStatus, InMemoryBackend, InputMeta, PlacementPolicy, PlacementPolicySpec, ResourceBackend,
    WorkflowTemplateSpec, GENERATION_LABEL, PROJECT_LABEL, ROLE_LABEL,
};

use super::support::{create_identity_convoy, placement_policy, test_meta};
use crate::config::ConfigStore;
use crate::in_process::convoy_admission::{allocate_convoy_generation, convoy_record_name, parse_role_address, RoleAddress};
use crate::in_process::{placement_target_host, resolve_local_convoy_name, InProcessDaemon};
use crate::providers::discovery::test_support::fake_discovery;

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

#[test]
fn qualified_role_address_is_a_typed_project_role_pair() {
    assert_eq!(
        RoleAddress::from_str("governor@andamento"),
        Ok(RoleAddress { project: "andamento".to_string(), role: "governor".to_string() })
    );
    for value in ["governor", "@andamento", "governor@", "governor@andamento@extra"] {
        assert!(RoleAddress::from_str(value).is_err(), "{value} must not produce a qualified address");
    }
}

#[tokio::test]
async fn convoy_role_resolution_can_disambiguate_a_projectless_convoy() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("udder"),
        backend.clone(),
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
    placement_policy(&backend, "a-collision", "collision").await;
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
    placement_policy(&backend, "udder-kind", "udder").await;
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host identity").to_string();

    let stale_source = ResourceBackend::InMemory(InMemoryBackend::default());
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
                disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
                admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
                ..HostStatus::default()
            },
        )
        .await
        .expect("publish live local capacity");
    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("self-targeted"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: host_id.clone(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("self-targeted placement policy");

    daemon
        .check_remote_placement_free_space_floor(
            "flotilla",
            Some(&PlacementDecision {
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                policy_name: "self-targeted".to_string(),
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
        ResourceBackend::InMemory(InMemoryBackend::default()),
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
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
                disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
                admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
                ..HostStatus::default()
            },
        )
        .await
        .expect("publish live local capacity");
    let policy = backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("self-targeted"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: "local-host".to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("self-targeted placement policy");

    let target = placement_target_host(&backend, "flotilla", &policy).await.expect("resolve display-name host reference");
    assert_eq!(target.reference.as_str(), host_id);
    assert_eq!(daemon.remote_placement_host("flotilla", Some("self-targeted")).await.expect("resolve host-direct routing"), None);
    daemon
        .check_remote_placement_free_space_floor(
            "flotilla",
            Some(&PlacementDecision {
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                policy_name: "self-targeted".to_string(),
                target_host: target,
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            }),
        )
        .await
        .expect("healthy authoritative local capacity should admit self-targeted placement");
}
