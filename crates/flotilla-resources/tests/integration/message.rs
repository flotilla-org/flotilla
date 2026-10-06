use chrono::{TimeZone, Utc};
use flotilla_protocol::ResourceRef;
use flotilla_resources::{
    decode_stored_resource_document, InMemoryBackend, InputMeta, Message, MessageExpectation, MessagePhase, MessageReference,
    MessageRelation, MessageSpec, MessageStatus, MessageStatusPatch, ReplicationClass, ResolvedMessageReceiver, Resource, ResourceBackend,
    StatusPatch, REGISTERED_RESOURCE_KINDS,
};
use hegel::generators as gs;

fn at(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("timestamp")
}

fn spec(reference: Option<MessageReference>, expectation: MessageExpectation) -> MessageSpec {
    MessageSpec::builder()
        .sender("flotilla/checks".into())
        .receiver("flotilla/convoy/work/coder".into())
        .relation(MessageRelation::System)
        .body("review the settled checks".into())
        .references(reference.clone().into_iter().collect())
        .maybe_subject(reference)
        .expectation(expectation)
        .build()
}

// The new kind participates in ordinary durable resource storage and retains
// its typed references, expectations and delivery evidence across decoding.
#[hegel::test]
fn message_stored_shape_round_trips(tc: hegel::TestCase) {
    // Every typed reference, expectation and phase, including absent subject,
    // is covered explicitly; revisions include empty and nonempty values.
    let revision = tc.draw(gs::integers::<u32>().min_value(0).max_value(3)).to_string();
    let resource = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "work");
    let reference = match tc.draw(gs::integers::<u8>().min_value(0).max_value(7)) {
        0 => None,
        1 => Some(MessageReference::ChangeRequest { service: "github".into(), scope: "org/repo".into(), number: 1, revision }),
        2 => Some(MessageReference::Commit { repository: resource, revision }),
        3 => Some(MessageReference::Ref { repository: resource, name: "main".into(), revision }),
        4 => Some(MessageReference::Artifact { resource, revision }),
        5 => Some(MessageReference::Issue { service: "github".into(), scope: "org/repo".into(), number: 2, revision }),
        6 => Some(MessageReference::Comment { service: "github".into(), scope: "org/repo".into(), id: "comment".into(), revision }),
        _ => Some(MessageReference::ControlRecord { resource, revision }),
    };
    let expectation = match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
        0 => MessageExpectation::None,
        1 => MessageExpectation::Reply,
        _ => MessageExpectation::Outcome { condition: "convoy/work .phase == Landed".parse().expect("leaf") },
    };
    let phases = [
        MessagePhase::Accepted,
        MessagePhase::WaitingOnReferences,
        MessagePhase::Deliverable,
        MessagePhase::Delivered,
        MessagePhase::Satisfied,
        MessagePhase::Answered,
        MessagePhase::OutcomeMet,
        MessagePhase::Expired,
        MessagePhase::Superseded,
        MessagePhase::DeadLettered,
    ];
    let phase = phases[tc.draw(gs::integers::<usize>().min_value(0).max_value(phases.len() - 1))];
    let spec = spec(reference, expectation);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let resolver = ResourceBackend::InMemory(InMemoryBackend::default()).using::<Message>("flotilla");
        let object = resolver.create(&InputMeta::builder().name("input".into()).build(), &spec).await.expect("create");
        let status = MessageStatus::builder()
            .phase(phase)
            .since(at(10))
            .reason("waiting for transport".into())
            .maybe_resolved_receiver(phase.has_delivery_evidence().then(|| {
                ResolvedMessageReceiver::builder()
                    .crew_id("crew".into())
                    .session("terminal".into())
                    .delivered_at(at(10))
                    .evidence("accepted by adapter".into())
                    .build()
            }))
            .build();
        let object = resolver.update_status("input", &object.metadata.resource_version, &status).await.expect("write status");
        let mut encoded = serde_json::to_value(&object).expect("encode");
        encoded["kind"] = "Message".into();
        encoded["apiVersion"] = "flotilla.work/v1".into();
        decode_stored_resource_document(&encoded).expect("registered stored decoder");
        let decoded = resolver.get("input").await.expect("read");
        assert_eq!(decoded.spec, spec);
        assert_eq!(decoded.status, Some(status));
    });
    assert_eq!(
        REGISTERED_RESOURCE_KINDS.iter().find(|kind| kind.kind == "Message").expect("registered").replication_class,
        ReplicationClass::HomeBoundRuntime
    );
}

// A waiting rung keeps its original since timestamp while its reason is
// unchanged. Delivery evidence prevents a later wait or duplicate delivery
// from revoking acceptance or changing the resolved holder.
#[hegel::test]
fn repeated_wait_and_delivery_preserve_evidence(tc: hegel::TestCase) {
    let elapsed = tc.draw(gs::integers::<i64>().min_value(0).max_value(100));
    let mut status = MessageStatus::default();
    let wait = MessageStatusPatch::Wait { phase: MessagePhase::Deliverable, reason: "waiting for turn boundary".into(), at: at(10) };
    wait.apply(&mut status);
    let original = status.clone();
    MessageStatusPatch::Wait { phase: MessagePhase::Deliverable, reason: "waiting for turn boundary".into(), at: at(10 + elapsed) }
        .apply(&mut status);
    assert_eq!(status, original);
    let receiver = ResolvedMessageReceiver::builder()
        .crew_id("crew".into())
        .session("terminal".into())
        .delivered_at(at(110))
        .evidence("acknowledged".into())
        .build();
    let delivery = MessageStatusPatch::Delivered { receiver, at: at(110) };
    delivery.apply(&mut status);
    let delivered = status.clone();
    wait.apply(&mut status);
    delivery.apply(&mut status);
    assert_eq!(status, delivered);
    Message::validate_status_update(Some(&original), &status).expect("valid delivery");
    assert!(Message::validate_status_update(Some(&delivered), &original).is_err());
}

// Status writes require a reason on every waiting rung and a resolved holder
// with evidence on every delivered rung; a subject must be a declared reference.
#[test]
fn malformed_message_states_are_refused() {
    let mut status = MessageStatus::builder().phase(MessagePhase::Accepted).since(at(0)).build();
    assert!(Message::validate_status_update(None, &status).is_err());
    status.phase = MessagePhase::Delivered;
    assert!(Message::validate_status_update(None, &status).is_err());
    let mut malformed = spec(None, MessageExpectation::None);
    malformed.subject =
        Some(MessageReference::ChangeRequest { service: "github".into(), scope: "org/repo".into(), number: 1, revision: "head".into() });
    assert!(Message::validate_spec(&InputMeta::builder().name("invalid".into()).build(), &malformed).is_err());
}
