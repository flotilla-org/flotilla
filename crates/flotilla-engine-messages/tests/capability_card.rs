//! A crew session's capability card is admitted on the engine (#3108): its subject, the old
//! TerminalSession reference, is stored as the session's seat with the card's revision, and the
//! crew's old-shape view names the session again.

use std::collections::BTreeMap;

use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_protocol::ResourceRef;
use flotilla_resources::{
    message_record_name, Convoy, ConvoySpec, InputMeta, MessageReference, MessageRelation, MessageSpec, Selector, TerminalBrief,
    TerminalCrewContext, TerminalSession, TerminalSessionSource, TerminalSessionSpec, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";

#[tokio::test]
async fn a_capability_card_is_admitted_about_its_sessions_seat() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    backend
        .using::<Convoy>(NAMESPACE)
        .create(
            &InputMeta::builder().name("crew".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).role("crew".into()).build(),
        )
        .await
        .expect("convoy");
    backend
        .using::<TerminalSession>(NAMESPACE)
        .create(
            &InputMeta::builder()
                .name("session".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "crew".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "env".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".into(), adapter: None, model: None },
                    brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: "brief".into(), copies: vec![] },
                    context: Box::new(TerminalCrewContext {
                        namespace: NAMESPACE.into(),
                        convoy: "crew".into(),
                        vessel_ref: "vessel".into(),
                    }),
                },
                cwd: "/repo".into(),
                env: BTreeMap::new(),
                pool: "passthrough".into(),
            },
        )
        .await
        .expect("session");
    let home = EngineMessagesHome::open("home", NAMESPACE, &backend);
    let primitives = home.primitives();

    let receiver = "flotilla/crew/work/coder";
    let sender = "system:capabilities";
    let name = message_record_name(receiver, sender, "session:capabilities@7");
    let subject = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "TerminalSession", NAMESPACE, "session"),
        revision: "capabilities@7".into(),
    };
    let card = MessageSpec::builder()
        .sender(sender.into())
        .receiver(receiver.into())
        .relation(MessageRelation::System)
        .body("## Your capabilities\n".into())
        .references(vec![subject.clone()])
        .subject(subject.clone())
        .build();
    primitives.admit_old(NAMESPACE, &name, &card).await.expect("the card is admitted");

    let inbox = primitives.inbox_for(NAMESPACE, receiver).await.expect("the receiver's inbox");
    let (id, _) = primitives.locate_in(&inbox, &name).expect("locate").expect("the admitted card");
    let stored = primitives.pending_in(&inbox).expect("pending").into_iter().find(|(_, message)| message.name() == id).expect("stored").1;
    let stored_subject = stored.spec().subject().expect("a subject");
    assert_eq!(stored_subject.address.to_string(), "project:flotilla/convoy:crew/role:coder");
    assert_eq!(stored_subject.revision, "capabilities@7");
    let view = primitives.view_to(NAMESPACE, receiver, &name).await.expect("view").expect("the card's view").object;
    assert_eq!(view.spec.subject, Some(subject));
}
