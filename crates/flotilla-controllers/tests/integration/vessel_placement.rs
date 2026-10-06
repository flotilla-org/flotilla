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
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                policy_name: "host-direct-feta".to_string(),
                target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("feta-host"), display_name: "feta".to_string() },
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
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
        flotilla_resources::home_bound_authorship_collisions(&feta, NAMESPACE)
            .await
            .expect("placement host collision diagnosis")
            .is_empty(),
        "the placed-Vessel actuator is a projection of the origin's Vessel, not a second author"
    );
    kiwi.replica_writer::<Vessel>(NodeId::new("feta-root"), NAMESPACE)
        .replace(&feta.using::<Vessel>(NAMESPACE).list().await.expect("list actuator Vessels"), Utc::now())
        .await
        .expect("replicate actuator status to admitting root");
    assert!(
        flotilla_resources::home_bound_authorship_collisions(&kiwi, NAMESPACE)
            .await
            .expect("admitting host collision diagnosis")
            .is_empty(),
        "the origin must not treat the actuator replica as another author"
    );
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

#[tokio::test]
async fn owning_daemon_projects_a_vessel_placed_on_its_agentless_ssh_host() {
    let admitting_root = NodeId::new("admitting-root");
    let admitting = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(admitting_root.clone());
    let owner = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("owning-root"));
    let convoys = admitting.using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &InputMeta::builder().name("ssh-placement".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("admitting convoy");
    convoys
        .update_status("ssh-placement", &convoy.metadata.resource_version, &ConvoyStatus {
            placement_decision: Some(PlacementDecision {
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                policy_name: "host-direct-ssh-host".to_string(),
                target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("ssh-host"), display_name: "beaufort".to_string() },
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            }),
            ..ConvoyStatus::default()
        })
        .await
        .expect("placement decision");
    admitting
        .using::<Vessel>(NAMESPACE)
        .create(&InputMeta::builder().name("ssh-placement-work".to_string()).build(), &VesselSpec {
            convoy_ref: "ssh-placement".to_string(),
            vessel_name: "work".to_string(),
            placement_policy_ref: "host-direct-ssh-host".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("admitting Vessel");
    owner
        .replica_writer::<Convoy>(admitting_root.clone(), NAMESPACE)
        .replace(&convoys.list().await.expect("convoys"), Utc::now())
        .await
        .expect("replicate convoy");
    owner
        .replica_writer::<Vessel>(admitting_root, NAMESPACE)
        .replace(&admitting.using::<Vessel>(NAMESPACE).list().await.expect("vessels"), Utc::now())
        .await
        .expect("replicate vessel");

    let projector = VesselPlacementProjector::new(owner.clone(), NAMESPACE, CanonicalHostId::resolved("owner-host"))
        .with_additional_host_refs([CanonicalHostId::resolved("ssh-host")]);
    assert_eq!(projector.sync_once().await.expect("project SSH Vessel").created, 1);
    let actuator = owner.using::<Vessel>(NAMESPACE).get("ssh-placement-work").await.expect("owned actuator");
    assert_eq!(actuator.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).map(String::as_str), Some("ssh-host"));
}

// #2775: replicated status churn must not cause repeated no-op placement scans.
// Observe the existing sync event with a task-local subscriber; no global tracing state.
#[tokio::test(start_paused = true)]
async fn placement_status_churn_does_not_spin() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::{layer::SubscriberExt, Layer};

    struct SyncCounter(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> Layer<S> for SyncCounter {
        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            struct Message(bool);
            impl tracing::field::Visit for Message {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "message" && format!("{value:?}").contains("synchronized placed Vessel actuators") {
                        self.0 = true;
                    }
                }
            }
            let mut message = Message(false);
            event.record(&mut message);
            if message.0 {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    let origin = NodeId::new("admitting");
    let source = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(origin.clone());
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("actuator"));
    let convoys = source.using::<Convoy>(NAMESPACE);
    let mut convoy = convoys
        .create(
            &InputMeta::builder().name("status-churn".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("placement scenario operation");
    let writer = backend.replica_writer::<Convoy>(origin, NAMESPACE);
    writer.replace(&convoys.list().await.expect("placement scenario operation"), Utc::now()).await.expect("placement scenario operation");
    let scans = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(SyncCounter(scans.clone()));
    let projector = VesselPlacementProjector::new(backend, NAMESPACE, CanonicalHostId::resolved("local"));
    let task = tokio::spawn(async move { projector.run().await }.with_subscriber(subscriber));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(scans.load(Ordering::SeqCst), 1, "initial reconciliation");
    for update in 0..100 {
        convoy = convoys
            .update_status("status-churn", &convoy.metadata.resource_version, &ConvoyStatus {
                message: Some(format!("progress-{update}")),
                ..ConvoyStatus::default()
            })
            .await
            .expect("placement scenario operation");
        writer
            .replace(&convoys.list().await.expect("placement scenario operation"), Utc::now())
            .await
            .expect("placement scenario operation");
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(std::time::Duration::from_millis(30)).await;
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    eprintln!("placement load: 100 replicated status updates: {} scans", scans.load(Ordering::SeqCst));
    assert_eq!(scans.load(Ordering::SeqCst), 1, "status-only updates must not rescan placement");
    // Real placement changes still reconcile, with one scan for a queued burst.
    for update in 0..100 {
        convoy = convoys
            .update_status("status-churn", &convoy.metadata.resource_version, &ConvoyStatus {
                placement_decision: Some(PlacementDecision {
                    minimal_alternatives: Vec::new(),
                    escalation_reason: None,
                    policy_name: "host-direct".into(),
                    target_host: PlacementTargetHost {
                        reference: CanonicalHostId::resolved(format!("host-{update}")),
                        display_name: "target".into(),
                    },
                    refused_candidates: Vec::new(),
                    viable_not_selected: Vec::new(),
                    allocation: None,
                }),
                ..ConvoyStatus::default()
            })
            .await
            .expect("placement scenario operation");
        writer
            .replace(&convoys.list().await.expect("placement scenario operation"), Utc::now())
            .await
            .expect("placement scenario operation");
    }
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(std::time::Duration::from_millis(30)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(scans.load(Ordering::SeqCst), 2, "placement burst should reconcile once");
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(scans.load(Ordering::SeqCst), 3, "bounded resync should repair drift even without watch changes");
    task.abort();
}
