use flotilla_store_memory::InMemoryBackend;
use std::collections::BTreeMap;

use chrono::Utc;
use flotilla_crew::convoy::ConvoyReconciler;
use flotilla_protocol::{CanonicalHostId, FulfilmentAllocation, FulfilmentAllocationCandidate, PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    host_records::{HeldHosts, HostFacts},
    Convoy, ConvoySpec, ConvoyStatus, FulfilmentFacts, HostConnection, HostStatus, InputMeta,
};
use flotilla_store::ResourceBackend;

#[tokio::test]
async fn convoy_waits_for_selected_minimal_kind_with_legacy_host_alias_and_resumes_when_capacity_returns() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let hosts = std::sync::Arc::new(HeldHosts::default());
    let host = |name: &str, status: Option<HostStatus>| HostFacts {
        name: name.into(),
        display_name: "feta".into(),
        connection: HostConnection::Daemon,
        max_vessels: None,
        status,
    };
    let full = HostStatus {
        ready: true,
        heartbeat_at: Some(Utc::now()),
        fulfilment_facts: BTreeMap::from([(
            "linux-docker".to_string(),
            FulfilmentFacts { free_vessel_slots: Some(0), observed_at: Utc::now(), ..Default::default() },
        )]),
        ..Default::default()
    };
    hosts.put(host("host-id", Some(full.clone())));
    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys
        .create(
            &InputMeta::builder().name("waiting".to_string()).build(),
            &ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("missing".to_string()).build(),
        )
        .await
        .expect("convoy");
    let decision = PlacementDecision {
        provider_instance: None,
        provider_workspace_root: None,
        kind: "linux-docker".to_string(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("host-id"), display_name: "feta".to_string() },
        minimal_alternatives: vec!["large-host".to_string()],
        escalation_reason: None,
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: Some(FulfilmentAllocation {
            chosen_kind: "linux-docker".to_string(),
            reservation_reason: None,
            candidates: vec![FulfilmentAllocationCandidate {
                kind: "linux-docker".to_string(),
                host: "feta".to_string(),
                cost_class: "owned_idle".to_string(),
                host_ready: true,
                sleeping_until: None,
                free_vessel_slots: Some(0),
                max_vessels: None,
                single_tenant: false,
                reserved_for_platform: false,
                minimal: true,
                available: false,
            }],
        }),
    };
    let convoy = convoys
        .update_status(
            "waiting",
            &convoy.metadata.resource_version,
            &ConvoyStatus { placement_decision: Some(decision), ..Default::default() },
        )
        .await
        .expect("decision");
    let reconciler = ConvoyReconciler::new(
        std::sync::Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads),
        crate::common::detached_convoys(),
        crate::common::detached_messages(),
    )
    .with_hosts(hosts.clone());
    let wait = reconciler.capacity_wait(&convoy).await.expect("judge full");
    assert!(wait.as_ref().is_some_and(|wait| wait.contains("linux-docker") && wait.contains("no free vessel slots")), "{wait:?}");

    let mut free = full;
    free.fulfilment_facts.get_mut("linux-docker").expect("facts").free_vessel_slots = Some(1);
    hosts.put(host("host-id", Some(free)));
    assert_eq!(reconciler.capacity_wait(&convoy).await.expect("judge free"), None, "restored capacity ends the wait");

    hosts.put(host("another-host-id", None));
    let wait = reconciler.capacity_wait(&convoy).await.expect("an ambiguous host waits instead of failing");
    assert!(wait.as_ref().is_some_and(|wait| wait.contains("ambiguous")), "{wait:?}");
}
