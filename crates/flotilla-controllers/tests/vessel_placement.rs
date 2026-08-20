use std::collections::BTreeMap;

use chrono::Utc;
use flotilla_controllers::reconcilers::VesselPlacementProjector;
use flotilla_protocol::{CanonicalHostId, NodeId, PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    Convoy, ConvoySpec, ConvoyStatus, InMemoryBackend, InputMeta, ResourceBackend, ResourceError, Vessel, VesselSpec,
    ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL,
};

const NAMESPACE: &str = "flotilla";

#[tokio::test]
async fn placed_replica_is_projected_into_the_actuation_hosts_local_store() {
    let kiwi_root = NodeId::new("kiwi-root");
    let feta_root = NodeId::new("feta-root");
    let kiwi = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(kiwi_root.clone());
    let feta = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(feta_root);

    let convoys = kiwi.using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &InputMeta::builder().name("remote-placement".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create admitting Convoy");
    convoys
        .update_status("remote-placement", &convoy.metadata.resource_version, &ConvoyStatus {
            placement_decision: Some(PlacementDecision {
                policy_name: "host-direct-feta".to_string(),
                target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("feta-host"), display_name: "feta".to_string() },
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
            }),
            ..ConvoyStatus::default()
        })
        .await
        .expect("record placement");
    kiwi.using::<Vessel>(NAMESPACE)
        .create(
            &InputMeta::builder()
                .name("remote-placement-work".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "remote-placement".to_string())]))
                .build(),
            &VesselSpec {
                convoy_ref: "remote-placement".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "host-direct-feta".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("create admitting Vessel");

    feta.replica_writer::<Convoy>(kiwi_root.clone(), NAMESPACE)
        .replace(&convoys.list().await.expect("list Convoys"), Utc::now())
        .await
        .expect("replicate Convoy");
    feta.replica_writer::<Vessel>(kiwi_root.clone(), NAMESPACE)
        .replace(&kiwi.using::<Vessel>(NAMESPACE).list().await.expect("list Vessels"), Utc::now())
        .await
        .expect("replicate Vessel");

    let projector = VesselPlacementProjector::new(feta.clone(), NAMESPACE, CanonicalHostId::resolved("feta-host"));
    let sync = projector.sync_once().await.expect("project placed Vessel");
    assert_eq!(sync.created, 1);
    assert_eq!(projector.sync_once().await.expect("repeat projection"), Default::default());

    let actuator =
        feta.using::<Vessel>(NAMESPACE).get("remote-placement-work").await.expect("placement host should author an actuator Vessel");
    assert_eq!(actuator.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).map(String::as_str), Some("feta-host"));
    assert_eq!(actuator.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).map(String::as_str), Some("kiwi-root"));
    assert!(
        !kiwi
            .using::<Vessel>(NAMESPACE)
            .get("remote-placement-work")
            .await
            .expect("admitting Vessel remains")
            .metadata
            .annotations
            .contains_key(ACTUATOR_SOURCE_ROOT_ANNOTATION),
        "projection must not transfer ownership of the admitting object"
    );

    kiwi.using::<Vessel>(NAMESPACE).delete("remote-placement-work").await.expect("owner requests Vessel deletion");
    assert_eq!(
        projector.sync_once().await.expect("reconcile from last-known replica"),
        Default::default(),
        "unreplicated owner deletion must freeze destructive actuation"
    );
    feta.using::<Vessel>(NAMESPACE)
        .get("remote-placement-work")
        .await
        .expect("actuator Vessel must survive while its owner is unreachable");

    feta.replica_writer::<Vessel>(kiwi_root, NAMESPACE)
        .replace(&kiwi.using::<Vessel>(NAMESPACE).list().await.expect("list deleted owner Vessels"), Utc::now())
        .await
        .expect("replicate owner deletion");
    assert_eq!(projector.sync_once().await.expect("apply observed deletion").deleted, 1);
    assert!(
        matches!(feta.using::<Vessel>(NAMESPACE).get("remote-placement-work").await, Err(ResourceError::NotFound { .. })),
        "actuator teardown may proceed after owner deletion intent is observed"
    );
}
