use chrono::{TimeZone, Utc};
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
        // #3433: receiver reads hydrate only addressed receiver rows, even with
        // unrelated holders and active messages sharing the inbox.
        let inbox = flotilla_messaging::MessageInbox::new(backend.clone(), "flotilla");
        for (index, address) in ["flotilla/convoy/work/coder", "flotilla/coder"].into_iter().enumerate() {
            let mut intent = spec(None, MessageExpectation::Reply);
            intent.receiver = address.into();
            inbox.accept(&InputMeta::builder().name(format!("addressed-{index}")).build(), &intent, at(10)).await.expect("admit");
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
