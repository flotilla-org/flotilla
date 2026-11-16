use std::sync::Arc;

use flotilla_resources::{CrewWorkPhase, HoldAct};

use super::fixture;
use crate::crew_ops::{convoy_message_address, CrewTurnDeliveryActuator};
use flotilla_resources::{Convoy as ResourceConvoy, ConvoySpec, InputMeta};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

// The delivery port must refuse both operations after its owning service stops;
// retaining the actuator cannot keep the service alive (#2221).
#[tokio::test]
async fn actuator_refuses_delivery_and_hold_after_service_stops() {
    use crate::leaf_engine::{CrewTurnIntent, TurnDeliveryActuator};
    let (crew, _backend, _probe, _config) = fixture(CrewWorkPhase::Done).await;
    let actuator = Arc::new(CrewTurnDeliveryActuator { crew: Arc::downgrade(&crew) });
    crew.set_turn_delivery_actuator(actuator.clone()).await;
    drop(crew);
    assert!(actuator.crew.upgrade().is_none(), "subscriptions and actuator must not form a strong cycle");
    let request = CrewTurnIntent {
        receiver: None,
        namespace: "flotilla".into(),
        convoy: "crew".into(),
        source: "rule".into(),
        vessel: "work".into(),
        role: "coder".into(),
        brief: "continue".into(),
        subject_revision: "head".into(),
        subject: None,
        sender: crate::leaf_engine::Party::fleet("nudge"),
        relation: flotilla_resources::MessageRelation::System,
        references: Vec::new(),
        delivery_condition: None,
        expectation: Default::default(),
        stall_of: None,
    };
    assert_eq!(actuator.deliver(&request).await.expect_err("stopped delivery"), "daemon stopped before turn delivery");
    assert_eq!(
        actuator.hold(&request, &HoldAct::State, "stopped").await.expect_err("stopped hold"),
        "daemon stopped before turn-delivery hold"
    );
}

// #1986: each orientation query follows live Project membership and roles while
// the crew, convoy, and admission snapshot remain intact.

// #2592: attribution uses role/project addresses.
// Formatting glue: these rows exhaust absent/present project.
#[tokio::test]
async fn convoy_message_addresses_are_role_and_project() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    for (name, role, project, expected) in [
        ("named", "graphql-budget", None, "graphql-budget"),
        ("named-project", "graphql-budget", Some("project"), "graphql-budget@project"),
    ] {
        let convoy = convoys
            .create(
                &InputMeta::builder().name(name.into()).build(),
                &ConvoySpec::builder()
                    .workflow_ref("workflow".to_string())
                    .role(role.to_string())
                    .maybe_project_ref(project.map(str::to_string))
                    .build(),
            )
            .await
            .expect("convoy");
        assert_eq!(convoy_message_address(&convoy), expected);
    }
}
