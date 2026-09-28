use std::collections::BTreeMap;

use chrono::Utc;
use flotilla_protocol::{CanonicalHostId, FulfilmentAllocation, FulfilmentAllocationCandidate, PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    controller::Reconciler, Convoy, ConvoyReconciler, ConvoySpec, ConvoyStatus, ConvoyStatusPatch, FulfilmentFacts, Host, HostSpec,
    HostStatus, InMemoryBackend, InputMeta, ResourceBackend, StatusPatch, WorkflowTemplate,
};

#[tokio::test]
async fn convoy_waits_for_selected_minimal_kind_and_resumes_when_capacity_returns() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let hosts = backend.using::<Host>("flotilla");
    let host = hosts.create(&InputMeta::builder().name("feta".to_string()).build(), &HostSpec::default()).await.expect("host");
    let full = HostStatus {
        ready: true,
        heartbeat_at: Some(Utc::now()),
        fulfilment_facts: BTreeMap::from([("linux-docker".to_string(), FulfilmentFacts {
            free_vessel_slots: Some(0),
            observed_at: Utc::now(),
            ..Default::default()
        })]),
        ..Default::default()
    };
    let host = hosts.update_status("feta", &host.metadata.resource_version, &full).await.expect("full host");
    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys
        .create(
            &InputMeta::builder().name("waiting".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("missing".to_string()).build(),
        )
        .await
        .expect("convoy");
    let decision = PlacementDecision {
        policy_name: "linux-docker".to_string(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("feta"), display_name: "feta".to_string() },
        minimal_alternatives: vec!["large-host".to_string()],
        escalation_reason: None,
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: Some(FulfilmentAllocation {
            chosen_kind: "linux-docker".to_string(),
            candidates: vec![FulfilmentAllocationCandidate {
                kind: "linux-docker".to_string(),
                host: "feta".to_string(),
                cost_class: "owned_idle".to_string(),
                host_ready: true,
                sleeping_until: None,
                free_vessel_slots: Some(0),
                reserved_for_platform: false,
                minimal: true,
                available: false,
            }],
        }),
    };
    let convoy = convoys
        .update_status("waiting", &convoy.metadata.resource_version, &ConvoyStatus {
            placement_decision: Some(decision),
            ..Default::default()
        })
        .await
        .expect("decision");
    let reconciler = ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla"))
        .with_hosts(backend.including_replicas::<Host>("flotilla"));
    let prepared = reconciler.prepare(&convoy).await.expect("prepare full");
    let outcome = reconciler.reconcile(&convoy, &prepared, Utc::now());
    assert!(outcome.actuations.is_empty());
    assert!(outcome.requeue_after.is_some());
    let mut status = convoy.status.expect("convoy status");
    outcome.patch.expect("stall patch").apply(&mut status);
    assert!(status
        .stalled
        .as_ref()
        .is_some_and(|stall| stall.evidence.contains("linux-docker") && stall.evidence.contains("no free vessel slots")));

    let mut free = full;
    free.fulfilment_facts.get_mut("linux-docker").expect("facts").free_vessel_slots = Some(1);
    hosts.update_status("feta", &host.metadata.resource_version, &free).await.expect("capacity restored");
    let convoy = convoys.get("waiting").await.expect("convoy");
    let convoy = convoys.update_status("waiting", &convoy.metadata.resource_version, &status).await.expect("persist stall");
    let prepared = reconciler.prepare(&convoy).await.expect("prepare free");
    let outcome = reconciler.reconcile(&convoy, &prepared, Utc::now());
    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::SetStalled { condition: None })));
}
