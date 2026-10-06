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

// Convoy-relative roles are qualified at creation, while fully qualified role
// and principal addresses remain strings with no closed set of role names.
#[hegel::test]
fn relative_role_addresses_are_qualified_once(tc: hegel::TestCase) {
    use flotilla_resources::{qualify_message_address, validate_message_address, MessageAddressContext};
    let index = tc.draw(gs::integers::<u32>().min_value(0).max_value(100));
    let role = format!("reviewer-{index}");
    let context = MessageAddressContext { project: "project".into(), convoy: "convoy".into(), vessel: "work".into() };
    let address = qualify_message_address(&role, &context).expect("qualify role");
    assert_eq!(address, format!("project/convoy/work/{role}"));
    assert_eq!(qualify_message_address(&address, &context).expect("qualify again"), address);
    for address in ["project/governor", "fleet/operator", "principal:alice", "flotilla/checks"] {
        assert_eq!(qualify_message_address(address, &context).expect("full address"), address);
    }
    for invalid in ["", "project//work/coder", "principal:", "project/..", "fleet/op\nerator"] {
        assert!(validate_message_address(invalid).is_err(), "invalid address: {invalid:?}");
    }
}

// Identical subject revisions replace pending messages but suppress a new
// message while a delivered predecessor expects a reply or outcome. A new
// revision, another sender/receiver, or no subject is independent.
#[hegel::test]
fn subject_supersession_follows_identity_and_expectation(tc: hegel::TestCase) {
    use flotilla_resources::{MessageAdmission, MessageInbox};
    let delivered = tc.draw(gs::booleans());
    let open = tc.draw(gs::booleans());
    let same_revision = tc.draw(gs::booleans());
    let same_sender = tc.draw(gs::booleans());
    let same_receiver = tc.draw(gs::booleans());
    let has_subject = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    // The complete six-bit product guarantees identity collisions and open
    // expectations in every generated run; draws vary their traversal order.
    runtime.block_on(async {
        for case in 0..64 {
            let delivered = delivered ^ (case & 1 != 0);
            let open = open ^ (case & 2 != 0);
            let same_revision = same_revision ^ (case & 4 != 0);
            let same_sender = same_sender ^ (case & 8 != 0);
            let same_receiver = same_receiver ^ (case & 16 != 0);
            let has_subject = has_subject ^ (case & 32 != 0);
            let backend = ResourceBackend::InMemory(InMemoryBackend::default());
            let resolver = backend.using::<Message>("flotilla");
            let inbox = MessageInbox::new(backend, "flotilla");
            let subject = has_subject.then(|| MessageReference::ChangeRequest {
                service: "github".into(),
                scope: "org/repo".into(),
                number: 1,
                revision: "head".into(),
            });
            let first_spec = spec(subject, if open { MessageExpectation::Reply } else { MessageExpectation::None });
            inbox.accept(&InputMeta::builder().name("first".into()).build(), &first_spec, at(10)).await.expect("first");
            if delivered {
                flotilla_resources::apply_status_patch(&resolver, "first", &MessageStatusPatch::Delivered {
                    receiver: ResolvedMessageReceiver::builder()
                        .crew_id("crew".into())
                        .session("session".into())
                        .delivered_at(at(20))
                        .evidence("accepted".into())
                        .build(),
                    at: at(20),
                })
                .await
                .expect("deliver first");
            }
            let mut second_spec = first_spec.clone();
            second_spec.body = "replacement".into();
            if !same_revision && has_subject {
                let Some(MessageReference::ChangeRequest { revision, .. }) = &mut second_spec.subject else { unreachable!() };
                *revision = "new-head".into();
                second_spec.references = second_spec.subject.clone().into_iter().collect();
            }
            if !same_sender {
                second_spec.sender = "flotilla/review".into();
            }
            if !same_receiver {
                second_spec.receiver = "flotilla/convoy/work/reviewer".into();
            }
            let admission = inbox.accept(&InputMeta::builder().name("second".into()).build(), &second_spec, at(30)).await.expect("second");
            let matches = has_subject && same_revision && same_sender && same_receiver;
            assert_eq!(matches!(admission, MessageAdmission::Suppressed { .. }), matches && delivered && open);
            let first = resolver.get("first").await.expect("first record");
            assert_eq!(first.status.as_ref().is_some_and(|status| status.phase == MessagePhase::Superseded), matches && !delivered);
            assert_eq!(resolver.list().await.expect("records").items.len(), if matches && delivered && open { 1 } else { 2 });
        }
    });
}

// Subject-free intent is independent unless the producer explicitly names a
// predecessor. Retries with the same ID are idempotent; an ID cannot change body.
#[tokio::test]
async fn explicit_supersession_and_creation_retries_are_scoped_and_idempotent() {
    use flotilla_resources::{MessageAdmission, MessageInbox};
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let resolver = backend.using::<Message>("flotilla");
    let inbox = MessageInbox::new(backend, "flotilla");
    let original = spec(None, MessageExpectation::None);
    let meta = InputMeta::builder().name("original".into()).build();
    let first = inbox.accept(&meta, &original, at(10)).await.expect("create");
    let retry = inbox.accept(&meta, &original, at(20)).await.expect("retry");
    let (MessageAdmission::Accepted(first), MessageAdmission::Accepted(retry)) = (first, retry) else { panic!("accepted") };
    assert_eq!(first.metadata.resource_version, retry.metadata.resource_version);
    let mut replacement = original.clone();
    replacement.body = "different".into();
    assert!(inbox.accept(&meta, &replacement, at(30)).await.is_err());
    replacement.supersedes = Some("original".into());
    let replacement_meta = InputMeta::builder().name("replacement".into()).build();
    replacement.sender = "flotilla/other".into();
    assert!(inbox.accept(&replacement_meta, &replacement, at(30)).await.is_err());
    replacement.sender = original.sender;
    inbox.accept(&replacement_meta, &replacement, at(30)).await.expect("explicit successor");
    assert_eq!(resolver.get("original").await.expect("original").status.expect("status").phase, MessagePhase::Superseded);
    replacement.interrupting = true;
    assert!(inbox.accept(&InputMeta::builder().name("interrupt".into()).build(), &replacement, at(40)).await.is_err());
}

// Concurrent admissions for one subject leave exactly one pending successor;
// the inbox serializes the check and supersession for all cloned producers.
#[tokio::test]
async fn concurrent_same_subject_admissions_leave_one_pending_message() {
    use flotilla_resources::MessageInbox;
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let resolver = backend.using::<Message>("flotilla");
    let inbox = MessageInbox::new(backend, "flotilla");
    let original = spec(
        Some(MessageReference::ChangeRequest { service: "github".into(), scope: "org/repo".into(), number: 1, revision: "head".into() }),
        MessageExpectation::Reply,
    );
    let mut tasks = Vec::new();
    for index in 0..8 {
        let inbox = inbox.clone();
        let spec = original.clone();
        tasks.push(tokio::spawn(async move {
            inbox.accept(&InputMeta::builder().name(format!("message-{index}")).build(), &spec, at(10 + index)).await.expect("admit")
        }));
    }
    for task in tasks {
        task.await.expect("join");
    }
    let messages = resolver.list().await.expect("messages").items;
    assert_eq!(messages.len(), 8);
    assert_eq!(messages.iter().filter(|message| message.status.as_ref().is_none_or(|status| !status.phase.is_terminal())).count(), 1);
}

fn holder_spec(convoy: &str) -> flotilla_resources::TerminalSessionSpec {
    use flotilla_resources::{Selector, TerminalBrief, TerminalCrewContext, TerminalSessionSource, TerminalSessionSpec};
    TerminalSessionSpec::builder()
        .env_ref("host-direct".into())
        .role("coder".into())
        .cwd("/workspace".into())
        .pool("cleat".into())
        .source(TerminalSessionSource::Agent {
            selector: Selector::for_capability("coding"),
            brief: TerminalBrief {
                path: ".flotilla/briefs/coder.md".into(),
                content: "work".into(),
                artifact_digest: None,
                copies: Vec::new(),
            },
            context: Box::new(TerminalCrewContext {
                namespace: "flotilla".into(),
                convoy: convoy.into(),
                vessel_ref: format!("{convoy}-work"),
            }),
            message: None,
        })
        .build()
}

// A role with no holder waits. Resolution reads today's holder, including
// replicated terminals, and project-level addresses follow the newest admitted
// standing generation rather than retaining yesterday's session.
#[tokio::test]
async fn addresses_resolve_current_holders_after_replication() {
    use std::collections::BTreeMap;

    use flotilla_protocol::NodeId;
    use flotilla_resources::{
        resolve_message_receiver, Convoy, ConvoySpec, ResourceProvenance, TerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
    };
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let remote = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("remote"));
    assert!(resolve_message_receiver(&backend, "flotilla", "flotilla/convoy/work/coder").await.expect("resolve absent").is_none());
    let convoys = backend.using::<Convoy>("flotilla");
    convoys
        .create(
            &InputMeta::builder().name("convoy".into()).build(),
            &ConvoySpec::builder()
                .workflow_ref("workflow".into())
                .project_ref("flotilla".into())
                .role("governor".into())
                .generation(1)
                .build(),
        )
        .await
        .expect("old generation");
    assert!(resolve_message_receiver(&backend, "flotilla", "flotilla/convoy/work/coder").await.expect("resolve absent terminal").is_none());
    let remote_terminals = remote.using::<TerminalSession>("flotilla");
    remote_terminals
        .create(
            &InputMeta::builder()
                .name("terminal".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "convoy".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                ]))
                .build(),
            &holder_spec("convoy"),
        )
        .await
        .expect("remote holder");
    backend
        .replica_writer::<TerminalSession>(NodeId::new("remote"), "flotilla")
        .replace(&remote_terminals.list().await.expect("remote snapshot"), at(20))
        .await
        .expect("replicate");
    let holder = resolve_message_receiver(&backend, "flotilla", "flotilla/convoy/work/coder").await.expect("resolve").expect("holder");
    assert_eq!(holder.object.metadata.name, "terminal");
    assert!(matches!(holder.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == NodeId::new("remote")));
    convoys
        .create(
            &InputMeta::builder().name("next-convoy".into()).build(),
            &ConvoySpec::builder()
                .workflow_ref("workflow".into())
                .project_ref("flotilla".into())
                .role("coder".into())
                .generation(2)
                .build(),
        )
        .await
        .expect("new generation");
    backend
        .using::<TerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("new-terminal".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "next-convoy".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                ]))
                .build(),
            &holder_spec("next-convoy"),
        )
        .await
        .expect("new holder");
    assert!(resolve_message_receiver(&backend, "flotilla", "flotilla/coder").await.expect("undeclared role").is_none());
    let declarations = backend.using::<flotilla_resources::ConvoyEnsure>("flotilla");
    let declaration = declarations
        .create(
            &InputMeta::builder().name("coder-holder".into()).build(),
            &flotilla_resources::ConvoyEnsureSpec::builder()
                .project_ref("flotilla".into())
                .role("coder".into())
                .workflow_ref("workflow".into())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("holder declaration");
    let declaration = declarations.get(&declaration.metadata.name).await.expect("current declaration");
    declarations
        .update_status("coder-holder", &declaration.metadata.resource_version, &flotilla_resources::ConvoyEnsureStatus {
            convoy_ref: Some("next-convoy".into()),
            ..Default::default()
        })
        .await
        .expect("declared current holder");
    let holder = resolve_message_receiver(&backend, "flotilla", "flotilla/coder").await.expect("project role").expect("holder");
    assert_eq!(holder.object.metadata.name, "new-terminal");
}

// A crash after creating immutable intent must be repaired by retrying its ID:
// initialise status and supersede the older pending subject exactly once.
#[tokio::test]
async fn retry_repairs_partial_admission_without_reviving_older_intent() {
    use flotilla_resources::{MessageAdmission, MessageInbox};
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let messages = backend.using::<Message>("flotilla");
    let inbox = MessageInbox::new(backend.clone(), "flotilla");
    let intent = spec(
        Some(MessageReference::ChangeRequest { service: "github".into(), scope: "owner/repo".into(), number: 1, revision: "head".into() }),
        MessageExpectation::None,
    );
    inbox.accept(&InputMeta::builder().name("old".into()).build(), &intent, at(1)).await.unwrap();
    messages.create(&InputMeta::builder().name("next".into()).build(), &intent).await.unwrap();
    let MessageAdmission::Accepted(repaired) =
        inbox.accept(&InputMeta::builder().name("next".into()).build(), &intent, at(2)).await.unwrap()
    else {
        panic!("accepted")
    };
    assert!(repaired.status.is_some());
    assert_eq!(messages.get("old").await.unwrap().status.unwrap().phase, MessagePhase::Superseded);
    inbox.accept(&InputMeta::builder().name("third".into()).build(), &intent, at(3)).await.unwrap();
    inbox.accept(&InputMeta::builder().name("next".into()).build(), &intent, at(4)).await.unwrap();
    assert_eq!(messages.get("third").await.unwrap().status.unwrap().phase, MessagePhase::Accepted);
}

// Explicit sender context qualifies both partial roles before persisting intent.
#[tokio::test]
async fn contextual_admission_qualifies_both_roles() {
    use flotilla_resources::{MessageAddressContext, MessageAdmission, MessageInbox};
    let inbox = MessageInbox::new(ResourceBackend::InMemory(InMemoryBackend::default()), "flotilla");
    let mut intent = spec(None, MessageExpectation::None);
    intent.sender = "reviewer".into();
    intent.receiver = "coder".into();
    let MessageAdmission::Accepted(record) = inbox
        .accept_in_context(
            &InputMeta::builder().name("relative".into()).build(),
            &intent,
            &MessageAddressContext { project: "project".into(), convoy: "convoy".into(), vessel: "work".into() },
            at(1),
        )
        .await
        .unwrap()
    else {
        panic!("admitted")
    };
    assert_eq!(record.spec.sender, "project/convoy/work/reviewer");
    assert_eq!(record.spec.receiver, "project/convoy/work/coder");
}
