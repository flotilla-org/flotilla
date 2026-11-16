//! A turn's text is framed from the engine message as it is held (#3108): its parties in today's
//! spelling and its subject, with nothing else of it spelled. The text is the text the old shape
//! framed, byte for byte.

use std::collections::BTreeMap;

use flotilla_engine_messages::{view::turn_header, OldStoreRecords};
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_resources::{
    Convoy, ConvoySpec, InputMeta, MessageExpectation, MessageReference, MessageRelation, MessageSpec, ResourceRef, Selector,
    TerminalBrief, TerminalCrewContext, TerminalSession, TerminalSessionSource, TerminalSessionSpec, CONVOY_LABEL, ROLE_LABEL,
    VESSEL_LABEL,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const CODER: &str = "flotilla/crew/work/coder";
const REVIEWER: &str = "flotilla/crew/work/reviewer";

/// A crew session holding `role` in convoy `crew`'s work vessel, labelled as the vessel
/// reconciler labels it.
fn session(role: &str) -> (InputMeta, TerminalSessionSpec) {
    let meta = InputMeta::builder()
        .name(format!("crew-work-{role}"))
        .labels(BTreeMap::from([
            (CONVOY_LABEL.into(), "crew".into()),
            (VESSEL_LABEL.into(), "work".into()),
            (ROLE_LABEL.into(), role.into()),
        ]))
        .build();
    let spec = TerminalSessionSpec {
        env_ref: "env".into(),
        role: role.into(),
        source: TerminalSessionSource::Agent {
            selector: Selector { capability: "code".into(), adapter: None, model: None },
            brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: "brief".into(), copies: vec![] },
            context: Box::new(TerminalCrewContext { namespace: NAMESPACE.into(), convoy: "crew".into(), vessel_ref: "vessel".into() }),
        },
        cwd: "/repo".into(),
        env: BTreeMap::new(),
        pool: "passthrough".into(),
    };
    (meta, spec)
}

// Behaviour: each turn header frames as the old view of the same message framed: a crew peer
// shortened to its vessel and role, a subject the header names through its record, an
// expectation, a person, and flotilla's own rules.
#[tokio::test]
async fn a_turn_header_frames_as_the_old_view_did() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    backend
        .using::<Convoy>(NAMESPACE)
        .create(&InputMeta::builder().name("crew".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .expect("convoy");
    for role in ["coder", "reviewer"] {
        let (meta, spec) = session(role);
        backend.using::<TerminalSession>(NAMESPACE).create(&meta, &spec).await.expect("session");
    }
    let home = EngineMessagesHome::open("home", NAMESPACE, &backend);
    let primitives = home.primitives();
    let convoy = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", NAMESPACE, "crew"),
        revision: "1".into(),
    };
    let intents = [
        MessageSpec::builder()
            .sender(REVIEWER.into())
            .receiver(CODER.into())
            .relation(MessageRelation::Peer)
            .body("Please address the review.".into())
            .subject(convoy.clone())
            .references(vec![convoy])
            .expectation(MessageExpectation::Reply)
            .build(),
        MessageSpec::builder()
            .sender("principal:alice".into())
            .receiver(CODER.into())
            .relation(MessageRelation::Supervisor)
            .body("Continue the work.".into())
            .build(),
        MessageSpec::builder()
            .sender("system:turn-rules".into())
            .receiver(CODER.into())
            .relation(MessageRelation::System)
            .body("Complete this turn.".into())
            .build(),
    ];
    for (index, intent) in intents.iter().enumerate() {
        primitives.admit_old(NAMESPACE, &format!("turn-{index}"), intent).await.expect("admit");
    }

    let roles = primitives.roles(NAMESPACE).await.expect("fleet roles");
    let inbox = "project:flotilla/convoy:crew/role:coder".parse().expect("the coder's inbox");
    let held = primitives.open_in(&inbox).expect("the coder's messages");
    assert_eq!(held.len(), intents.len());
    let mut records = OldStoreRecords::empty(NAMESPACE);
    let headers = primitives
        .spell(&mut records, |records| held.iter().map(|(_, resource)| turn_header(resource, &roles, records)).collect::<Vec<_>>())
        .await
        .expect("spell the headers");
    for ((_, resource), header) in held.iter().zip(headers) {
        let record = flotilla_engine_messages::view::record_name(resource.name()).expect("a record name");
        let old = primitives.view_to(NAMESPACE, CODER, &record).await.expect("old view").expect("held");
        let header = header.expect("a header");
        assert_eq!(flotilla_messaging::frame_turn(&header), flotilla_messaging::frame_message(&old.object.spec), "{record}");
    }
}
