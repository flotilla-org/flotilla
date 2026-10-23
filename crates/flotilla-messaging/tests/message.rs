use chrono::{TimeZone, Utc};
use flotilla_agent_connector::{
    AgentTransport, Delivery, DeliveryTarget, SubmissionRef, TransportId, TransportObservation, TransportOutcome, TransportRouter,
    KEYSTROKE,
};
use flotilla_messaging::*;
use flotilla_protocol::ResourceRef;
use flotilla_resources::{
    InputMeta, Message, MessageExpectation, MessagePhase, MessageReference, MessageRelation, MessageSpec, MessageStatus,
    MessageStatusPatch, ReplicationClass, ResolvedMessageReceiver, Resource, StatusPatch,
};
use flotilla_store::{decode_stored_resource_document, ResourceBackend, REGISTERED_RESOURCE_KINDS};
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use hegel::generators as gs;
use std::sync::Arc;

// Today's deliveries reach a session through the keystroke transport; the
// fakes below stand in for it at the terminal boundary.
fn router<T: AgentTransport + 'static>(transport: &Arc<T>) -> TransportRouter {
    TransportRouter::new().with_transport(transport.clone())
}

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
        _ => MessageExpectation::Outcome { condition: "convoy/work .status.phase == Landed".parse().expect("leaf") },
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
    let mut spec = spec(reference, expectation);
    if tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) == 1 {
        spec.delivery_condition = Some("convoy/work .status.phase == Landed".parse().expect("delivery leaf"));
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let resolver = ResourceBackend::new(InMemoryBackend::default()).using::<Message>("flotilla");
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
    use flotilla_messaging::{MessageAdmission, MessageInbox};
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
            let backend = ResourceBackend::new(InMemoryBackend::default());
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
                flotilla_store::apply_status_patch(
                    &resolver,
                    "first",
                    &MessageStatusPatch::Delivered {
                        receiver: ResolvedMessageReceiver::builder()
                            .crew_id("crew".into())
                            .session("session".into())
                            .delivered_at(at(20))
                            .evidence("accepted".into())
                            .build(),
                        at: at(20),
                    },
                )
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
    use flotilla_messaging::{MessageAdmission, MessageInbox};
    let backend = ResourceBackend::new(InMemoryBackend::default());
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

    inbox.accept(&replacement_meta, &replacement, at(30)).await.expect("explicit successor");
    assert_eq!(resolver.get("original").await.expect("original").status.expect("status").phase, MessagePhase::Superseded);
    replacement.interrupting = true;
    assert!(inbox.accept(&InputMeta::builder().name("interrupt".into()).build(), &replacement, at(40)).await.is_err());
}

// Concurrent admissions for one subject leave exactly one pending successor;
// the inbox serializes the check and supersession for all cloned producers.
#[tokio::test]
async fn concurrent_same_subject_admissions_leave_one_pending_message() {
    use flotilla_messaging::MessageInbox;
    let backend = ResourceBackend::new(InMemoryBackend::default());
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

    use flotilla_messaging::resolve_message_receiver;
    use flotilla_protocol::NodeId;
    use flotilla_resources::{Convoy, ConvoySpec, ResourceProvenance, TerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let remote = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("remote"));
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
        .update_status(
            "coder-holder",
            &declaration.metadata.resource_version,
            &flotilla_resources::ConvoyEnsureStatus { convoy_ref: Some("next-convoy".into()), ..Default::default() },
        )
        .await
        .expect("declared current holder");
    let holder = resolve_message_receiver(&backend, "flotilla", "flotilla/coder").await.expect("project role").expect("holder");
    assert_eq!(holder.object.metadata.name, "new-terminal");
}

// A crash after creating immutable intent must be repaired by retrying its ID:
// initialise status and supersede the older pending subject exactly once.
#[tokio::test]
async fn retry_repairs_partial_admission_without_reviving_older_intent() {
    use flotilla_messaging::{MessageAdmission, MessageInbox};
    let backend = ResourceBackend::new(InMemoryBackend::default());
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
    use flotilla_messaging::{MessageAdmission, MessageInbox};
    use flotilla_resources::MessageAddressContext;
    let inbox = MessageInbox::new(ResourceBackend::new(InMemoryBackend::default()), "flotilla");
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

// Suppression during recovery must keep returning its canonical predecessor,
// including after the predecessor's expectation closes.
#[tokio::test]
async fn replay_of_suppressed_partial_admission_returns_canonical_predecessor() {
    use flotilla_messaging::{MessageAdmission, MessageInbox};
    use flotilla_store::apply_status_patch;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let inbox = MessageInbox::new(backend.clone(), "flotilla");
    let messages = backend.using::<Message>("flotilla");
    let first = spec(
        Some(MessageReference::ControlRecord {
            resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "convoy"),
            revision: "same".into(),
        }),
        MessageExpectation::Reply,
    );
    inbox.accept(&InputMeta::builder().name("first".into()).build(), &first, at(10)).await.unwrap();
    let mut next = first.clone();
    let subject = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "convoy"),
        revision: "same".into(),
    };
    next.subject = Some(subject.clone());
    next.references = vec![subject];
    messages.create(&InputMeta::builder().name("partial".into()).build(), &next).await.unwrap();
    apply_status_patch(
        &messages,
        "first",
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew".into())
                .session("session".into())
                .delivered_at(at(20))
                .evidence("receipt".into())
                .build(),
            at: at(20),
        },
    )
    .await
    .unwrap();
    for now in [21, 22] {
        let restarted = MessageInbox::new(backend.clone(), "flotilla");
        let admission = restarted.accept(&InputMeta::builder().name("partial".into()).build(), &next, at(now)).await.unwrap();
        assert!(matches!(admission, MessageAdmission::Suppressed { predecessor } if predecessor.metadata.name == "first"));
    }
    apply_status_patch(
        &messages,
        "first",
        &MessageStatusPatch::Finish { phase: MessagePhase::Answered, reason: "reply received".into(), at: at(23) },
    )
    .await
    .unwrap();
    let admission = inbox.accept(&InputMeta::builder().name("partial".into()).build(), &next, at(24)).await.unwrap();
    assert!(matches!(admission, MessageAdmission::Suppressed { predecessor } if predecessor.metadata.name == "first"));
    assert_eq!(messages.get("partial").await.unwrap().status.unwrap().phase, MessagePhase::Superseded);
}

#[tokio::test]
async fn four_part_addresses_select_the_role_within_a_shared_vessel() {
    use std::collections::BTreeMap;

    use flotilla_messaging::resolve_message_receiver;
    use flotilla_resources::{Convoy, ConvoySpec, TerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder().name("convoy".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build(),
        )
        .await
        .unwrap();
    for role in ["coder", "reviewer"] {
        let mut intent = holder_spec("convoy");
        intent.role = role.into();
        backend
            .using::<TerminalSession>("flotilla")
            .create(
                &InputMeta::builder()
                    .name(role.into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "convoy".into()),
                        (VESSEL_LABEL.into(), "work".into()),
                        (ROLE_LABEL.into(), role.into()),
                    ]))
                    .build(),
                &intent,
            )
            .await
            .unwrap();
    }
    for role in ["coder", "reviewer"] {
        let receiver = resolve_message_receiver(&backend, "flotilla", &format!("flotilla/convoy/work/{role}")).await.unwrap().unwrap();
        assert_eq!(receiver.object.metadata.name, role);
    }
    assert!(resolve_message_receiver(&backend, "flotilla", "flotilla/convoy/work/absent").await.unwrap().is_none());
}

// #3005: a delivery pass reads receiver evidence once across distinct addresses,
// including an empty batch; unresolved roles wait without submitting input.
#[tokio::test]
async fn delivery_receiver_reads_are_bounded() {
    use flotilla_messaging::MessageInbox;
    use flotilla_resources::{Convoy, ConvoySpec};
    use flotilla_store_testkit::ReadCountsBackendExt;
    let mut counts = Vec::new();
    for (n, max_reads) in [(0, 3), (10, 30), (200, 410)] {
        let memory = InMemoryBackend::default().with_read_counts();
        let backend = ResourceBackend::new(memory.clone());
        backend
            .using::<Convoy>("flotilla")
            .create(&InputMeta::builder().name("convoy".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
            .await
            .expect("convoy");
        let inbox = MessageInbox::new(backend.clone(), "flotilla");
        for index in 0..n {
            let mut intent = spec(None, MessageExpectation::Reply);
            intent.receiver = format!("flotilla/convoy/work/role-{index}");
            inbox.accept(&InputMeta::builder().name(format!("message-{index}")).build(), &intent, at(10)).await.expect("admit");
        }
        let transport = Arc::new(FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: TransportOutcome::NotSubmitted { reason: "unused".into() },
            accepted: Default::default(),
            working: Default::default(),
        });
        let before = memory.read_snapshot();
        inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("delivery pass");
        let calls = memory.read_snapshot().since(&before).calls_by_kind();
        // Preserve the measured whole-pass read budget without noisy output.
        let reads = calls.values().sum::<usize>();
        assert!(reads <= max_reads, "active={n}: {reads} reads exceed the {max_reads}-read pass budget: {calls:?}");
        counts.push(["Convoy", "TerminalSession", "ConvoyEnsure"].map(|kind| calls.get(kind).copied().unwrap_or_default()));
        assert!(transport.submissions.lock().expect("submissions").is_empty());
        for message in backend.using::<Message>("flotilla").list().await.expect("messages").items {
            assert_eq!(message.status.expect("waiting status").phase, MessagePhase::Accepted);
        }
    }
    assert_eq!(counts[0], [0; 3], "idle passes do not load receiver evidence");
    assert_eq!(counts[1], [1, 1, 0], "one scoped query per addressed kind; explicit roles need no ensures");
    assert_eq!(counts[1], counts[2]);
}

// A snapshot is stable within its batch and a new batch sees holder changes.
// Repeated pure resolutions, including non-agent and invalid addresses, do no reads.
#[hegel::test]
fn receiver_snapshot_is_pure_and_refreshes(tc: hegel::TestCase) {
    use flotilla_messaging::MessageReceiverSnapshot;
    use flotilla_resources::{Convoy, ConvoySpec, TerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
    use flotilla_store_testkit::ReadCountsBackendExt;
    // Empty through many lookups; every case pins missing, invalid, system and principal addresses.
    let repetitions = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let memory = InMemoryBackend::default().with_read_counts();
        let backend = ResourceBackend::new(memory.clone());
        backend
            .using::<Convoy>("flotilla")
            .create(&InputMeta::builder().name("convoy".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
            .await
            .expect("convoy");
        let old = MessageReceiverSnapshot::load(&backend, "flotilla").await.expect("snapshot");
        let address = "flotilla/convoy/work/coder";
        let before = memory.read_snapshot();
        for _ in 0..=repetitions {
            assert!(old.resolve(address).expect("absent").is_none());
            assert!(old.resolve("system:test").expect("system").is_none());
            assert!(old.resolve("principal:test").expect("principal").is_none());
            assert!(old.resolve("invalid").is_err());
        }
        assert!(memory.read_snapshot().since(&before).is_empty());
        backend
            .using::<TerminalSession>("flotilla")
            .create(
                &InputMeta::builder()
                    .name("holder".into())
                    .labels(std::collections::BTreeMap::from([
                        (CONVOY_LABEL.into(), "convoy".into()),
                        (ROLE_LABEL.into(), "coder".into()),
                        (VESSEL_LABEL.into(), "work".into()),
                    ]))
                    .build(),
                &holder_spec("convoy"),
            )
            .await
            .expect("holder");
        assert!(old.resolve(address).expect("old snapshot").is_none());
        let fresh = MessageReceiverSnapshot::load(&backend, "flotilla").await.expect("new snapshot");
        assert_eq!(fresh.resolve(address).expect("resolve").expect("holder").object.metadata.name, "holder");
    });
}

// Boundary fake for terminal submission and externally observed acceptance.
struct FakeMessageTransport {
    submissions: std::sync::Mutex<Vec<String>>,
    observations: std::sync::atomic::AtomicUsize,
    outcome: TransportOutcome,
    accepted: std::sync::atomic::AtomicBool,
    working: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl AgentTransport for FakeMessageTransport {
    fn id(&self) -> TransportId {
        KEYSTROKE.into()
    }
    async fn observe(&self, _: &DeliveryTarget, _: Option<&SubmissionRef>) -> Result<TransportObservation, String> {
        self.observations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(TransportObservation {
            ready: true,
            working: self.working.load(std::sync::atomic::Ordering::SeqCst),
            evidence: self.accepted.load(std::sync::atomic::Ordering::SeqCst).then(|| "later hook receipt".into()),
            ..Default::default()
        })
    }
    async fn submit(&self, batch: &Delivery) -> TransportOutcome {
        self.submissions.lock().expect("submissions").push(batch.text.clone());
        self.outcome.clone()
    }
    async fn poll(&self, _: &Delivery) -> TransportOutcome {
        self.outcome.clone()
    }
}
async fn delivery_inbox() -> (ResourceBackend, flotilla_messaging::MessageInbox) {
    delivery_inbox_from(&["flotilla/checks"; 3]).await
}

/// The standard holder with one Message per sender, `message-0` first.
async fn delivery_inbox_from(senders: &[&str]) -> (ResourceBackend, flotilla_messaging::MessageInbox) {
    use flotilla_resources::*;
    use flotilla_store::*;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder().name("convoy".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build(),
        )
        .await
        .expect("convoy");
    let terminals = backend.using::<TerminalSession>("flotilla");
    let holder = terminals
        .create(
            &InputMeta::builder()
                .name("terminal".into())
                .labels(std::collections::BTreeMap::from([
                    (CONVOY_LABEL.into(), "convoy".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                ]))
                .build(),
            &holder_spec("convoy"),
        )
        .await
        .expect("holder");
    terminals
        .update_status(
            "terminal",
            &holder.metadata.resource_version,
            &TerminalSessionStatus {
                phase: TerminalSessionPhase::Running,
                session_id: Some("session".into()),
                crew: Some(CrewSessionStatus { id: "crew".into(), adapter: "codex".into(), model: None, stance: "work".into() }),
                ..Default::default()
            },
        )
        .await
        .expect("running");
    let inbox = MessageInbox::new(backend.clone(), "flotilla");
    for (index, sender) in senders.iter().enumerate() {
        let mut intent = spec(None, MessageExpectation::None);
        intent.body = format!("body-{index}");
        intent.sender = (*sender).into();
        inbox.accept(&InputMeta::builder().name(format!("message-{index}")).build(), &intent, at(10)).await.expect("message");
    }
    (backend, inbox)
}

// Messages at one turn boundary form one FIFO batch. A held submission keeps
// observing and later evidence resolves it without typing again, even on restart.
#[tokio::test]
async fn delivery_batches_fifo_and_held_batches_resolve_without_retyping() {
    use std::sync::atomic::Ordering;
    let (backend, inbox) = delivery_inbox().await;
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Unconfirmed { reason: "acceptance not yet observed".into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("submit");
    // Reconstruct the inbox as on restart: the durable submission still forbids retyping.
    let restarted = flotilla_messaging::MessageInbox::new(backend.clone(), "flotilla");
    restarted.reconcile_delivery(&router(&transport), at(21)).await.expect("held observation");
    assert_eq!(transport.submissions.lock().expect("submissions").len(), 1);
    let text = transport.submissions.lock().expect("submissions")[0].clone();
    assert!(text.find("body-0").expect("find expected body in batch") < text.find("body-1").expect("find expected body in batch"));
    assert!(text.find("body-1").expect("find expected body in batch") < text.find("body-2").expect("find expected body in batch"));
    assert!(
        backend.using::<flotilla_resources::Demand>("flotilla").list().await.expect("demands").items.is_empty(),
        "allow late evidence before hold escalation"
    );
    restarted.reconcile_delivery(&router(&transport), at(320)).await.expect("hold deadline");
    assert_eq!(backend.using::<flotilla_resources::Demand>("flotilla").list().await.expect("demands").items.len(), 1);
    transport.accepted.store(true, Ordering::SeqCst);
    restarted.reconcile_delivery(&router(&transport), at(321)).await.expect("later evidence");
    assert_eq!(transport.observations.load(Ordering::SeqCst), 4);
    assert_eq!(transport.submissions.lock().expect("submissions").len(), 1);
    assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.iter().all(|message| message
        .status
        .as_ref()
        .expect("status")
        .phase
        == MessagePhase::Delivered));
    assert!(backend.using::<flotilla_resources::Demand>("flotilla").list().await.expect("demands").items.is_empty());
}

// Definitely unsent input backs off between counted attempts and stops at
// the durable budget; stopping retries does not stop attention observations.
#[tokio::test]
async fn genuinely_unsent_batches_back_off_and_stop_after_three_attempts() {
    let (backend, inbox) = delivery_inbox().await;
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::NotSubmitted { reason: "pool unavailable before typing".into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    for second in [20, 21, 80, 81, 200, 500] {
        inbox.reconcile_delivery(&router(&transport), at(second)).await.expect("reconcile");
    }
    assert_eq!(transport.submissions.lock().expect("submissions").len(), 3);
    assert_eq!(transport.observations.load(std::sync::atomic::Ordering::SeqCst), 6);
    for message in backend.using::<Message>("flotilla").list().await.expect("messages").items {
        let retry = message.status.expect("status").retry.expect("retry");
        assert_eq!(retry.attempts, 3);
        assert!(matches!(retry.disposition, flotilla_resources::ControllerRetryDisposition::Terminal { .. }));
    }
    inbox
        .accept(&InputMeta::builder().name("later".into()).build(), &spec(None, MessageExpectation::None), at(501))
        .await
        .expect("admit Message fixture");
    inbox.reconcile_delivery(&router(&transport), at(502)).await.expect("reconcile receiver delivery");
    assert_eq!(
        transport.submissions.lock().expect("lock captured transport evidence").len(),
        3,
        "later intent cannot bypass exhausted FIFO head"
    );
    let later = backend.using::<Message>("flotilla").get("later").await.expect("read Message fixture");
    assert!(later.status.expect("Message fixture has durable status").submission.is_none());
    inbox.fail_batch("message-0", "drop the exhausted batch", at(503)).await.expect("operator releases all exhausted members");
    for name in ["message-0", "message-1", "message-2"] {
        assert_eq!(
            backend.using::<Message>("flotilla").get(name).await.expect("closed batch member").status.expect("status").phase,
            MessagePhase::DeadLettered
        );
    }
    inbox.reconcile_delivery(&router(&transport), at(504)).await.expect("later intent can proceed");
    assert_eq!(transport.submissions.lock().expect("submissions").len(), 4);

    assert_eq!(transport.observations.load(std::sync::atomic::Ordering::SeqCst), 8, "held attention refreshes");
}

// #3455: a held batch of system turns does not hold an operator or supervisor
// message behind it. The successor supersedes the held system turns, keeping
// their submission audit and naming itself in their reason, and is delivered.
// A held batch carrying anyone else's intent stays held, and the successor
// waits behind it. While held, the receiver's gate names the batch, why it is
// held, and the command that clears it. Covers both ways a batch is held:
// ambiguous acceptance and an exhausted definitely-unsent budget.
#[hegel::test]
fn operator_message_supersedes_held_system_turns(tc: hegel::TestCase) {
    let ambiguous = tc.draw(gs::booleans());
    let mixed = tc.draw(gs::booleans());
    let operator = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let senders = ["system:turn-rules", "system:charter", if mixed { "flotilla/convoy/work/reviewer" } else { "system:turn-rules" }];
        let (backend, inbox) = delivery_inbox_from(&senders).await;
        let failing = Arc::new(FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: if ambiguous {
                TransportOutcome::Unconfirmed { reason: "submission was not confirmed".into() }
            } else {
                TransportOutcome::NotSubmitted { reason: "terminal was not ready before submission".into() }
            },
            accepted: Default::default(),
            working: Default::default(),
        });
        let held_at: &[i64] = if ambiguous { &[20, 400] } else { &[20, 21, 80, 81, 200, 500] };
        for second in held_at {
            inbox.reconcile_delivery(&router(&failing), at(*second)).await.expect("hold the batch");
        }
        let messages = backend.using::<Message>("flotilla");
        let held = flotilla_resources::HeldMessageBatch::collect(&messages.list().await.expect("messages").items);
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(held[0].members, ["message-0", "message-1", "message-2"]);
        assert_eq!(held[0].system_only, !mixed);
        let gates = backend.using::<flotilla_resources::Demand>("flotilla").list().await.expect("demands").items;
        assert_eq!(gates.len(), 1);
        let description = gates[0]
            .metadata
            .annotations
            .get(flotilla_resources::delivery_hold::DELIVERY_HOLD_REASON_ANNOTATION)
            .expect("the gate names its hold");
        assert!(description.contains(&format!("message batch `{}`", held[0].batch)), "{description}");
        assert!(description.contains("delivery held"), "{description}");
        assert!(description.contains("flotilla resource fail-message-batch message-0 --namespace flotilla"), "{description}");

        let successor = MessageSpec::builder()
            .sender(if operator { "principal:operator" } else { "flotilla/supervisor/work/bosun" }.into())
            .receiver("flotilla/convoy/work/coder".into())
            .relation(MessageRelation::Supervisor)
            .body("resume: the PR merged; complete the convoy".into())
            .build();
        inbox.accept(&InputMeta::builder().name("resume".into()).build(), &successor, at(501)).await.expect("operator resume");
        let accepting = Arc::new(FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: TransportOutcome::Accepted { evidence: "accepted by holder".into() },
            accepted: Default::default(),
            working: Default::default(),
        });
        inbox.reconcile_delivery(&router(&accepting), at(502)).await.expect("deliver past the held system turns");
        let resume = messages.get("resume").await.expect("resume").status.expect("status");
        let members = ["message-0", "message-1", "message-2"];
        if mixed {
            assert!(accepting.submissions.lock().expect("submissions").is_empty(), "another's held intent is never bypassed");
            assert!(!resume.phase.is_terminal() && resume.resolved_receiver.is_none());
            for name in members {
                let status = messages.get(name).await.expect("member").status.expect("status");
                assert!(!status.phase.is_terminal(), "{name} stays held");
            }
            return;
        }
        assert_eq!(resume.phase, MessagePhase::Delivered);
        assert_eq!(accepting.submissions.lock().expect("submissions").as_slice(), [frame_message(&successor)]);
        let who = if operator { "operator" } else { "supervisor" };
        for name in members {
            let status = messages.get(name).await.expect("member").status.expect("status");
            assert_eq!(status.phase, MessagePhase::Superseded);
            let reason = status.reason.expect("audit reason");
            assert!(reason.starts_with(&format!("superseded while held by {who} message `resume`: delivery held")), "{reason}");
            assert_eq!(status.submission.is_some(), ambiguous, "the submission audit is kept");
        }
        assert!(backend.using::<flotilla_resources::Demand>("flotilla").list().await.expect("demands").items.is_empty());
    });
}

// Delivery leaves reply/outcome expectations open. A correlated durable reply
// or a true admitted Leaf closes them, while unknown outcome evidence waits.
#[tokio::test]
async fn delivered_expectations_settle_from_reply_and_outcome_evidence() {
    use flotilla_resources::{Convoy, ConvoyPhase, ConvoyStatus};
    use TransportOutcome;
    let (backend, inbox) = delivery_inbox().await;
    for (name, expectation) in [
        ("reply-request", MessageExpectation::Reply),
        ("outcome-request", MessageExpectation::Outcome { condition: "convoy/convoy .status.phase == Active".parse().expect("condition") }),
    ] {
        inbox.accept(&InputMeta::builder().name(name.into()).build(), &spec(None, expectation), at(11)).await.expect("expectation");
    }
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Accepted { evidence: "accepted by holder".into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("deliver");
    inbox.reconcile_delivery(&router(&transport), at(21)).await.expect("unknown outcome");
    let messages = backend.using::<Message>("flotilla");
    assert_eq!(messages.get("outcome-request").await.expect("outcome").status.expect("status").phase, MessagePhase::Delivered);
    let mut reply = spec(None, MessageExpectation::None);
    std::mem::swap(&mut reply.sender, &mut reply.receiver);
    reply.in_reply_to = Some("reply-request".into());
    inbox.accept(&InputMeta::builder().name("reply".into()).build(), &reply, at(22)).await.expect("reply");
    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.get("convoy").await.expect("convoy");
    convoys
        .update_status("convoy", &convoy.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() })
        .await
        .expect("active evidence");
    inbox.reconcile_delivery(&router(&transport), at(23)).await.expect("settle");
    assert_eq!(messages.get("outcome-request").await.expect("outcome").status.expect("status").phase, MessagePhase::OutcomeMet);
    assert_eq!(messages.get("reply-request").await.expect("reply").status.expect("status").phase, MessagePhase::Answered);
}

// A single Working observation can be a redraw flicker. Only sustained
// post-submission Working evidence confirms a pending batch.
#[tokio::test]
async fn working_flicker_does_not_establish_delivery() {
    use std::sync::atomic::Ordering;
    let (backend, inbox) = delivery_inbox().await;
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Pending,
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("submit");
    for (second, working) in [(21, true), (22, false), (23, true)] {
        transport.working.store(working, Ordering::SeqCst);
        inbox.reconcile_delivery(&router(&transport), at(second)).await.expect("observe flicker");
        assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.iter().all(|message| message
            .status
            .as_ref()
            .expect("status")
            .phase
            == MessagePhase::Deliverable));
    }
    inbox.reconcile_delivery(&router(&transport), at(28)).await.expect("stable working");
    assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.iter().all(|message| message
        .status
        .as_ref()
        .expect("status")
        .phase
        == MessagePhase::Delivered));
    assert_eq!(transport.submissions.lock().expect("submissions").len(), 1);
}

// An uncertain submission cannot be superseded as definitely undelivered.
// Queue its successor behind the recorded batch; later delivery either opens
// an expectation and suppresses the successor, or permits its independent turn.
#[tokio::test]
async fn same_subject_successor_waits_for_uncertain_predecessor_acceptance() {
    use std::sync::atomic::Ordering;

    use flotilla_messaging::MessageAdmission;
    for expectation in [MessageExpectation::None, MessageExpectation::Reply] {
        let (backend, inbox) = delivery_inbox().await;
        let original = spec(
            Some(MessageReference::ChangeRequest {
                service: "github.com".into(),
                scope: "org/repo".into(),
                number: 1,
                revision: "head".into(),
            }),
            expectation.clone(),
        );
        inbox.accept(&InputMeta::builder().name("original".into()).build(), &original, at(11)).await.expect("original");
        let transport = Arc::new(FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: TransportOutcome::Unconfirmed { reason: "possible submission".into() },
            accepted: Default::default(),
            working: Default::default(),
        });
        inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("possible input");
        let mut successor = original.clone();
        successor.body = "newer body".into();
        assert!(matches!(
            inbox
                .accept(&InputMeta::builder().name("successor".into()).build(), &successor, at(21))
                .await
                .expect("successor intent retained"),
            MessageAdmission::Accepted(_)
        ));
        inbox.reconcile_delivery(&router(&transport), at(21)).await.expect("held pass");
        let messages = backend.using::<Message>("flotilla");
        assert_eq!(messages.get("original").await.expect("original").status.expect("status").phase, MessagePhase::Deliverable);
        assert!(messages
            .get("successor")
            .await
            .expect("successor")
            .status
            .expect("status")
            .reason
            .expect("waiting reason")
            .starts_with("waiting behind recorded batch"));
        assert_eq!(transport.submissions.lock().expect("submissions").len(), 1);
        transport.accepted.store(true, Ordering::SeqCst);
        inbox.reconcile_delivery(&router(&transport), at(22)).await.expect("late receipt");
        inbox.reconcile_delivery(&router(&transport), at(23)).await.expect("successor disposition");
        if expectation == MessageExpectation::Reply {
            assert_eq!(messages.get("successor").await.expect("successor").status.expect("status").phase, MessagePhase::Superseded);
            assert_eq!(transport.submissions.lock().expect("submissions").len(), 1);
        } else {
            assert_eq!(transport.submissions.lock().expect("submissions").len(), 2);
            assert!(transport.submissions.lock().expect("submissions")[1].contains("newer body"));
        }
    }
}

// Recovery finishes an interrupted admission before batch selection, so its
// superseded predecessor never becomes a second terminal input after restart.
#[tokio::test]
async fn delivery_repairs_interrupted_admission_before_transport() {
    use flotilla_messaging::MessageInbox;
    let (backend, inbox) = delivery_inbox().await;
    let messages = backend.using::<Message>("flotilla");
    for message in messages.list().await.expect("list Message fixtures").items {
        messages.delete(&message.metadata.name).await.expect("remove Message fixture");
    }
    let mut original = spec(
        Some(MessageReference::ChangeRequest { service: "github".into(), scope: "owner/repo".into(), number: 1, revision: "head".into() }),
        MessageExpectation::None,
    );
    original.body = "older payload".into();
    inbox.accept(&InputMeta::builder().name("old".into()).build(), &original, at(10)).await.expect("admit Message fixture");
    original.body = "recovered payload".into();
    messages.create(&InputMeta::builder().name("partial".into()).build(), &original).await.expect("create Message fixture");
    let restarted = MessageInbox::new(backend, "flotilla");
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Accepted { evidence: "receipt".into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    restarted.reconcile_delivery(&router(&transport), at(20)).await.expect("reconcile receiver delivery");
    {
        let inputs = transport.submissions.lock().expect("lock captured transport evidence");
        assert_eq!(inputs.len(), 1);
        assert!(inputs[0].contains("recovered payload"));
        assert!(!inputs[0].contains("older payload"));
    }
    assert_eq!(
        messages.get("old").await.expect("read Message fixture").status.expect("Message fixture has durable status").phase,
        MessagePhase::Superseded
    );
    assert_eq!(
        messages.get("partial").await.expect("read Message fixture").status.expect("Message fixture has durable status").phase,
        MessagePhase::Delivered
    );
}

// A deadline can close an unanswered known delivery, but cannot erase possible
// input. A notification already accepted remains satisfied past its deadline.
#[tokio::test]
async fn deadlines_preserve_acceptance_and_unresolved_submission() {
    use flotilla_resources::*;
    use flotilla_store::*;
    for (name, expectation, expected) in [
        ("notification", MessageExpectation::None, MessagePhase::Satisfied),
        ("reply", MessageExpectation::Reply, MessagePhase::Expired),
        ("uncertain", MessageExpectation::Reply, MessagePhase::Deliverable),
    ] {
        let (backend, inbox) = delivery_inbox().await;
        let messages = backend.using::<Message>("flotilla");
        for record in messages.list().await.expect("list Message fixtures").items {
            messages.delete(&record.metadata.name).await.expect("remove Message fixture");
        }
        let transport = Arc::new(FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: TransportOutcome::Unconfirmed { reason: "possible input".into() },
            accepted: Default::default(),
            working: Default::default(),
        });
        let mut intent = spec(None, expectation);
        intent.deadline = Some(at(25));
        inbox.accept(&InputMeta::builder().name(name.into()).build(), &intent, at(10)).await.expect("admit Message fixture");
        inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("reconcile receiver delivery");
        if name != "uncertain" {
            apply_status_patch(
                &messages,
                name,
                &MessageStatusPatch::Delivered {
                    receiver: ResolvedMessageReceiver::builder()
                        .crew_id("crew".into())
                        .session("session".into())
                        .delivered_at(at(21))
                        .evidence("later transport receipt".into())
                        .build(),
                    at: at(21),
                },
            )
            .await
            .expect("apply fixture status transition");
        }
        inbox.reconcile_delivery(&router(&transport), at(30)).await.expect("reconcile receiver delivery");
        let status = messages.get(name).await.expect("read Message fixture").status.expect("Message fixture has durable status");
        assert_eq!(status.phase, expected);
        assert_eq!(status.resolved_receiver.is_some(), name != "uncertain");
        assert!(status.submission.is_some(), "the original batch identity survives deadline handling");
        assert_eq!(transport.submissions.lock().expect("lock captured transport evidence").len(), 1);
    }
}

// Records accepted by A lack the optional delivery sequence introduced in B.
// Updating their status must not place their original intent behind new input.
#[tokio::test]
async fn older_admissions_without_sequence_keep_creation_order() {
    let (backend, inbox) = delivery_inbox().await;
    let messages = backend.using::<Message>("flotilla");
    for record in messages.list().await.expect("list Message fixtures").items {
        messages.delete(&record.metadata.name).await.expect("remove Message fixture");
    }
    let mut intent = spec(None, MessageExpectation::None);
    intent.body = "body-0".into();
    let older = messages.create(&InputMeta::builder().name("older".into()).build(), &intent).await.expect("create Message fixture");
    // This is A's stored accepted shape, not a mutation of B's immutable sequence.
    messages
        .update_status(
            "older",
            &older.metadata.resource_version,
            &MessageStatus::builder()
                .phase(MessagePhase::Accepted)
                .since(older.metadata.creation_timestamp)
                .reason("waiting for receiver resolution".into())
                .build(),
        )
        .await
        .expect("persist fixture status");
    for index in 1..3 {
        intent.body = format!("body-{index}");
        inbox.accept(&InputMeta::builder().name(format!("newer-{index}")).build(), &intent, at(10)).await.expect("admit Message fixture");
    }
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Unconfirmed { reason: "uncertain input".into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("reconcile receiver delivery");
    let inputs = transport.submissions.lock().expect("lock captured transport evidence");
    assert_eq!(inputs.len(), 1);
    assert!(
        inputs[0].find("body-0").expect("find expected body in batch") < inputs[0].find("body-1").expect("find expected body in batch")
    );
    assert!(
        inputs[0].find("body-1").expect("find expected body in batch") < inputs[0].find("body-2").expect("find expected body in batch")
    );
}

// A hung adapter must release admission within a bounded interval. A timeout
// after submission remains uncertain; observation timeouts prove no input.
struct HangingMessageTransport {
    stage: &'static str,
    inner: FakeMessageTransport,
}
#[async_trait::async_trait]
impl AgentTransport for HangingMessageTransport {
    fn id(&self) -> TransportId {
        KEYSTROKE.into()
    }
    async fn observe(&self, holder: &DeliveryTarget, submission: Option<&SubmissionRef>) -> Result<TransportObservation, String> {
        if self.stage == "observe" {
            return std::future::pending().await;
        }
        AgentTransport::observe(&self.inner, holder, submission).await
    }
    async fn submit(&self, batch: &Delivery) -> TransportOutcome {
        let outcome = AgentTransport::submit(&self.inner, batch).await;
        if self.stage == "submit" {
            return std::future::pending().await;
        }
        outcome
    }
    async fn poll(&self, batch: &Delivery) -> TransportOutcome {
        if self.stage == "poll" {
            return std::future::pending().await;
        }
        AgentTransport::poll(&self.inner, batch).await
    }
    async fn release(&self, _: &Delivery) {
        if self.stage == "release" {
            std::future::pending::<()>().await;
        }
    }
}
#[tokio::test(start_paused = true)]
async fn transport_timeouts_release_admission_without_retyping_uncertain_input() {
    use TransportOutcome;
    for stage in ["observe", "submit", "poll", "release"] {
        let (backend, inbox) = delivery_inbox().await;
        let transport = Arc::new(HangingMessageTransport {
            stage,
            inner: FakeMessageTransport {
                submissions: Default::default(),
                observations: Default::default(),
                outcome: if stage == "release" {
                    TransportOutcome::Accepted { evidence: "receipt".into() }
                } else {
                    TransportOutcome::Pending
                },
                accepted: Default::default(),
                working: Default::default(),
            },
        });
        let started = tokio::time::Instant::now();
        inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("reconcile receiver delivery");
        if stage == "poll" {
            inbox.reconcile_delivery(&router(&transport), at(21)).await.expect("reconcile receiver delivery");
        }
        assert!(started.elapsed() >= std::time::Duration::from_secs(30), "{stage} is bounded");
        inbox
            .accept(&InputMeta::builder().name("after-timeout".into()).build(), &spec(None, MessageExpectation::None), at(22))
            .await
            .expect("admit Message fixture");
        let status = backend
            .using::<Message>("flotilla")
            .get("message-0")
            .await
            .expect("read Message fixture")
            .status
            .expect("Message fixture has durable status");
        assert_eq!(status.submission.is_some(), stage != "observe");
        assert_eq!(status.resolved_receiver.is_some(), stage == "release");
        assert_eq!(transport.inner.submissions.lock().expect("lock captured transport evidence").len(), usize::from(stage != "observe"));
    }
}

// A crash can leave one member satisfied while the remaining batch receipt is
// unwritten. Recover the original receiver without reopening the terminal one.
#[tokio::test]
async fn partial_receipt_recovery_keeps_terminal_members_closed() {
    use flotilla_resources::*;
    use flotilla_store::*;
    let (backend, inbox) = delivery_inbox().await;
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Pending,
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("reconcile receiver delivery");
    let messages = backend.using::<Message>("flotilla");
    let receipt = ResolvedMessageReceiver::builder()
        .crew_id("crew".into())
        .session("session".into())
        .delivered_at(at(21))
        .evidence("receipt before crash".into())
        .build();
    apply_status_patch(&messages, "message-0", &MessageStatusPatch::Delivered { receiver: receipt.clone(), at: at(21) })
        .await
        .expect("apply fixture status transition");
    apply_status_patch(
        &messages,
        "message-0",
        &MessageStatusPatch::Finish { phase: MessagePhase::Satisfied, reason: "notification accepted".into(), at: at(21) },
    )
    .await
    .expect("apply fixture status transition");
    let settled = messages.get("message-0").await.expect("read Message fixture");
    MessageInbox::new(backend.clone(), "flotilla")
        .reconcile_delivery(&router(&transport), at(22))
        .await
        .expect("reconcile receiver delivery");
    assert_eq!(messages.get("message-0").await.expect("read Message fixture").status, settled.status, "terminal receipt is immutable");
    for name in ["message-1", "message-2"] {
        let status = messages.get(name).await.expect("read Message fixture").status.expect("Message fixture has durable status");
        assert_eq!(status.phase, MessagePhase::Delivered);
        assert_eq!(status.resolved_receiver, Some(receipt.clone()));
    }
    assert_eq!(transport.submissions.lock().expect("lock captured transport evidence").len(), 1);
}

// Explicit replacement closes even an open delivered expectation. It never
// aliases the replacement ID to the original or treats it as a subject repeat.
#[tokio::test]
async fn explicit_supersession_replaces_open_expectations() {
    use flotilla_messaging::{MessageAdmission, MessageInbox};
    use flotilla_store::apply_status_patch;
    for expectation in [MessageExpectation::Reply, MessageExpectation::None] {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let inbox = MessageInbox::new(backend.clone(), "flotilla");
        let messages = backend.using::<Message>("flotilla");
        let original = spec(None, expectation);
        inbox.accept(&InputMeta::builder().name("original".into()).build(), &original, at(10)).await.expect("original admission");
        apply_status_patch(
            &messages,
            "original",
            &MessageStatusPatch::Delivered {
                receiver: ResolvedMessageReceiver::builder()
                    .crew_id("crew".into())
                    .session("session".into())
                    .delivered_at(at(11))
                    .evidence("receipt".into())
                    .build(),
                at: at(11),
            },
        )
        .await
        .expect("original receipt");
        let mut replacement = original;
        replacement.supersedes = Some("original".into());
        let admitted = inbox
            .accept(&InputMeta::builder().name("replacement".into()).build(), &replacement, at(12))
            .await
            .expect("explicit replacement");
        assert!(matches!(admitted, MessageAdmission::Accepted(message) if message.metadata.name == "replacement"));
        assert_eq!(messages.get("original").await.expect("original").status.expect("status").phase, MessagePhase::Superseded);
    }
}

// Closing an ambiguous batch is an explicit operator decision. It preserves the
// recorded receiver and input audit, and the next independent intent can proceed.
#[tokio::test]
async fn operator_failure_releases_a_held_inbox_without_fabricating_receipt() {
    let (backend, inbox) = delivery_inbox().await;
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Unconfirmed { reason: "possible input".into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("held input");
    assert!(inbox.fail_batch("message-0", "", at(21)).await.is_err());
    inbox.fail_batch("message-0", "checked original session; cancel remaining intent", at(21)).await.expect("operator closure");
    for message in backend.using::<Message>("flotilla").list().await.expect("audit").items {
        let status = message.status.expect("status");
        assert_eq!(status.phase, MessagePhase::DeadLettered);
        assert!(status.submission.is_some(), "preserve ambiguous attempt audit");
        assert!(status.resolved_receiver.is_none(), "closure must not invent acceptance");
    }
    inbox
        .accept(&InputMeta::builder().name("next".into()).build(), &spec(None, MessageExpectation::None), at(22))
        .await
        .expect("next intent");
    inbox.reconcile_delivery(&router(&transport), at(23)).await.expect("next batch");
    assert_eq!(transport.submissions.lock().expect("submissions").len(), 2);
}

#[tokio::test]
async fn system_addresses_are_distinct_and_correlated_replies_are_storable() {
    use flotilla_messaging::MessageInbox;
    use flotilla_resources::{qualify_message_address, MessageAddressContext};
    let context = MessageAddressContext { project: "flotilla".into(), convoy: "convoy".into(), vessel: "work".into() };
    assert_eq!(qualify_message_address("system:checks", &context).expect("system address"), "system:checks");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let inbox = MessageInbox::new(backend.clone(), "flotilla");
    let mut request = spec(None, MessageExpectation::Reply);
    request.sender = "system:checks".into();
    inbox.accept(&InputMeta::builder().name("request".into()).build(), &request, at(10)).await.expect("system request");
    let mut reply = request;
    std::mem::swap(&mut reply.sender, &mut reply.receiver);
    reply.in_reply_to = Some("request".into());
    reply.expectation = MessageExpectation::None;
    inbox.accept(&InputMeta::builder().name("reply".into()).build(), &reply, at(11)).await.expect("system reply");
    assert_eq!(backend.using::<Message>("flotilla").get("reply").await.expect("reply").spec.receiver, "system:checks");
}

// Resource admission must remain available during every external transport
// boundary. The transport calls back through the real inbox, detecting a lock
// held over I/O without depending on an implementation mutex accessor.
#[tokio::test]
async fn transport_callbacks_can_admit_intent_without_waiting_for_delivery() {
    use flotilla_messaging::MessageInbox;
    struct AdmittingTransport {
        inbox: MessageInbox,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl AdmittingTransport {
        async fn admit(&self) {
            let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inbox
                .accept(&InputMeta::builder().name(format!("callback-{index}")).build(), &spec(None, MessageExpectation::None), at(21))
                .await
                .expect("admission during transport call");
        }
    }
    #[async_trait::async_trait]
    impl AgentTransport for AdmittingTransport {
        fn id(&self) -> TransportId {
            KEYSTROKE.into()
        }
        async fn observe(&self, _: &DeliveryTarget, _: Option<&SubmissionRef>) -> Result<TransportObservation, String> {
            self.admit().await;
            Ok(TransportObservation { ready: true, ..Default::default() })
        }
        async fn submit(&self, _: &Delivery) -> TransportOutcome {
            self.admit().await;
            TransportOutcome::Pending
        }
        async fn poll(&self, _: &Delivery) -> TransportOutcome {
            self.admit().await;
            TransportOutcome::Accepted { evidence: "receipt".into() }
        }
        async fn release(&self, _: &Delivery) {
            self.admit().await;
        }
    }
    let (_, inbox) = delivery_inbox().await;
    let transport = Arc::new(AdmittingTransport { inbox: inbox.clone(), calls: Default::default() });
    for second in [20, 22] {
        tokio::time::timeout(std::time::Duration::from_secs(2), inbox.reconcile_delivery(&router(&transport), at(second)))
            .await
            .expect("delivery must not block reentrant admission")
            .expect("delivery pass");
    }
    assert_eq!(transport.calls.load(std::sync::atomic::Ordering::SeqCst), 5, "observe/submit/observe/poll/release admitted independently");
}

// A project role can run as an internal crew role. Its correlated reply is
// authorized by the recorded holder incarnation, not address-string equality.
#[tokio::test]
async fn project_role_expectations_accept_replies_from_the_recorded_qualified_crew() {
    use flotilla_messaging::MessageInbox;
    use flotilla_resources::{ConvoyEnsure, ConvoyEnsureSpec, ConvoyEnsureStatus};
    let (backend, _) = delivery_inbox().await;
    let inbox = MessageInbox::new(backend.clone(), "flotilla");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    ensures
        .create(
            &InputMeta::builder().name("governor-holder".into()).build(),
            &ConvoyEnsureSpec::builder().project_ref("flotilla".into()).role("governor".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("standing declaration");
    let declaration = ensures.get("governor-holder").await.expect("local standing declaration status version");
    ensures
        .update_status(
            "governor-holder",
            &declaration.metadata.resource_version,
            &ConvoyEnsureStatus { convoy_ref: Some("convoy".into()), ..Default::default() },
        )
        .await
        .expect("admitted standing holder");
    let mut request = spec(None, MessageExpectation::Reply);
    request.sender = "system:checks".into();
    request.receiver = "flotilla/governor".into();
    inbox.accept(&InputMeta::builder().name("request".into()).build(), &request, at(11)).await.expect("project-role request");
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Accepted { evidence: "original receiver accepted".into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("deliver to project-role holder");
    let mut reply = spec(None, MessageExpectation::None);
    reply.sender = "other/convoy/work/coder".into();
    reply.receiver = "system:checks".into();
    reply.in_reply_to = Some("request".into());
    inbox.accept(&InputMeta::builder().name("wrong-project".into()).build(), &reply, at(21)).await.expect("uncorrelated sender intent");
    inbox.reconcile_delivery(&router(&transport), at(22)).await.expect("refuse wrong-project reply evidence");
    assert_eq!(
        backend.using::<Message>("flotilla").get("request").await.expect("request").status.expect("status").phase,
        MessagePhase::Delivered
    );
    reply.sender = "flotilla/convoy/work/coder".into();
    inbox.accept(&InputMeta::builder().name("reply".into()).build(), &reply, at(23)).await.expect("qualified crew reply");
    backend
        .using::<flotilla_resources::TerminalSession>("flotilla")
        .delete("terminal")
        .await
        .expect("holder can disappear after publishing its reply");
    inbox.reconcile_delivery(&router(&transport), at(24)).await.expect("settle recorded holder reply after teardown");
    assert_eq!(
        backend.using::<Message>("flotilla").get("request").await.expect("request").status.expect("status").phase,
        MessagePhase::Answered
    );
}

// Idle prompt redraws cannot acknowledge input. Changed output paired with fresh
// Working acknowledges the original batch without waiting for screen debounce.
#[tokio::test]
async fn changed_output_requires_fresh_working_to_resolve_message_hold() {
    struct ScreenTransport {
        changed: std::sync::atomic::AtomicBool,
        working: std::sync::atomic::AtomicBool,
        inputs: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl AgentTransport for ScreenTransport {
        fn id(&self) -> TransportId {
            KEYSTROKE.into()
        }
        async fn observe(&self, _: &DeliveryTarget, _: Option<&SubmissionRef>) -> Result<TransportObservation, String> {
            Ok(TransportObservation {
                ready: true,
                working: self.working.load(std::sync::atomic::Ordering::SeqCst),
                output_digest: Some(if self.changed.load(std::sync::atomic::Ordering::SeqCst) { "redraw" } else { "before" }.into()),
                ..Default::default()
            })
        }
        async fn submit(&self, _: &Delivery) -> TransportOutcome {
            self.inputs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            TransportOutcome::Unconfirmed { reason: "possible input".into() }
        }
        async fn poll(&self, _: &Delivery) -> TransportOutcome {
            TransportOutcome::Pending
        }
    }
    let (backend, inbox) = delivery_inbox().await;
    let transport = Arc::new(ScreenTransport { changed: Default::default(), working: Default::default(), inputs: Default::default() });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("record held input");
    transport.changed.store(true, std::sync::atomic::Ordering::SeqCst);
    inbox.reconcile_delivery(&router(&transport), at(21)).await.expect("idle redraw");
    assert!(backend.using::<Message>("flotilla").list().await.expect("held audit").items.iter().all(|message| message
        .status
        .as_ref()
        .expect("status")
        .phase
        == MessagePhase::Deliverable));
    transport.working.store(true, std::sync::atomic::Ordering::SeqCst);
    inbox.reconcile_delivery(&router(&transport), at(22)).await.expect("fresh Working with changed output");
    assert!(backend.using::<Message>("flotilla").list().await.expect("receipts").items.iter().all(|message| message
        .status
        .as_ref()
        .expect("status")
        .phase
        == MessagePhase::Delivered));
    assert_eq!(transport.inputs.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// A crash after durable batch closure but before gate deletion must not leave
// stale operator attention. Unrelated demands remain untouched.
#[tokio::test]
async fn closed_batch_recovery_clears_only_its_existing_delivery_gate() {
    use flotilla_resources::{Demand, MessageStatusPatch, TerminalSession};
    use flotilla_store::apply_status_patch;
    let (backend, inbox) = delivery_inbox().await;
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Unconfirmed { reason: "held input".into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("held batch");
    inbox.reconcile_delivery(&router(&transport), at(320)).await.expect("hold escalation");
    let demands = backend.using::<Demand>("flotilla");
    assert_eq!(demands.list().await.expect("delivery gate").items.len(), 1);
    let holder = backend.using::<TerminalSession>("flotilla").get("terminal").await.expect("holder");
    let (mut meta, manual) = flotilla_resources::delivery_hold::delivery_demand(&holder);
    meta.name = "manual-review".into();
    demands.create(&meta, &manual).await.expect("unrelated operator gate");
    for member in ["message-0", "message-1", "message-2"] {
        apply_status_patch(
            &backend.using::<Message>("flotilla"),
            member,
            &MessageStatusPatch::Finish {
                phase: MessagePhase::DeadLettered,
                reason: "operator closure persisted before process exit".into(),
                at: at(321),
            },
        )
        .await
        .expect("durable closure before crash");
    }
    flotilla_messaging::MessageInbox::new(backend, "flotilla")
        .reconcile_delivery(&router(&transport), at(322))
        .await
        .expect("restart repairs gate cleanup");
    let remaining = demands.list().await.expect("remaining operator attention").items;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].metadata.name, "manual-review");
    assert_eq!(transport.submissions.lock().expect("inputs").len(), 1);
}

// The next-generation pre-roll guard refuses every legacy authority witness
// before typed decoding can discard retired fields; clean new shapes pass.
#[hegel::test]
fn migration_guard_refuses_legacy_authority_witnesses(tc: hegel::TestCase) {
    let variant = tc.draw(gs::integers::<u8>().min_value(0).max_value(4));
    let clean = tc.draw(gs::booleans());
    let mut document = match variant {
        0 => serde_json::json!({"kind":"TerminalSession", "status":{"legacy_message_receipts":{}}}),
        1 => serde_json::json!({"kind":"TerminalSession", "spec":{"source":{"message":null}}}),
        2 | 3 => serde_json::json!({"kind":"Convoy", "status":{"turn_deliveries":{"source":{}}}}),
        _ => serde_json::json!({"kind":"Message", "status":{"submission":{"batch_id":"new"}}}),
    };
    if !clean {
        match variant {
            0 => document["status"]["legacy_message_receipts"]["receipt"] = serde_json::json!({"sender":"system:old"}),
            1 => document["spec"]["source"]["message"] = serde_json::json!({"id":"old"}),
            2 => document["status"]["turn_deliveries"]["source"]["pending_brief"] = serde_json::json!({"content":"old"}),
            3 => document["status"]["turn_deliveries"]["source"]["pending_supervisor_turn"] = serde_json::json!({"message":{"id":"old"}}),
            _ => document["status"]["submission"]["legacy_launch"] = serde_json::json!({"terminal":"old"}),
        }
    }
    assert_eq!(flotilla_store::validate_message_migration_complete(&document).is_ok(), clean);
}

// Legacy launch witnesses with ordinary receiver receipts or explicit operator
// closure have no unresolved uncertainty and may be retired by the next decoder.
#[tokio::test]
async fn migration_guard_accepts_resolved_legacy_launches() {
    for status in [
        serde_json::json!({"phase":"delivered", "resolved_receiver":{"crew_id":"crew", "session":"session", "delivered_at":at(20), "evidence":"receipt"}}),
        serde_json::json!({"phase":"dead_lettered", "reason":"operator failed batch: explicitly resolved"}),
    ] {
        let mut document = serde_json::json!({"kind":"Message", "status":status});
        document["status"]["submission"] = serde_json::json!({"legacy_launch":{"terminal":"old"}});
        flotilla_store::validate_message_migration_complete(&document).expect("resolved witness is eligible");
    }
}

// Both backing stores exclude terminal history, retain batch members/replies,
// and preserve exact producer replay after age-based body compaction.
#[tokio::test]
async fn indexed_message_history_and_retention_contract() {
    use flotilla_messaging::MessageInbox;
    use flotilla_resources::MessageSubmission;
    use flotilla_store::MessageQuery;
    for backend in [
        ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()),
        ResourceBackend::new(SqliteBackend::open_in_memory().expect("sqlite")),
    ] {
        let resolver = backend.using::<Message>("flotilla");
        let inbox = MessageInbox::new(backend.clone(), "flotilla").with_audit_retention_days(1);
        let intent = spec(None, MessageExpectation::None);
        for index in 0..256 {
            let meta = InputMeta::builder().name(format!("history-{index}")).build();
            inbox.accept(&meta, &intent, at(10)).await.expect("admit history");
            let record = resolver.get(&meta.name).await.expect("record");
            let mut status = record.status.unwrap();
            status.phase = MessagePhase::Expired;
            status.since = at(20);
            if index == 2 {
                status.phase = MessagePhase::Superseded;
                status.canonical_predecessor = Some(ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "history-1"));
            }
            resolver.update_status(&meta.name, &record.metadata.resource_version, &status).await.expect("terminal history");
        }
        let active_meta = InputMeta::builder().name("active".into()).build();
        inbox.accept(&active_meta, &intent, at(30)).await.expect("active input");
        assert_eq!(resolver.query(&MessageQuery::active()).await.unwrap().len(), 1);
        assert_eq!(resolver.query(&MessageQuery::Active { receiver: Some("other/role".into()) }).await.unwrap().len(), 0);
        let active = resolver.get("active").await.unwrap();
        let mut status = active.status.unwrap();
        status.submission = Some(
            MessageSubmission::builder()
                .batch_id("recovery".into())
                .crew_id("crew".into())
                .session("session".into())
                .started_at(at(30))
                .members(vec!["active".into(), "history-0".into()])
                .build(),
        );
        resolver.update_status("active", &active.metadata.resource_version, &status).await.unwrap();
        assert_eq!(resolver.query(&MessageQuery::Batch { id: "recovery".into() }).await.unwrap().len(), 1);
        assert_eq!(inbox.compact_audit(at(86420)).await.unwrap(), 0, "exact retention boundary keeps bodies");
        assert_eq!(inbox.compact_audit(at(86421)).await.unwrap(), 255, "active recovery protects its terminal member");
        let compacted = resolver.get("history-1").await.unwrap();
        assert!(compacted.spec.body.is_empty());
        assert!(compacted.spec.body_digest.is_some());
        assert_eq!(compacted.status.unwrap().phase, MessagePhase::Expired);
        let replay = inbox.accept(&InputMeta::builder().name("history-1".into()).build(), &intent, at(90000)).await.unwrap();
        assert!(matches!(replay, flotilla_messaging::MessageAdmission::Accepted(_)));
        let suppressed = inbox.accept(&InputMeta::builder().name("history-2".into()).build(), &intent, at(90000)).await.unwrap();
        assert!(
            matches!(suppressed, flotilla_messaging::MessageAdmission::Suppressed { predecessor } if predecessor.metadata.name == "history-1")
        );
        assert!(!resolver.get("history-0").await.unwrap().spec.body.is_empty());
        assert_eq!(inbox.compact_audit(at(90000)).await.unwrap(), 0, "compaction is idempotent");
        let reply_spec = MessageSpec::builder()
            .sender("flotilla/receiver".into())
            .receiver("flotilla/checks".into())
            .relation(MessageRelation::Peer)
            .body("reply".into())
            .in_reply_to("active".into())
            .build();
        inbox.accept(&InputMeta::builder().name("reply".into()).build(), &reply_spec, at(40)).await.unwrap();
        assert_eq!(backend.query_messages("flotilla", &MessageQuery::ReplyTo { name: "active".into() }).await.unwrap().len(), 1);
        resolver.delete("reply").await.unwrap();
        assert!(backend.query_messages("flotilla", &MessageQuery::ReplyTo { name: "active".into() }).await.unwrap().is_empty());
    }
}

// New-shape submissions retain receiver-owned receipts across compaction;
// unresolved transport evidence is retained with its body for explicit recovery.
#[tokio::test]
async fn audit_compaction_keeps_receipts_and_uncertain_submissions() {
    use flotilla_messaging::MessageInbox;
    use flotilla_resources::MessageSubmission;
    let (backend, _) = delivery_inbox().await;
    let inbox = MessageInbox::new(backend.clone(), "flotilla").with_audit_retention_days(1);
    let messages = backend.using::<Message>("flotilla");
    for uncertain in [false, true] {
        let name = if uncertain { "uncertain" } else { "receipt" };
        let intent = spec(None, MessageExpectation::None);
        inbox.accept(&InputMeta::builder().name(name.into()).build(), &intent, at(10)).await.unwrap();
        let record = messages.get(name).await.unwrap();
        let mut status = record.status.unwrap();
        status.phase = if uncertain { MessagePhase::DeadLettered } else { MessagePhase::Satisfied };
        status.since = at(20);
        status.submission = Some(
            MessageSubmission::builder()
                .batch_id(name.into())
                .crew_id("crew".into())
                .session("session".into())
                .started_at(at(10))
                .members(vec![name.into()])
                .build(),
        );
        if !uncertain {
            status.resolved_receiver = Some(
                ResolvedMessageReceiver::builder()
                    .crew_id("crew".into())
                    .session("session".into())
                    .delivered_at(at(15))
                    .evidence("receipt".into())
                    .build(),
            );
        }
        messages.update_status(name, &record.metadata.resource_version, &status).await.unwrap();
        if !uncertain {
            let convoys = backend.using::<flotilla_resources::Convoy>("flotilla");
            let convoy = convoys.get("convoy").await.unwrap();
            let mut convoy_status = convoy.status.unwrap_or_default();
            let state = flotilla_resources::CrewWorkState::builder()
                .phase(flotilla_resources::CrewWorkPhase::Done)
                .pending_follow_up(ResourceRef::new("flotilla.work/v1", "Message", "flotilla", name))
                .build();
            convoy_status.crew_work.entry("work".into()).or_default().insert("coder".into(), state);
            let convoy = convoys.update_status("convoy", &convoy.metadata.resource_version, &convoy_status).await.unwrap();
            assert_eq!(inbox.compact_audit(at(90000)).await.unwrap(), 0, "pending workflow continuation keeps its body");
            assert!(!messages.get(name).await.unwrap().spec.body.is_empty());
            convoy_status.crew_work.get_mut("work").unwrap().get_mut("coder").unwrap().pending_follow_up = None;
            convoys.update_status("convoy", &convoy.metadata.resource_version, &convoy_status).await.unwrap();
        }
        inbox.compact_audit(at(90000)).await.unwrap();
        let compacted = messages.get(name).await.unwrap();
        assert_eq!(compacted.status.as_ref(), Some(&status));
        assert_eq!(compacted.spec.body.is_empty(), !uncertain);
        assert_eq!(MessageInbox::new(backend.clone(), "flotilla").with_audit_retention_days(0).compact_audit(at(900000)).await.unwrap(), 0);
    }
}

// Indexed results agree with the stored view after each generated lifecycle
// step, including deletion/recreation and receiver-home replica replacement.
#[hegel::test]
fn message_indexes_follow_authority_and_replica_lifecycles(tc: hegel::TestCase) {
    use flotilla_protocol::NodeId;

    use flotilla_store::MessageQuery;
    // Names collide deliberately; operations span empty, duplicate, terminal,
    // and recreated records. Replica snapshots follow every authority step.
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let operations: Vec<_> = (0..steps)
        .map(|_| (tc.draw(gs::integers::<usize>().min_value(0).max_value(3)), tc.draw(gs::integers::<u8>().min_value(0).max_value(2))))
        .collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        for backend in [
            ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()),
            ResourceBackend::new(SqliteBackend::open_in_memory().unwrap()),
        ] {
            let resolver = backend.using::<Message>("flotilla");
            for (step, (id, operation)) in operations.iter().enumerate() {
                let name = format!("message-{id}");
                match operation {
                    0 => {
                        if matches!(resolver.get(&name).await, Err(flotilla_resources::ResourceError::NotFound { .. })) {
                            let mut intent = spec(None, MessageExpectation::None);
                            intent.in_reply_to = Some(format!("request-{}", id % 2));
                            resolver.create(&InputMeta::builder().name(name.clone()).build(), &intent).await.unwrap();
                        }
                    }
                    1 => {
                        if let Ok(record) = resolver.get(&name).await {
                            if record.status.as_ref().is_none_or(|status| !status.phase.is_terminal()) {
                                resolver
                                    .update_status(
                                        &name,
                                        &record.metadata.resource_version,
                                        &MessageStatus::builder().phase(MessagePhase::Expired).since(at(step as i64)).build(),
                                    )
                                    .await
                                    .unwrap();
                            }
                        }
                    }
                    _ => {
                        if resolver.get(&name).await.is_ok() {
                            resolver.delete(&name).await.unwrap();
                        }
                    }
                }
                let snapshot = resolver.list().await.unwrap();
                let expected: std::collections::BTreeSet<_> = snapshot
                    .items
                    .iter()
                    .filter(|record| record.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
                    .map(|record| record.metadata.name.clone())
                    .collect();
                let indexed: std::collections::BTreeSet<_> =
                    resolver.query(&MessageQuery::active()).await.unwrap().into_iter().map(|record| record.metadata.name).collect();
                assert_eq!(indexed, expected);
                backend
                    .replica_writer::<Message>(NodeId::new("receiver-home"), "remote")
                    .replace(&snapshot, at(step as i64))
                    .await
                    .unwrap();
                let replica_active: std::collections::BTreeSet<_> = backend
                    .query_messages("remote", &MessageQuery::active())
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|record| record.object.metadata.name)
                    .collect();
                assert_eq!(replica_active, expected);
                for id in 0..2 {
                    let name = format!("request-{id}");
                    let expected = snapshot.items.iter().filter(|record| record.spec.in_reply_to.as_ref() == Some(&name)).count();
                    assert_eq!(backend.query_messages("remote", &MessageQuery::ReplyTo { name }).await.unwrap().len(), expected);
                }
            }
        }
    });
}

// Growing terminal history does not change the active delivery batch or the
// number of holder observations made by a receiver reconciliation pass.
#[tokio::test]
async fn active_delivery_work_is_independent_of_audit_history() {
    use TransportOutcome;
    for historical in [0, 256] {
        let (backend, inbox) = delivery_inbox().await;
        let messages = backend.using::<Message>("flotilla");
        for id in 0..historical {
            let record = messages
                .create(&InputMeta::builder().name(format!("old-{id}")).build(), &spec(None, MessageExpectation::None))
                .await
                .unwrap();
            messages
                .update_status(
                    &record.metadata.name,
                    &record.metadata.resource_version,
                    &MessageStatus::builder().phase(MessagePhase::Expired).since(at(1)).build(),
                )
                .await
                .unwrap();
        }
        let transport = Arc::new(FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: TransportOutcome::Accepted { evidence: "receipt".into() },
            accepted: Default::default(),
            working: Default::default(),
        });
        inbox.reconcile_delivery(&router(&transport), at(20)).await.unwrap();
        assert_eq!(transport.observations.load(std::sync::atomic::Ordering::SeqCst), 1);
        let submissions = transport.submissions.lock().unwrap().clone();
        assert_eq!(submissions.len(), 1);
        assert!(submissions[0].contains("body-0") && submissions[0].contains("body-1") && submissions[0].contains("body-2"));
        assert!(!submissions[0].contains("review the settled checks"));
        assert_eq!(messages.list().await.unwrap().items.len(), historical + 3, "audit identities stay present");
    }
}

async fn routing_project(backend: &ResourceBackend, name: &str, parent: Option<&str>) {
    use flotilla_resources::{Project, ProjectSpec, RoleDefinition, RoleSubscription};
    let role = RoleDefinition {
        subscriptions: Some(vec![RoleSubscription::builder().topic("supervision".into()).subtree(true).build()]),
        ..Default::default()
    };
    backend
        .definitions::<Project>("flotilla")
        .create(
            &InputMeta::builder().name(name.into()).build(),
            &ProjectSpec::builder()
                .display_name(name.into())
                .maybe_parent(parent.map(str::to_string))
                .role_definitions(std::collections::BTreeMap::from([("guide".into(), role)]))
                .build(),
        )
        .await
        .expect("project");
}

async fn routing_holder(backend: &ResourceBackend, project: &str, generation: &str) {
    use flotilla_resources::*;
    let name = format!("{project}-{generation}");
    backend
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref(project.into()).role("guide".into()).build(),
        )
        .await
        .expect("standing convoy");
    backend
        .using::<TerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name(format!("terminal-{name}"))
                .labels(std::collections::BTreeMap::from([
                    (CONVOY_LABEL.into(), name.clone()),
                    (ROLE_LABEL.into(), "guide".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                ]))
                .build(),
            &holder_spec(&name),
        )
        .await
        .expect("terminal");
    let ensures = backend.definitions::<ConvoyEnsure>("flotilla");
    let declaration = match ensures.get(&format!("{project}-holder")).await {
        Ok(declaration) => declaration,
        Err(_) => ensures
            .create(
                &InputMeta::builder().name(format!("{project}-holder")).build(),
                &ConvoyEnsureSpec::builder().project_ref(project.into()).role("guide".into()).repositories(Vec::new()).build(),
            )
            .await
            .expect("ensure"),
    };
    let declaration = backend.using::<ConvoyEnsure>("flotilla").get(&declaration.metadata.name).await.expect("local declaration");
    backend
        .using::<ConvoyEnsure>("flotilla")
        .update_status(
            &declaration.metadata.name,
            &declaration.metadata.resource_version,
            &ConvoyEnsureStatus { convoy_ref: Some(name), ..Default::default() },
        )
        .await
        .expect("holder pointer");
}

// Supervision skips unoccupied parent levels, stops at the nearest subscribed
// holder, and excludes the sender. Generate every local/parent presence pair.
#[hegel::test]
fn subscription_supervision_follows_live_parent_chain(tc: hegel::TestCase) {
    let local = tc.draw(gs::booleans());
    let parent = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        use flotilla_store::*;
        use flotilla_store_testkit::ReadCountsBackendExt;
        let memory = InMemoryBackend::default().with_read_counts();
        let backend = ResourceBackend::new(memory.clone());
        routing_project(&backend, "root", None).await;
        routing_project(&backend, "parent", Some("root")).await;
        routing_project(&backend, "child", Some("parent")).await;
        routing_holder(&backend, "root", "one").await;
        if parent {
            routing_holder(&backend, "parent", "one").await;
        }
        if local {
            routing_holder(&backend, "child", "one").await;
        }
        let path = supervision_path(&backend, "flotilla", "child", "child/task/work/coder").await.expect("path");
        let expected = if local {
            "child/guide"
        } else if parent {
            "parent/guide"
        } else {
            "root/guide"
        };
        assert_eq!(path[0].address, expected);
        let path = supervision_path(&backend, "flotilla", "child", "child/child-one/work/guide").await.expect("exclude sender");
        assert!(!path.iter().any(|contact| contact.address == "child/guide"));
        let before = memory.read_snapshot();
        let book = crew_address_book(&backend, "flotilla", "child/task/work/coder").await.expect("contacts");
        assert_eq!(book.supervision[0].address, expected);
        // Every presence combination shares one local-plus-replica snapshot,
        // even when supervision and peer contacts resolve several addresses.
        let calls = memory.read_snapshot().since(&before).calls_by_kind();
        for kind in ["Convoy", "TerminalSession", "ConvoyEnsure"] {
            assert_eq!(calls.get(kind).copied().unwrap_or_default(), 2, "one snapshot for {kind}");
        }
        routing_holder(&backend, "root", "two").await;
        let next = crew_address_book(&backend, "flotilla", "child/task/work/coder").await.expect("refreshed contacts");
        assert_eq!(next.supervision.last().expect("root").terminal.as_deref(), Some("terminal-root-two"));
    });
}

// Two applied revisions before a turn boundary leave only the latest pending
// notification; reconciling again or restarting does not create another input.
#[tokio::test]
async fn charter_updates_coalesce_to_latest_revision_once() {
    use flotilla_resources::*;
    use flotilla_store::*;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    routing_project(&backend, "root", None).await;
    routing_holder(&backend, "root", "one").await;
    let projects = backend.definitions::<Project>("flotilla");
    for revision in ["first", "second"] {
        let project = projects.get("root").await.expect("project");
        let mut meta = InputMeta::from(&project.metadata);
        meta.annotations.insert("flotilla.work/charter-revision".into(), revision.into());
        projects.apply(&meta, &project.spec).await.expect("revision");
        reconcile_charter_notifications(&MessageInbox::new(backend.clone(), "flotilla"), at(10)).await.expect("notification");
    }
    reconcile_charter_notifications(&MessageInbox::new(backend.clone(), "flotilla"), at(11)).await.expect("repeat");
    let messages = backend.using::<Message>("flotilla").list().await.expect("messages");
    assert_eq!(messages.items.len(), 2);
    let pending: Vec<_> = messages.items.iter().filter(|message| !message.status.as_ref().expect("admitted").phase.is_terminal()).collect();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].spec.body.contains("charter@second"));
}

// A child supervision topic delivers at the occupied ancestor, and a reply
// from that actual qualified holder answers it even across Project boundaries.
#[tokio::test]
async fn topic_receipt_and_reply_use_the_ancestor_holders_address() {
    use flotilla_resources::*;
    use flotilla_store::*;
    use flotilla_store_testkit::ReadCountsBackendExt;
    for adopted in [false, true] {
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
        routing_project(&backend, "root", None).await;
        routing_project(&backend, "child", Some("root")).await;
        routing_holder(&backend, "root", "one").await;
        let terminals = backend.using::<TerminalSession>("flotilla");
        if adopted {
            let projects = backend.definitions::<Project>("flotilla");
            let project = projects.get("root").await.expect("project");
            let mut shape = project.spec;
            shape.role_definitions.get_mut("guide").expect("role").adoptable = Some(true);
            projects.apply(&InputMeta::from(&project.metadata), &shape).await.expect("adoptable");
            let holder = terminals.get("terminal-root-one").await.expect("holder");
            let mut meta = InputMeta::from(&holder.metadata);
            meta.annotations.insert(ROLE_ADDRESS_ANNOTATION.into(), "root/guide".into());
            terminals.update(&meta, &holder.metadata.resource_version, &holder.spec).await.expect("claim");
        }
        let holder = terminals.get("terminal-root-one").await.expect("holder");
        terminals
            .update_status(
                &holder.metadata.name,
                &holder.metadata.resource_version,
                &TerminalSessionStatus {
                    phase: TerminalSessionPhase::Running,
                    session_id: Some("root-session".into()),
                    crew: Some(CrewSessionStatus { id: "root-crew".into(), adapter: "codex".into(), model: None, stance: "work".into() }),
                    ..Default::default()
                },
            )
            .await
            .expect("running");
        let inbox = MessageInbox::new(backend.clone(), "flotilla");
        let intent = MessageSpec::builder()
            .sender("child/task/work/coder".into())
            .receiver("topic:child/supervision".into())
            .relation(MessageRelation::Supervisor)
            .body("needs ruling".into())
            .expectation(MessageExpectation::Reply)
            .build();
        // Admission rejects malformed topics before they can poison a pass.
        let mut invalid = intent.clone();
        invalid.receiver = "topic:child".into();
        assert!(inbox.accept(&InputMeta::builder().name("invalid-topic".into()).build(), &invalid, at(10)).await.is_err());
        inbox.accept(&InputMeta::builder().name("topic-stall".into()).build(), &intent, at(10)).await.expect("accept");
        let transport = Arc::new(FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: TransportOutcome::Accepted { evidence: "hook receipt".into() },
            accepted: Default::default(),
            working: Default::default(),
        });
        let before = backend.read_snapshot();
        inbox.reconcile_delivery(&router(&transport), at(11)).await.expect("delivery");
        // Topic delivery loads its standing declaration and holder exactly once.
        let reads = backend.read_snapshot().since(&before).objects_by_kind();
        for kind in ["ConvoyEnsure", "Convoy", "TerminalSession"] {
            assert_eq!(reads.get(kind), Some(&1), "topic receiver rows: {reads:?}");
        }
        let messages = backend.using::<Message>("flotilla");
        let delivered = messages.get("topic-stall").await.expect("message");
        let receiver = delivered.status.expect("status").resolved_receiver.expect("receipt");
        let address = if adopted { "root/guide" } else { "root/root-one/work/guide" };
        assert_eq!(receiver.role_address.as_deref(), Some(address));
        let reply = MessageSpec::builder()
            .sender(address.into())
            .receiver(intent.sender)
            .relation(MessageRelation::Supervisor)
            .body("retry".into())
            .in_reply_to("topic-stall".into())
            .build();
        inbox.accept(&InputMeta::builder().name("ruling".into()).build(), &reply, at(12)).await.expect("reply");
        inbox.reconcile_delivery(&router(&transport), at(13)).await.expect("correlation");
        assert_eq!(messages.get("topic-stall").await.expect("message").status.expect("status").phase, MessagePhase::Answered);
    }
}

// Adoptable roles can be held by an existing agent without an ensured Convoy.
// Stopped terminals retire presence; ambiguous live claims are never guessed.
#[tokio::test]
async fn adoptable_role_uses_a_unique_running_terminal_claim() {
    use flotilla_resources::*;
    use flotilla_store::*;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    routing_project(&backend, "root", None).await;
    let projects = backend.definitions::<Project>("flotilla");
    let project = projects.get("root").await.expect("project");
    let mut shape = project.spec;
    shape.role_definitions.insert("attended".into(), RoleDefinition { adoptable: Some(true), ..Default::default() });
    projects.apply(&InputMeta::from(&project.metadata), &shape).await.expect("adoptable definition");
    assert!(resolve_message_receiver(&backend, "flotilla", "root/attended").await.expect("absent").is_none());
    let terminals = backend.using::<TerminalSession>("flotilla");
    for name in ["external-one", "external-two"] {
        terminals
            .create(
                &InputMeta::builder()
                    .name(name.into())
                    .annotations(std::collections::BTreeMap::from([(ROLE_ADDRESS_ANNOTATION.into(), "root/attended".into())]))
                    .build(),
                &holder_spec("external"),
            )
            .await
            .expect("claimed terminal");
        let terminal = terminals.get(name).await.expect("terminal");
        terminals
            .update_status(
                name,
                &terminal.metadata.resource_version,
                &TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() },
            )
            .await
            .expect("running claim");
        if name == "external-one" {
            assert_eq!(
                resolve_message_receiver(&backend, "flotilla", "root/attended")
                    .await
                    .expect("resolve")
                    .expect("claim")
                    .object
                    .metadata
                    .name,
                name
            );
        }
    }
    assert!(resolve_message_receiver(&backend, "flotilla", "root/attended").await.is_err());
    for name in ["external-one", "external-two"] {
        let terminal = terminals.get(name).await.expect("terminal");
        terminals
            .update_status(
                name,
                &terminal.metadata.resource_version,
                &TerminalSessionStatus { phase: TerminalSessionPhase::Stopped, ..Default::default() },
            )
            .await
            .expect("retired");
    }
    assert!(resolve_message_receiver(&backend, "flotilla", "root/attended").await.expect("retired").is_none());
}

// Bad claims are isolated to their terminal; the healthy holder still receives
// its revision on the same pass, including after a reconciler restart.
#[tokio::test]
async fn malformed_adopted_claim_does_not_block_healthy_charter_notifications() {
    use flotilla_resources::*;
    use flotilla_store::*;
    for claim in ["root/guide/extra", "root/", "missing-slash", "unknown/guide", "root/undeclared"] {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        routing_project(&backend, "root", None).await;
        routing_holder(&backend, "root", "one").await;
        let projects = backend.definitions::<Project>("flotilla");
        let project = projects.get("root").await.expect("project");
        let mut meta = InputMeta::from(&project.metadata);
        meta.annotations.insert("flotilla.work/charter-revision".into(), "new".into());
        projects.apply(&meta, &project.spec).await.expect("revision");
        let terminals = backend.using::<TerminalSession>("flotilla");
        let invalid = terminals
            .create(
                &InputMeta::builder()
                    .name("a-invalid".into())
                    .annotations(std::collections::BTreeMap::from([(ROLE_ADDRESS_ANNOTATION.into(), claim.into())]))
                    .build(),
                &holder_spec("external"),
            )
            .await
            .expect("invalid claim fixture");
        terminals
            .update_status(
                "a-invalid",
                &invalid.metadata.resource_version,
                &TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() },
            )
            .await
            .expect("running");
        for _ in 0..2 {
            reconcile_charter_notifications(&MessageInbox::new(backend.clone(), "flotilla"), at(10)).await.expect("healthy notification");
        }
        let messages = backend.using::<Message>("flotilla").list().await.expect("messages");
        assert_eq!(messages.items.len(), 1, "claim {claim}");
        assert_eq!(messages.items[0].spec.receiver, "root/root-one/work/guide");
    }
}

// Structured revision evidence suppresses the initial revision; prose cannot
// suppress a changed revision. Rendering may admit unrelated input, and repeated
// passes do not call a custom renderer again even if it omits revision text.
#[tokio::test]
async fn custom_charter_renderer_uses_structured_revision_and_does_not_lock_admission() {
    use flotilla_resources::*;
    use flotilla_store::*;
    struct Renderer {
        inbox: MessageInbox,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl CharterBriefRenderer for Renderer {
        async fn render(&self, _: CharterBriefInput<'_>) -> Result<String, ResourceError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inbox
                .accept(
                    &InputMeta::builder().name("render-witness".into()).build(),
                    &MessageSpec::builder()
                        .sender("system:renderer".into())
                        .receiver("system:observer".into())
                        .relation(MessageRelation::System)
                        .body("independent admission".into())
                        .build(),
                    at(10),
                )
                .await?;
            Ok("Custom template without revision marker".into())
        }
    }
    let backend = ResourceBackend::new(InMemoryBackend::default());
    routing_project(&backend, "root", None).await;
    routing_holder(&backend, "root", "one").await;
    let terminals = backend.using::<TerminalSession>("flotilla");
    let terminal = terminals.get("terminal-root-one").await.expect("holder");
    let mut meta = InputMeta::from(&terminal.metadata);
    meta.annotations.insert(BRIEF_CHARTER_REVISION_ANNOTATION.into(), "initial".into());
    let mut terminal_spec = terminal.spec;
    let TerminalSessionSource::Agent { brief, .. } = &mut terminal_spec.source else { panic!("agent") };
    brief.content = "User prose: Charter revision: `changed`".into();
    terminals.update(&meta, &terminal.metadata.resource_version, &terminal_spec).await.expect("initial evidence");
    let inbox = MessageInbox::new(backend.clone(), "flotilla");
    let renderer = Renderer { inbox: inbox.clone(), calls: Default::default() };
    let projects = backend.definitions::<Project>("flotilla");
    for revision in ["initial", "changed", "changed"] {
        let project = projects.get("root").await.expect("project");
        let mut meta = InputMeta::from(&project.metadata);
        meta.annotations.insert("flotilla.work/charter-revision".into(), revision.into());
        projects.apply(&meta, &project.spec).await.expect("revision");
        tokio::time::timeout(std::time::Duration::from_secs(2), reconcile_charter_notifications_with_renderer(&inbox, &renderer, at(10)))
            .await
            .expect("render does not hold admission lock")
            .expect("notification");
    }
    assert_eq!(renderer.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let messages = backend.using::<Message>("flotilla").list().await.expect("messages");
    let notifications: Vec<_> = messages.items.iter().filter(|message| message.spec.sender == FLEET_STORE_SENDER).collect();
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].spec.body.contains("Custom template without revision marker"));
    assert!(matches!(&notifications[0].spec.subject, Some(MessageReference::ControlRecord { revision, .. }) if revision == "changed"));
}

// An unknown Project is a configuration error, distinct from an empty but valid
// subscription path. It must never be presented as an unoccupied supervisor.
#[tokio::test]
async fn unknown_project_contact_query_reports_configuration_error() {
    use flotilla_store::*;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let book = crew_address_book(&backend, "flotilla", "missing/task/work/coder").await.expect("diagnostic projection");
    assert_eq!(book.routing_issue.as_deref(), Some("unknown Project `missing`"));
    assert!(book.render().contains("unknown Project `missing`"));
    assert!(!book.render().contains("No current subscriber"));
}

// #2927: release uses today's subject revision and firing condition, including
// changes after submission began. Generate each state transition and replay
// validation twice to cover duplicate release and persistent closure.
#[hegel::test]
fn held_turn_release_checks_revision_and_condition(tc: hegel::TestCase) {
    use flotilla_resources::*;
    let change = tc.draw(gs::integers::<u8>().min_value(0).max_value(6));
    let legacy = tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) == 1;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let (backend, inbox) = delivery_inbox().await;
        let crs = backend.using::<ChangeRequest>("flotilla");
        let name = flotilla_resources::address::record_name(
            &flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 2920, &[])
                .expect("change request address"),
        )
        .expect("record name");
        let record = crs
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("org/repo".into())
                    .number(2920)
                    .observing_authority("test".into())
                    .build(),
            )
            .await
            .unwrap();
        let observation = |head: &str, checks| ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: Observation { value: Some(ObservedChangeRequestState::Open), observed_at: at(30) },
            head_sha: Observation { value: Some(head.into()), observed_at: at(30) },
            checks: Observation { value: checks, observed_at: at(30) },
            review: ChangeRequestReviewObservation { actionable_at_head: Default::default() },
            mergeable: Default::default(),
        };
        crs.update_status(&name, &record.metadata.resource_version, &observation("old", Some(ObservedChecks::Fail))).await.unwrap();
        let reference = MessageReference::ChangeRequest {
            service: "github.com".into(),
            scope: "org/repo".into(),
            number: 2920,
            revision: "old".into(),
        };
        let mut intent = spec(Some(reference), MessageExpectation::None);
        intent.sender = "system:turn-rules".into();
        intent.delivery_condition = Some("cr/github.com/org/repo/2920 .checks == fail".parse().unwrap());
        if legacy {
            intent.delivery_condition = None;
        }
        inbox.accept(&InputMeta::builder().name("turn".into()).build(), &intent, at(10)).await.unwrap();
        if legacy {
            // The receiver may see an old Message before its admission receipt
            // replicates. Missing condition proof must hold the turn.
            assert!(inbox.validate_delivery_members(&["turn".into()], at(31)).await.is_err());
            let convoys = backend.using::<Convoy>("flotilla");
            let convoy = convoys.get("convoy").await.unwrap();
            let rule = TurnDeliveryRule::builder()
                .on(flotilla_resources::parse_leaf_template("$cr.checks == fail").unwrap())
                .to(TurnDeliveryTarget::builder().vessel("work".into()).role("coder".into()).build())
                .brief("checks settled".into())
                .hold(HoldAct::State)
                .build();
            let snapshot = WorkflowSnapshot {
                cascade: None,
                exit: None,
                turn_delivery: [("checks-settled".into(), rule)].into_iter().collect(),
                stall_nudges: Default::default(),
                supervision: None,
                vessels: vec![],
            };
            let episode = TurnDeliveryEpisode::builder()
                .subject_revision("old".into())
                .evidence_at(at(10))
                .judged_claim_at(at(9))
                .outcome(TurnDeliveryOutcome::MessageAccepted {
                    new_turn: true,
                    message: ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "turn"),
                    rung: TurnDeliveryRung::WarmSession,
                    accepted_at: at(10),
                })
                .build();
            convoys
                .update_status(
                    "convoy",
                    &convoy.metadata.resource_version,
                    &ConvoyStatus {
                        phase: ConvoyPhase::Active,
                        workflow_snapshot: Some(snapshot),
                        turn_deliveries: [("checks-settled".into(), TurnDeliveryStatus { episodes: vec![episode], ..Default::default() })]
                            .into_iter()
                            .collect(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }

        let transport = Arc::new(FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: TransportOutcome::Pending,
            accepted: Default::default(),
            working: Default::default(),
        });
        inbox.reconcile_delivery(&router(&transport), at(20)).await.unwrap();
        let record = crs.get(&name).await.unwrap();
        let status = match change {
            0 => observation("old", Some(ObservedChecks::Fail)),
            1 => observation("new", Some(ObservedChecks::Fail)),
            2 => observation("old", Some(ObservedChecks::Pass)),
            _ => observation("old", None),
        };
        let mut status = status;
        if change == 4 {
            status.state.value = Some(ObservedChangeRequestState::Merged);
            status.checks.value = Some(ObservedChecks::Fail);
        }
        if change == 5 {
            status.checks = Observation { value: Some(ObservedChecks::Fail), observed_at: at(-300) };
        }
        if change == 6 {
            status.checks = Observation::known(ObservedChecks::Fail, at(30));
            status.head_sha.observed_at = at(-300);
        }
        crs.update_status(&name, &record.metadata.resource_version, &status).await.unwrap();
        for _ in 0..2 {
            let result = inbox.validate_delivery_members(&["turn".into()], at(31)).await;
            if matches!(change, 3 | 5 | 6) {
                assert!(result.is_err(), "unknown is held, never mistaken for false");
            } else {
                assert_eq!(result.unwrap(), change == 0);
            }
            let status = backend.using::<Message>("flotilla").get("turn").await.unwrap().status.unwrap();
            assert_eq!(status.phase == MessagePhase::Superseded, matches!(change, 1 | 2 | 4));
            assert!(status.resolved_receiver.is_none(), "stale closure never fabricates receipt");
        }
    });
}

// A pending submission becomes an operator HumanGate exactly at five minutes;
// repeated overdue passes preserve one gate and never claim input receipt.
#[tokio::test]
async fn pending_submission_surfaces_at_hold_boundary() {
    use flotilla_resources::Demand;
    use TransportOutcome;
    let (backend, inbox) = delivery_inbox().await;
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::Pending,
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.unwrap();
    inbox.reconcile_delivery(&router(&transport), at(319)).await.unwrap();
    assert!(backend.using::<Demand>("flotilla").list().await.unwrap().items.is_empty());
    for second in [320, 321, 10000] {
        inbox.reconcile_delivery(&router(&transport), at(second)).await.unwrap();
        assert_eq!(backend.using::<Demand>("flotilla").list().await.unwrap().items.len(), 1);
        assert_eq!(transport.submissions.lock().unwrap().len(), 1);
        assert!(backend.using::<Message>("flotilla").get("message-0").await.unwrap().status.unwrap().resolved_receiver.is_none());
    }
}

// Previous-generation admission retries reuse immutable Messages; new guarded
// intents with different firing conditions remain distinct.
#[test]
fn delivery_guard_admission_compatibility() {
    let legacy = spec(None, MessageExpectation::None);
    let mut current = legacy.clone();
    current.delivery_condition = Some("convoy/work .status.phase == Active".parse().unwrap());
    assert!(legacy.same_intent(&current));
    assert!(current.same_intent(&legacy));
    let mut other = current.clone();
    other.delivery_condition = Some("convoy/work .status.phase == Landed".parse().unwrap());
    assert!(!current.same_intent(&other));
}

// A legacy condition receipt that never replicates is known-unsent, but not
// tight-looped: the deployed retry budget backs off and raises one operator gate.
#[tokio::test]
async fn missing_condition_receipt_exhausts_bounded_retries_without_input() {
    use flotilla_resources::*;
    struct GuardedTransport {
        inbox: MessageInbox,
        inner: FakeMessageTransport,
    }
    #[async_trait::async_trait]
    impl AgentTransport for GuardedTransport {
        fn id(&self) -> TransportId {
            KEYSTROKE.into()
        }
        async fn observe(&self, holder: &DeliveryTarget, submission: Option<&SubmissionRef>) -> Result<TransportObservation, String> {
            self.inner.observe(holder, submission).await
        }
        async fn submit(&self, batch: &Delivery) -> TransportOutcome {
            self.inner.submissions.lock().unwrap().push(batch.submission.batch_id.clone());
            let error = self.inbox.validate_delivery_members(&batch.submission.members, at(20)).await.unwrap_err();
            TransportOutcome::NotSubmitted { reason: error.to_string() }
        }
        async fn poll(&self, _: &Delivery) -> TransportOutcome {
            panic!("known-unsent guard failures have no pending write to poll")
        }
    }
    let (backend, inbox) = delivery_inbox().await;
    let messages = backend.using::<Message>("flotilla");
    for index in 0..3 {
        messages.delete(&format!("message-{index}")).await.unwrap();
    }
    let convoy = backend.using::<Convoy>("flotilla").get("convoy").await.unwrap();
    let mut intent = spec(
        Some(MessageReference::ControlRecord {
            resource: flotilla_protocol::ResourceRef::new(api_version(Convoy::API_PATHS), "Convoy", "flotilla", "convoy"),
            revision: convoy.metadata.resource_version,
        }),
        MessageExpectation::None,
    );
    intent.sender = "system:turn-rules".into();
    inbox.accept(&InputMeta::builder().name("legacy".into()).build(), &intent, at(10)).await.unwrap();
    let transport = Arc::new(GuardedTransport {
        inbox: MessageInbox::new(backend.clone(), "flotilla"),
        inner: FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: TransportOutcome::Pending,
            accepted: Default::default(),
            working: Default::default(),
        },
    });
    for (second, attempts) in [(20, 1), (21, 1), (79, 1), (80, 2), (81, 2), (199, 2), (200, 3), (500, 3), (10000, 3)] {
        inbox.reconcile_delivery(&router(&transport), at(second)).await.unwrap();
        assert_eq!(transport.inner.submissions.lock().unwrap().len(), attempts);
        let status = messages.get("legacy").await.unwrap().status.unwrap();
        assert!(status.resolved_receiver.is_none());
        assert!(status.submission.is_none());
    }
    assert_eq!(backend.using::<Demand>("flotilla").list().await.unwrap().items.len(), 1);
    assert!(matches!(
        messages.get("legacy").await.unwrap().status.unwrap().retry.unwrap().disposition,
        ControllerRetryDisposition::Terminal { .. }
    ));
}

// #3042: a receiver batch reads each named holder kind once; adding unrelated
// convoys, messages, terminals and ensures cannot increase decoding or calls.
#[tokio::test]
async fn addressed_receiver_reads_ignore_unrelated_holders() {
    use flotilla_messaging::{resolve_message_receiver, MessageReceiverSnapshot};
    use flotilla_resources::{
        Convoy, ConvoyEnsure, ConvoyEnsureSpec, ConvoyEnsureStatus, ConvoySpec, TerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
    };
    use flotilla_store_testkit::{assert_flat_reads, ReadCountsBackendExt, ReadSnapshot};
    use std::collections::BTreeMap;
    // Exact per-kind (calls, decoded objects) for Convoy, TerminalSession, ConvoyEnsure, Message.
    fn kinds(reads: &ReadSnapshot) -> [(usize, usize); 4] {
        ["Convoy", "TerminalSession", "ConvoyEnsure", "Message"].map(|kind| {
            (reads.calls_by_kind().get(kind).copied().unwrap_or_default(), reads.objects_by_kind().get(kind).copied().unwrap_or_default())
        })
    }
    async fn resolve_reads(sqlite: bool, unrelated: usize) -> (ReadSnapshot, ReadSnapshot) {
        let backend = if sqlite {
            ResourceBackend::new(SqliteBackend::open_in_memory().expect("sqlite"))
        } else {
            ResourceBackend::new(InMemoryBackend::default())
        }
        .with_read_counts();
        for n in 0..=unrelated {
            let name = if n == 0 { "convoy".into() } else { format!("unrelated-{n}") };
            backend
                .using::<Convoy>("flotilla")
                .create(
                    &InputMeta::builder().name(name.clone()).build(),
                    &ConvoySpec::builder()
                        .workflow_ref("workflow".into())
                        .project_ref("flotilla".into())
                        .role(if n == 0 { "coder".into() } else { "other".into() })
                        .build(),
                )
                .await
                .expect("convoy");
            let terminal = if n == 0 { "terminal".into() } else { format!("terminal-{n}") };
            backend
                .using::<TerminalSession>("flotilla")
                .create(
                    &InputMeta::builder()
                        .name(terminal)
                        .labels(BTreeMap::from([
                            (CONVOY_LABEL.into(), name.clone()),
                            (ROLE_LABEL.into(), "coder".into()),
                            (VESSEL_LABEL.into(), "work".into()),
                        ]))
                        .build(),
                    &holder_spec(&name),
                )
                .await
                .expect("holder");
            let ensure_name = if n == 0 { "ensure".into() } else { format!("ensure-{n}") };
            let ensures = backend.using::<ConvoyEnsure>("flotilla");
            ensures
                .create(
                    &InputMeta::builder().name(ensure_name.clone()).build(),
                    &ConvoyEnsureSpec::builder()
                        .project_ref("flotilla".into())
                        .role(if n == 0 { "coder".into() } else { format!("other-{n}") })
                        .repositories(vec![])
                        .build(),
                )
                .await
                .expect("ensure");
            let ensure = ensures.get(&ensure_name).await.expect("local ensure version");
            ensures
                .update_status(
                    &ensure_name,
                    &ensure.metadata.resource_version,
                    &ConvoyEnsureStatus { convoy_ref: Some(name), ..Default::default() },
                )
                .await
                .expect("ensure status");
            if n > 0 {
                backend
                    .using::<Message>("flotilla")
                    .create(&InputMeta::builder().name(format!("message-{n}")).build(), &spec(None, MessageExpectation::Reply))
                    .await
                    .expect("noise message");
            }
        }
        // #3433: the delivery pass must hydrate only addressed receiver rows,
        // even with unrelated holders and active messages sharing the inbox.
        let inbox = flotilla_messaging::MessageInbox::new(backend.clone(), "flotilla");
        for (index, address) in ["flotilla/convoy/work/coder", "flotilla/coder"].into_iter().enumerate() {
            let mut intent = spec(None, MessageExpectation::Reply);
            intent.receiver = address.into();
            inbox.accept(&InputMeta::builder().name(format!("addressed-{index}")).build(), &intent, at(10)).await.expect("admit");
        }
        let transport = Arc::new(FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: TransportOutcome::NotSubmitted { reason: "unused".into() },
            accepted: Default::default(),
            working: Default::default(),
        });
        let before = backend.read_snapshot();
        inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("delivery pass");
        let pass = backend.read_snapshot().since(&before);
        for kind in ["Convoy", "TerminalSession", "ConvoyEnsure"] {
            assert_eq!(pass.objects_by_kind().get(kind), Some(&1), "delivery receiver rows: {pass}");
        }
        let before = backend.read_snapshot();
        let snapshot = MessageReceiverSnapshot::load_for_addresses(
            &backend,
            "flotilla",
            &["flotilla/convoy/work/coder".into(), "flotilla/coder".into()],
        )
        .await
        .expect("targeted snapshot");
        for address in ["flotilla/convoy/work/coder", "flotilla/coder"] {
            assert_eq!(snapshot.resolve(address).expect("resolve").expect("holder").object.metadata.name, "terminal");
        }
        let batch = backend.read_snapshot().since(&before);
        batch.assert_no_lists("receiver batch");
        assert_eq!(kinds(&batch), [(1, 1), (1, 1), (1, 1), (0, 0)]);
        // A four-part address selects its crew, even when the named convoy
        // contains other sessions. It does not need standing-role holders.
        backend
            .using::<TerminalSession>("flotilla")
            .create(
                &InputMeta::builder()
                    .name("other-crew".into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "convoy".into()),
                        (ROLE_LABEL.into(), "reviewer".into()),
                        (VESSEL_LABEL.into(), "work".into()),
                    ]))
                    .build(),
                &holder_spec("convoy"),
            )
            .await
            .expect("other crew");
        let before = backend.read_snapshot();
        assert_eq!(
            resolve_message_receiver(&backend, "flotilla", "flotilla/convoy/work/coder")
                .await
                .expect("explicit receiver")
                .expect("holder")
                .object
                .metadata
                .name,
            "terminal"
        );
        let resolve = backend.read_snapshot().since(&before);
        resolve.assert_no_lists("message resolve");
        assert_eq!(kinds(&resolve), [(1, 1), (1, 1), (0, 0), (0, 0)]);
        let before = backend.read_snapshot();
        assert!(resolve_message_receiver(&backend, "flotilla", "principal:operator").await.expect("principal").is_none());
        assert!(backend.read_snapshot().since(&before).is_empty(), "principal addresses never load agent holders");
        (batch, resolve)
    }
    for sqlite in [false, true] {
        assert_flat_reads([0, 48], |unrelated| resolve_reads(sqlite, unrelated)).await;
    }
}

// Message pushdown queries classify every persisted phase using the model's
// terminal rule. Deletion must remove stale inbox and audit index entries.
#[tokio::test]
async fn message_query_phase_classification_matches_model() {
    use flotilla_store::MessageQuery;
    for backend in
        [ResourceBackend::new(InMemoryBackend::default()), ResourceBackend::new(SqliteBackend::open_in_memory().expect("sqlite backend"))]
    {
        let messages = backend.using::<Message>("flotilla");
        // Explicit finite generator spans absent status and every phase. Each
        // record starts fresh because terminal message statuses are immutable.
        for phase in std::iter::once(None).chain(MessagePhase::ALL.into_iter().map(Some)) {
            let object = messages
                .create(&InputMeta::builder().name("phase".into()).build(), &spec(None, MessageExpectation::None))
                .await
                .expect("create message");
            if let Some(phase) = phase {
                let status = MessageStatus::builder()
                    .phase(phase)
                    .since(at(20))
                    .reason("phase scenario".into())
                    .maybe_resolved_receiver(phase.has_delivery_evidence().then(|| {
                        ResolvedMessageReceiver::builder()
                            .crew_id("crew".into())
                            .session("terminal".into())
                            .delivered_at(at(20))
                            .evidence("receipt".into())
                            .build()
                    }))
                    .build();
                messages.update_status("phase", &object.metadata.resource_version, &status).await.expect("set valid message phase");
            }
            let terminal = phase.is_some_and(MessagePhase::is_terminal);
            for receiver in [None, Some(object.spec.receiver.clone())] {
                assert_eq!(messages.query(&MessageQuery::Active { receiver }).await.expect("query inbox").len(), usize::from(!terminal));
            }
            assert!(messages
                .query(&MessageQuery::Active { receiver: Some("missing/role".into()) })
                .await
                .expect("query absent receiver")
                .is_empty());
            assert!(messages.query(&MessageQuery::AuditBefore { before: at(20) }).await.expect("query exact cutoff").is_empty());
            assert_eq!(
                messages.query(&MessageQuery::AuditBefore { before: at(21) }).await.expect("query old audit").len(),
                usize::from(terminal)
            );
            messages.delete("phase").await.expect("delete phase record");
            assert!(messages.query(&MessageQuery::active()).await.expect("deleted inbox entry").is_empty());
            assert!(messages.query(&MessageQuery::AuditBefore { before: at(21) }).await.expect("deleted audit entry").is_empty());
        }
    }
}

// The inbox owns the earliest future delivery deadline, regardless of record
// name or insertion order. Empty and terminal-only inboxes have no timer.
#[hegel::test]
fn inbox_delivery_deadline_is_earliest(tc: hegel::TestCase) {
    let delays: Vec<i64> = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(12)))
        .map(|_| tc.draw(gs::integers::<i64>().min_value(1).max_value(120)))
        .collect();
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let inbox = MessageInbox::new(backend.clone(), "flotilla");
        assert_eq!(inbox.next_delivery_wake(at(10)).await.unwrap(), None);
        for (i, delay) in delays.iter().enumerate() {
            let mut intent = spec(None, MessageExpectation::Reply);
            intent.deadline = Some(at(10 + delay));
            inbox.accept(&InputMeta::builder().name(format!("deadline-{i}")).build(), &intent, at(10)).await.unwrap();
        }
        assert_eq!(
            inbox.next_delivery_wake(at(10)).await.unwrap(),
            Some(std::time::Duration::from_secs(*delays.iter().min().unwrap() as u64))
        );
        for i in 0..delays.len() {
            flotilla_store::apply_status_patch(
                &backend.using::<Message>("flotilla"),
                &format!("deadline-{i}"),
                &MessageStatusPatch::Finish { phase: MessagePhase::Expired, reason: "closed".into(), at: at(11) },
            )
            .await
            .unwrap();
        }
        assert_eq!(inbox.next_delivery_wake(at(10)).await.unwrap(), None);
    });
}

// Separate inbox handles share a store, but not their process-local admission
// locks. Recovery racing the original admission must converge on one order.
#[test]
fn concurrent_partial_admissions_preserve_one_acceptance_order() {
    use flotilla_store::{ReadObserver, ReadOperation};
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Barrier,
    };

    #[derive(Debug)]
    struct ConcurrentAdmissionReads {
        armed: AtomicBool,
        arrivals: AtomicUsize,
        barrier: Barrier,
    }
    impl ReadObserver for ConcurrentAdmissionReads {
        fn read(&self, kind: &str, operation: ReadOperation, _: usize) {
            if self.armed.load(Ordering::SeqCst)
                && kind == "Message"
                && operation == ReadOperation::Query
                && self.arrivals.fetch_add(1, Ordering::SeqCst) < 2
            {
                // Message queries notify after releasing the store lock. Both
                // real admissions have now read the same interrupted intent.
                self.barrier.wait();
            }
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    let observer =
        Arc::new(ConcurrentAdmissionReads { armed: AtomicBool::new(false), arrivals: AtomicUsize::new(0), barrier: Barrier::new(2) });
    let backend = ResourceBackend::new(InMemoryBackend::default().with_read_observer(observer.clone()));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let meta = InputMeta::builder().name("racing-admission".into()).build();
    let intent = spec(None, MessageExpectation::Reply);
    runtime.block_on(backend.using::<Message>("flotilla").create(&meta, &intent)).expect("interrupted admission");
    observer.armed.store(true, Ordering::SeqCst);
    let results = std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let inbox = MessageInbox::new(backend.clone(), "flotilla");
                let meta = &meta;
                let intent = &intent;
                scope.spawn(move || {
                    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(inbox.accept(
                        meta,
                        intent,
                        at(10),
                    ))
                })
            })
            .collect();
        tasks.into_iter().map(|task| task.join().expect("admission thread")).collect::<Vec<_>>()
    });
    let admitted: Vec<_> = results
        .into_iter()
        .map(|result| match result.expect("both admissions succeed") {
            MessageAdmission::Accepted(message) => message,
            other => panic!("unexpected admission {other:?}"),
        })
        .collect();
    assert_eq!(admitted[0].status, admitted[1].status);
    assert!(admitted[0].status.as_ref().expect("admitted").accepted_sequence.is_some());
}

// N→N+1 receipts without role_address still accept a reply from the recorded
// incarnation after its standing-role declaration has disappeared.
#[tokio::test]
async fn legacy_project_role_reply_uses_recorded_incarnation() {
    use flotilla_store_testkit::ReadCountsBackendExt;
    let (backend, _) = delivery_inbox().await;
    let backend = backend.with_read_counts();
    let inbox = flotilla_messaging::MessageInbox::new(backend.clone(), "flotilla");
    let messages = backend.using::<Message>("flotilla");
    for index in 0..3 {
        messages.delete(&format!("message-{index}")).await.expect("remove fixture messages");
    }
    let mut request = spec(None, MessageExpectation::Reply);
    request.sender = "system:checks".into();
    request.receiver = "flotilla/governor".into();
    let message = messages.create(&InputMeta::builder().name("legacy-request".into()).build(), &request).await.expect("request");
    messages
        .update_status(
            "legacy-request",
            &message.metadata.resource_version,
            &MessageStatus::builder()
                .phase(MessagePhase::Delivered)
                .since(at(10))
                .reason("legacy receipt".into())
                .resolved_receiver(
                    ResolvedMessageReceiver::builder()
                        .crew_id("crew".into())
                        .session("session".into())
                        .delivered_at(at(10))
                        .evidence("accepted".into())
                        .build(),
                )
                .build(),
        )
        .await
        .expect("legacy status");
    let mut reply = spec(None, MessageExpectation::None);
    reply.sender = "flotilla/convoy/work/coder".into();
    reply.receiver = request.sender;
    reply.in_reply_to = Some("legacy-request".into());
    for index in 0..3 {
        let mut wrong = reply.clone();
        wrong.sender = format!("flotilla/wrong-{index}/work/coder");
        inbox.accept(&InputMeta::builder().name(format!("bad-reply-{index}")).build(), &wrong, at(11)).await.expect("wrong sender");
    }
    inbox.accept(&InputMeta::builder().name("legacy-reply".into()).build(), &reply, at(11)).await.expect("reply");
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::NotSubmitted { reason: "unused".into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    let before = backend.read_snapshot();
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("correlation");
    // One empty adopted-role query plus one recorded-crew query, reused across
    // every correlated reply, including rejected senders.
    let reads = backend.read_snapshot().since(&before);
    assert_eq!(reads.calls_by_kind().get("TerminalSession"), Some(&2));
    assert_eq!(reads.objects_by_kind().get("TerminalSession"), Some(&1));
    assert_eq!(messages.get("legacy-request").await.expect("request").status.expect("status").phase, MessagePhase::Answered);
}

// In-memory transport with a chosen name, standing in for a second agent
// transport (such as the Codex app-server) beside keystroke.
struct NamedTransport {
    id: &'static str,
    outcome: TransportOutcome,
    submissions: std::sync::Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl AgentTransport for NamedTransport {
    fn id(&self) -> TransportId {
        self.id.into()
    }
    async fn observe(&self, _: &DeliveryTarget, _: Option<&SubmissionRef>) -> Result<TransportObservation, String> {
        Ok(TransportObservation { ready: true, ..Default::default() })
    }
    async fn submit(&self, delivery: &Delivery) -> TransportOutcome {
        assert_eq!(delivery.submission.transport.as_ref().map(TransportId::as_str), Some(self.id));
        self.submissions.lock().expect("submissions").push(delivery.text.clone());
        self.outcome.clone()
    }
    async fn poll(&self, delivery: &Delivery) -> TransportOutcome {
        // Per the contract, a submission this transport never took is unconfirmed.
        if !self.submissions.lock().expect("submissions").contains(&delivery.text) {
            return TransportOutcome::Unconfirmed { reason: "no record of this submission".into() };
        }
        self.outcome.clone()
    }
}

// The holder's harness (`codex` in the fixture) offers the named transport
// ahead of keystroke for new turns.
struct PreferNamed(&'static str);
impl flotilla_agent_connector::RouteDeclarations for PreferNamed {
    fn declare(&self, operation: flotilla_agent_connector::Operation, _: Option<&str>) -> flotilla_agent_connector::OperationDeclaration {
        use flotilla_agent_connector::{OperationDeclaration, TransportOffer};
        match operation {
            flotilla_agent_connector::Operation::NewTurn => {
                OperationDeclaration::offering([TransportOffer::new(self.0, "app-server"), TransportOffer::new(KEYSTROKE, "tui")])
            }
            _ => OperationDeclaration::none(),
        }
    }
}

fn two_route_router(first: &Arc<NamedTransport>, keystroke: &Arc<NamedTransport>) -> TransportRouter {
    TransportRouter::new()
        .with_connector("codex", Arc::new(PreferNamed(first.id)))
        .with_transport(keystroke.clone())
        .with_transport(first.clone())
}

fn named(id: &'static str, outcome: TransportOutcome) -> Arc<NamedTransport> {
    Arc::new(NamedTransport { id, outcome, submissions: Default::default() })
}

// Behaviour (#3545, ADR 0059 §4): with two transports registered for the
// holder, every message is delivered exactly once, through the transport the
// router chose, and its receipt names that transport. Three rounds of
// admission cover a first batch of three and later single-message batches.
#[tokio::test]
async fn two_registered_transports_deliver_each_message_once_and_receipt_names_it() {
    let (backend, inbox) = delivery_inbox().await;
    let first = named("codex-app-server", TransportOutcome::Accepted { evidence: "turn/started".into() });
    let keystroke = named(KEYSTROKE, TransportOutcome::Accepted { evidence: "typed".into() });
    let router = two_route_router(&first, &keystroke);
    let mut bodies: Vec<String> = (0..3).map(|index| format!("body-{index}")).collect();
    for round in 0..3 {
        if round > 0 {
            let mut intent = spec(None, MessageExpectation::None);
            intent.body = format!("later-{round}");
            bodies.push(intent.body.clone());
            inbox.accept(&InputMeta::builder().name(format!("later-{round}")).build(), &intent, at(20 + round)).await.expect("admit");
        }
        inbox.reconcile_delivery(&router, at(30 + round)).await.expect("delivery pass");
        let sent: Vec<String> =
            [&first, &keystroke].iter().flat_map(|transport| transport.submissions.lock().expect("submissions").clone()).collect();
        for body in &bodies {
            assert_eq!(sent.iter().filter(|text| text.contains(body.as_str())).count(), 1, "{body} delivered exactly once");
        }
        assert!(keystroke.submissions.lock().expect("submissions").is_empty(), "the second transport never delivers");
        for message in backend.using::<Message>("flotilla").list().await.expect("messages").items {
            let status = message.status.expect("status");
            assert!(status.phase.has_delivery_evidence(), "{:?}", status.phase);
            let receipt = status.resolved_receiver.expect("receipt");
            assert_eq!(receipt.transport.as_deref(), Some("codex-app-server"), "the receipt names the chosen transport");
            assert_eq!(receipt.evidence, "turn/started");
        }
    }
}

// Behaviour (#3566's fallback rule): when the preferred transport refuses a
// send as unsupported, nothing was written, so the batch falls back once to
// keystroke; the receipt names keystroke and later batches to the same
// session go straight to keystroke without retrying the refused transport.
#[tokio::test]
async fn unsupported_transport_falls_back_once_and_receipt_names_the_fallback() {
    let (backend, inbox) = delivery_inbox().await;
    let guarded = named("keystroke-guarded", TransportOutcome::Unsupported { reason: "cleat daemon lacks the composer guard".into() });
    let keystroke = named(KEYSTROKE, TransportOutcome::Accepted { evidence: "typed".into() });
    let router = two_route_router(&guarded, &keystroke);
    inbox.reconcile_delivery(&router, at(20)).await.expect("first batch");
    assert_eq!(guarded.submissions.lock().expect("submissions").len(), 1);
    assert_eq!(keystroke.submissions.lock().expect("submissions").len(), 1);
    let mut intent = spec(None, MessageExpectation::None);
    intent.body = "later".into();
    inbox.accept(&InputMeta::builder().name("later".into()).build(), &intent, at(21)).await.expect("admit");
    inbox.reconcile_delivery(&router, at(22)).await.expect("second batch");
    assert_eq!(guarded.submissions.lock().expect("submissions").len(), 1, "the refused transport is not retried");
    assert_eq!(keystroke.submissions.lock().expect("submissions").len(), 2);
    for message in backend.using::<Message>("flotilla").list().await.expect("messages").items {
        let status = message.status.expect("status");
        assert!(status.phase.has_delivery_evidence(), "{:?}", status.phase);
        assert_eq!(status.resolved_receiver.expect("receipt").transport.as_deref(), Some(KEYSTROKE));
    }
}

// Behaviour: a possibly delivered batch that failed over is recorded against
// the transport that took it, so later polls and the receipt follow that
// transport and nothing is resubmitted.
#[tokio::test]
async fn failed_over_pending_batch_records_and_polls_the_transport_used() {
    let (backend, inbox) = delivery_inbox().await;
    let first = named("codex-app-server", TransportOutcome::NotSubmitted { reason: "app-server not live".into() });
    let keystroke = named(KEYSTROKE, TransportOutcome::Pending);
    let router = two_route_router(&first, &keystroke);
    inbox.reconcile_delivery(&router, at(20)).await.expect("submit");
    let recorded = backend.using::<Message>("flotilla").get("message-0").await.expect("message").status.expect("status");
    assert_eq!(recorded.submission.expect("recorded submission").transport.as_deref(), Some(KEYSTROKE));
    inbox.reconcile_delivery(&router, at(21)).await.expect("poll");
    assert_eq!(first.submissions.lock().expect("submissions").len(), 1);
    assert_eq!(keystroke.submissions.lock().expect("submissions").len(), 1, "polling never resubmits");
}

// Stands in for a transport whose submission hangs past the call bound.
struct HangingSubmitTransport {
    submissions: std::sync::Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl AgentTransport for HangingSubmitTransport {
    fn id(&self) -> TransportId {
        KEYSTROKE.into()
    }
    async fn observe(&self, _: &DeliveryTarget, _: Option<&SubmissionRef>) -> Result<TransportObservation, String> {
        Ok(TransportObservation { ready: true, ..Default::default() })
    }
    async fn submit(&self, delivery: &Delivery) -> TransportOutcome {
        self.submissions.lock().expect("submissions").push(delivery.text.clone());
        std::future::pending().await
    }
    async fn poll(&self, _: &Delivery) -> TransportOutcome {
        TransportOutcome::Pending
    }
}

// Behaviour: when the router fails over and the next transport's submission
// outlives the call bound, the batch counts as possibly delivered. It stays
// recorded against the planned transport, is held rather than retried, and
// is never resubmitted through either transport, even past the hold bound.
#[tokio::test(start_paused = true)]
async fn submit_timeout_after_failover_holds_without_resubmission() {
    let (backend, inbox) = delivery_inbox().await;
    let first = named("codex-app-server", TransportOutcome::NotSubmitted { reason: "app-server not live".into() });
    let hanging = Arc::new(HangingSubmitTransport { submissions: Default::default() });
    let router = TransportRouter::new()
        .with_connector("codex", Arc::new(PreferNamed(first.id)))
        .with_transport(hanging.clone())
        .with_transport(first.clone());
    inbox.reconcile_delivery(&router, at(20)).await.expect("submit");
    let status = backend.using::<Message>("flotilla").get("message-0").await.expect("message").status.expect("status");
    assert_eq!(status.submission.expect("held submission").transport.as_deref(), Some("codex-app-server"));
    assert!(status.resolved_receiver.is_none());
    for second in [21, 400] {
        inbox.reconcile_delivery(&router, at(second)).await.expect("held pass");
    }
    assert_eq!(first.submissions.lock().expect("submissions").len(), 1);
    assert_eq!(hanging.submissions.lock().expect("submissions").len(), 1, "never resubmitted");
    for message in backend.using::<Message>("flotilla").list().await.expect("messages").items {
        assert!(!message.status.expect("status").phase.has_delivery_evidence());
    }
}

// A permanently closed controller fence is definitely unsent, produces no
// receipt, and immediately raises the named operator gate without budget retries.
#[tokio::test]
async fn unverifiable_controller_fence_is_immediately_actionable_and_not_retried() {
    let (backend, inbox) = delivery_inbox().await;
    let reason = flotilla_resources::CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE;
    let transport = Arc::new(FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: TransportOutcome::SessionNeedsRestart { reason: reason.into() },
        accepted: Default::default(),
        working: Default::default(),
    });
    inbox.reconcile_delivery(&router(&transport), at(20)).await.expect("refusal");
    let gates = backend.using::<flotilla_resources::Demand>("flotilla").list().await.expect("gates").items;
    assert_eq!(gates.len(), 1, "operator sees failure on the first attempt");
    let description = &gates[0].metadata.annotations[flotilla_resources::delivery_hold::DELIVERY_HOLD_REASON_ANNOTATION];
    assert!(description.contains("ControllerFenceUnverifiable"));
    assert!(description.contains("exit the agent"));
    assert!(description.contains("flotilla convoy resume"));
    assert!(!description.contains("pending acceptance evidence"));
    assert!(!description.contains("attempt 3/3"));
    // Reconstruct the delivery loop, then advance beyond every retry/hold bound.
    let restarted = MessageInbox::new(backend.clone(), "flotilla");
    for second in [21, 80, 500, 3600] {
        restarted.reconcile_delivery(&router(&transport), at(second)).await.expect("observe parked failure");
    }
    assert_eq!(transport.submissions.lock().expect("submissions").len(), 1);
    for message in backend.using::<Message>("flotilla").list().await.expect("messages").items {
        let status = message.status.expect("status");
        assert!(status.resolved_receiver.is_none(), "no receipt for refused input");
        assert!(status.submission.is_none(), "known unsent input is not ambiguous");
        assert_eq!(status.reason.as_deref(), Some(reason));
        let retry = status.retry.expect("permanent failure");
        assert_eq!(retry.attempts, 1);
        assert!(matches!(retry.disposition, flotilla_resources::ControllerRetryDisposition::Terminal { .. }));
    }
}
