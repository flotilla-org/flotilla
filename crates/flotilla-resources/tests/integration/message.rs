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

// Suppression during recovery must keep returning its canonical predecessor,
// including after the predecessor's expectation closes.
#[tokio::test]
async fn replay_of_suppressed_partial_admission_returns_canonical_predecessor() {
    use flotilla_resources::{apply_status_patch, MessageAdmission, MessageInbox};
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let inbox = MessageInbox::new(backend.clone(), "flotilla");
    let messages = backend.using::<Message>("flotilla");
    let first = spec(None, MessageExpectation::Reply);
    inbox.accept(&InputMeta::builder().name("first".into()).build(), &first, at(10)).await.unwrap();
    let mut next = first.clone();
    next.supersedes = Some("first".into());
    messages.create(&InputMeta::builder().name("partial".into()).build(), &next).await.unwrap();
    apply_status_patch(&messages, "first", &MessageStatusPatch::Delivered {
        receiver: ResolvedMessageReceiver::builder()
            .crew_id("crew".into())
            .session("session".into())
            .delivered_at(at(20))
            .evidence("receipt".into())
            .build(),
        at: at(20),
    })
    .await
    .unwrap();
    for now in [21, 22] {
        let restarted = MessageInbox::new(backend.clone(), "flotilla");
        let admission = restarted.accept(&InputMeta::builder().name("partial".into()).build(), &next, at(now)).await.unwrap();
        assert!(matches!(admission, MessageAdmission::Suppressed { predecessor } if predecessor.metadata.name == "first"));
    }
    apply_status_patch(&messages, "first", &MessageStatusPatch::Finish {
        phase: MessagePhase::Answered,
        reason: "reply received".into(),
        at: at(23),
    })
    .await
    .unwrap();
    let admission = inbox.accept(&InputMeta::builder().name("partial".into()).build(), &next, at(24)).await.unwrap();
    assert!(matches!(admission, MessageAdmission::Suppressed { predecessor } if predecessor.metadata.name == "first"));
    assert_eq!(messages.get("partial").await.unwrap().status.unwrap().phase, MessagePhase::Superseded);
}

#[tokio::test]
async fn four_part_addresses_select_the_role_within_a_shared_vessel() {
    use std::collections::BTreeMap;

    use flotilla_resources::{resolve_message_receiver, Convoy, ConvoySpec, TerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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

// Boundary fake for terminal submission and externally observed acceptance.
struct FakeMessageTransport {
    submissions: std::sync::Mutex<Vec<String>>,
    observations: std::sync::atomic::AtomicUsize,
    outcome: flotilla_resources::MessageTransportOutcome,
    accepted: std::sync::atomic::AtomicBool,
    working: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl flotilla_resources::MessageTransport for FakeMessageTransport {
    async fn observe(
        &self,
        _: &flotilla_resources::ResourceObject<flotilla_resources::TerminalSession>,
        _: Option<&flotilla_resources::MessageSubmission>,
    ) -> Result<flotilla_resources::MessageObservation, String> {
        self.observations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(flotilla_resources::MessageObservation {
            ready: true,
            working: self.working.load(std::sync::atomic::Ordering::SeqCst),
            evidence: self.accepted.load(std::sync::atomic::Ordering::SeqCst).then(|| "later hook receipt".into()),
            ..Default::default()
        })
    }
    async fn submit(&self, batch: &flotilla_resources::MessageBatch) -> flotilla_resources::MessageTransportOutcome {
        self.submissions.lock().expect("submissions").push(batch.text.clone());
        self.outcome.clone()
    }
    async fn poll(&self, _: &flotilla_resources::MessageBatch) -> flotilla_resources::MessageTransportOutcome {
        self.outcome.clone()
    }
}
async fn delivery_inbox() -> (ResourceBackend, flotilla_resources::MessageInbox) {
    use flotilla_resources::*;
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
        .update_status("terminal", &holder.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some("session".into()),
            crew: Some(CrewSessionStatus { id: "crew".into(), adapter: "codex".into(), model: None, stance: "work".into() }),
            ..Default::default()
        })
        .await
        .expect("running");
    let inbox = MessageInbox::new(backend.clone(), "flotilla");
    for index in 0..3 {
        let mut intent = spec(None, MessageExpectation::None);
        intent.body = format!("body-{index}");
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
    let transport = FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: flotilla_resources::MessageTransportOutcome::Unconfirmed { reason: "acceptance not yet observed".into() },
        accepted: Default::default(),
        working: Default::default(),
    };
    inbox.reconcile_delivery(&transport, at(20)).await.expect("submit");
    // Reconstruct the inbox as on restart: the durable submission still forbids retyping.
    let restarted = flotilla_resources::MessageInbox::new(backend.clone(), "flotilla");
    restarted.reconcile_delivery(&transport, at(21)).await.expect("held observation");
    assert_eq!(transport.submissions.lock().expect("submissions").len(), 1);
    let text = transport.submissions.lock().expect("submissions")[0].clone();
    assert!(text.find("body-0").expect("find expected body in batch") < text.find("body-1").expect("find expected body in batch"));
    assert!(text.find("body-1").expect("find expected body in batch") < text.find("body-2").expect("find expected body in batch"));
    assert_eq!(backend.using::<flotilla_resources::Demand>("flotilla").list().await.expect("demands").items.len(), 1);
    transport.accepted.store(true, Ordering::SeqCst);
    restarted.reconcile_delivery(&transport, at(22)).await.expect("later evidence");
    assert_eq!(transport.observations.load(Ordering::SeqCst), 3);
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
    let transport = FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: flotilla_resources::MessageTransportOutcome::NotSubmitted { reason: "pool unavailable before typing".into() },
        accepted: Default::default(),
        working: Default::default(),
    };
    for second in [20, 21, 25, 26, 35, 100] {
        inbox.reconcile_delivery(&transport, at(second)).await.expect("reconcile");
    }
    assert_eq!(transport.submissions.lock().expect("submissions").len(), 3);
    assert_eq!(transport.observations.load(std::sync::atomic::Ordering::SeqCst), 6);
    for message in backend.using::<Message>("flotilla").list().await.expect("messages").items {
        let retry = message.status.expect("status").retry.expect("retry");
        assert_eq!(retry.attempts, 3);
        assert!(matches!(retry.disposition, flotilla_resources::ControllerRetryDisposition::Terminal { .. }));
    }
    inbox
        .accept(&InputMeta::builder().name("later".into()).build(), &spec(None, MessageExpectation::None), at(101))
        .await
        .expect("admit Message fixture");
    inbox.reconcile_delivery(&transport, at(102)).await.expect("reconcile receiver delivery");
    assert_eq!(
        transport.submissions.lock().expect("lock captured transport evidence").len(),
        3,
        "later intent cannot bypass exhausted FIFO head"
    );
    let later = backend.using::<Message>("flotilla").get("later").await.expect("read Message fixture");
    assert!(later.status.expect("Message fixture has durable status").submission.is_none());
    assert_eq!(transport.observations.load(std::sync::atomic::Ordering::SeqCst), 7, "held attention refreshes");
}

// Delivery leaves reply/outcome expectations open. A correlated durable reply
// or a true admitted Leaf closes them, while unknown outcome evidence waits.
#[tokio::test]
async fn delivered_expectations_settle_from_reply_and_outcome_evidence() {
    use flotilla_resources::{Convoy, ConvoyPhase, ConvoyStatus, MessageTransportOutcome};
    let (backend, inbox) = delivery_inbox().await;
    for (name, expectation) in [
        ("reply-request", MessageExpectation::Reply),
        ("outcome-request", MessageExpectation::Outcome { condition: "convoy/convoy .status.phase == Active".parse().expect("condition") }),
    ] {
        inbox.accept(&InputMeta::builder().name(name.into()).build(), &spec(None, expectation), at(11)).await.expect("expectation");
    }
    let transport = FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: MessageTransportOutcome::Accepted { evidence: "accepted by holder".into() },
        accepted: Default::default(),
        working: Default::default(),
    };
    inbox.reconcile_delivery(&transport, at(20)).await.expect("deliver");
    inbox.reconcile_delivery(&transport, at(21)).await.expect("unknown outcome");
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
    inbox.reconcile_delivery(&transport, at(23)).await.expect("settle");
    assert_eq!(messages.get("outcome-request").await.expect("outcome").status.expect("status").phase, MessagePhase::OutcomeMet);
    assert_eq!(messages.get("reply-request").await.expect("reply").status.expect("status").phase, MessagePhase::Answered);
}

// A single Working observation can be a redraw flicker. Only sustained
// post-submission Working evidence confirms a pending batch.
#[tokio::test]
async fn working_flicker_does_not_establish_delivery() {
    use std::sync::atomic::Ordering;
    let (backend, inbox) = delivery_inbox().await;
    let transport = FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: flotilla_resources::MessageTransportOutcome::Pending,
        accepted: Default::default(),
        working: Default::default(),
    };
    inbox.reconcile_delivery(&transport, at(20)).await.expect("submit");
    for (second, working) in [(21, true), (22, false), (23, true)] {
        transport.working.store(working, Ordering::SeqCst);
        inbox.reconcile_delivery(&transport, at(second)).await.expect("observe flicker");
        assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.iter().all(|message| message
            .status
            .as_ref()
            .expect("status")
            .phase
            == MessagePhase::Deliverable));
    }
    inbox.reconcile_delivery(&transport, at(24)).await.expect("stable working");
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

    use flotilla_resources::{MessageAdmission, MessageTransportOutcome};
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
        let transport = FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: MessageTransportOutcome::Unconfirmed { reason: "possible submission".into() },
            accepted: Default::default(),
            working: Default::default(),
        };
        inbox.reconcile_delivery(&transport, at(20)).await.expect("possible input");
        let mut successor = original.clone();
        successor.body = "newer body".into();
        assert!(matches!(
            inbox
                .accept(&InputMeta::builder().name("successor".into()).build(), &successor, at(21))
                .await
                .expect("successor intent retained"),
            MessageAdmission::Accepted(_)
        ));
        inbox.reconcile_delivery(&transport, at(21)).await.expect("held pass");
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
        inbox.reconcile_delivery(&transport, at(22)).await.expect("late receipt");
        inbox.reconcile_delivery(&transport, at(23)).await.expect("successor disposition");
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
    use flotilla_resources::{MessageInbox, MessageTransportOutcome};
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
    let transport = FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: MessageTransportOutcome::Accepted { evidence: "receipt".into() },
        accepted: Default::default(),
        working: Default::default(),
    };
    restarted.reconcile_delivery(&transport, at(20)).await.expect("reconcile receiver delivery");
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
        let transport = FakeMessageTransport {
            submissions: Default::default(),
            observations: Default::default(),
            outcome: MessageTransportOutcome::Unconfirmed { reason: "possible input".into() },
            accepted: Default::default(),
            working: Default::default(),
        };
        let mut intent = spec(None, expectation);
        intent.deadline = Some(at(25));
        inbox.accept(&InputMeta::builder().name(name.into()).build(), &intent, at(10)).await.expect("admit Message fixture");
        inbox.reconcile_delivery(&transport, at(20)).await.expect("reconcile receiver delivery");
        if name != "uncertain" {
            apply_status_patch(&messages, name, &MessageStatusPatch::Delivered {
                receiver: ResolvedMessageReceiver::builder()
                    .crew_id("crew".into())
                    .session("session".into())
                    .delivered_at(at(21))
                    .evidence("later transport receipt".into())
                    .build(),
                at: at(21),
            })
            .await
            .expect("apply fixture status transition");
        }
        inbox.reconcile_delivery(&transport, at(30)).await.expect("reconcile receiver delivery");
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
    let transport = FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: flotilla_resources::MessageTransportOutcome::Unconfirmed { reason: "uncertain input".into() },
        accepted: Default::default(),
        working: Default::default(),
    };
    inbox.reconcile_delivery(&transport, at(20)).await.expect("reconcile receiver delivery");
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
impl flotilla_resources::MessageTransport for HangingMessageTransport {
    async fn observe(
        &self,
        holder: &flotilla_resources::ResourceObject<flotilla_resources::TerminalSession>,
        submission: Option<&flotilla_resources::MessageSubmission>,
    ) -> Result<flotilla_resources::MessageObservation, String> {
        if self.stage == "observe" {
            return std::future::pending().await;
        }
        flotilla_resources::MessageTransport::observe(&self.inner, holder, submission).await
    }
    async fn submit(&self, batch: &flotilla_resources::MessageBatch) -> flotilla_resources::MessageTransportOutcome {
        let outcome = flotilla_resources::MessageTransport::submit(&self.inner, batch).await;
        if self.stage == "submit" {
            return std::future::pending().await;
        }
        outcome
    }
    async fn poll(&self, batch: &flotilla_resources::MessageBatch) -> flotilla_resources::MessageTransportOutcome {
        if self.stage == "poll" {
            return std::future::pending().await;
        }
        flotilla_resources::MessageTransport::poll(&self.inner, batch).await
    }
    async fn release(&self, _: &flotilla_resources::MessageBatch) {
        if self.stage == "release" {
            std::future::pending::<()>().await;
        }
    }
}
#[tokio::test(start_paused = true)]
async fn transport_timeouts_release_admission_without_retyping_uncertain_input() {
    use flotilla_resources::MessageTransportOutcome;
    for stage in ["observe", "submit", "poll", "release"] {
        let (backend, inbox) = delivery_inbox().await;
        let transport = HangingMessageTransport {
            stage,
            inner: FakeMessageTransport {
                submissions: Default::default(),
                observations: Default::default(),
                outcome: if stage == "release" {
                    MessageTransportOutcome::Accepted { evidence: "receipt".into() }
                } else {
                    MessageTransportOutcome::Pending
                },
                accepted: Default::default(),
                working: Default::default(),
            },
        };
        let started = tokio::time::Instant::now();
        inbox.reconcile_delivery(&transport, at(20)).await.expect("reconcile receiver delivery");
        if stage == "poll" {
            inbox.reconcile_delivery(&transport, at(21)).await.expect("reconcile receiver delivery");
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
    let (backend, inbox) = delivery_inbox().await;
    let transport = FakeMessageTransport {
        submissions: Default::default(),
        observations: Default::default(),
        outcome: MessageTransportOutcome::Pending,
        accepted: Default::default(),
        working: Default::default(),
    };
    inbox.reconcile_delivery(&transport, at(20)).await.expect("reconcile receiver delivery");
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
    apply_status_patch(&messages, "message-0", &MessageStatusPatch::Finish {
        phase: MessagePhase::Satisfied,
        reason: "notification accepted".into(),
        at: at(21),
    })
    .await
    .expect("apply fixture status transition");
    let settled = messages.get("message-0").await.expect("read Message fixture");
    MessageInbox::new(backend.clone(), "flotilla").reconcile_delivery(&transport, at(22)).await.expect("reconcile receiver delivery");
    assert_eq!(messages.get("message-0").await.expect("read Message fixture").status, settled.status, "terminal receipt is immutable");
    for name in ["message-1", "message-2"] {
        let status = messages.get(name).await.expect("read Message fixture").status.expect("Message fixture has durable status");
        assert_eq!(status.phase, MessagePhase::Delivered);
        assert_eq!(status.resolved_receiver, Some(receipt.clone()));
    }
    assert_eq!(transport.submissions.lock().expect("lock captured transport evidence").len(), 1);
}
