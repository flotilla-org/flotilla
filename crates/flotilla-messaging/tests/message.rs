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
    // A role with no live standing generation has no holder.
    assert!(resolve_message_receiver(&backend, "flotilla", "flotilla/coder").await.expect("role without generation").is_none());
    // The role's current generation: its newest live convoy ensured from the role's declaration.
    convoys
        .create(
            &InputMeta::builder()
                .name("next-convoy".into())
                .annotations(BTreeMap::from([(flotilla_resources::ENSURED_FROM_ANNOTATION.into(), "coder-holder".into())]))
                .build(),
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
    let holder = resolve_message_receiver(&backend, "flotilla", "flotilla/coder").await.expect("project role").expect("holder");
    assert_eq!(holder.object.metadata.name, "new-terminal");
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

// A relaunched vessel's earlier session stays listed, ended, beside its successor's (on b0, a
// launch that gave up, which reads Failed): the live one holds the role. With every launch ended, the newest launch's session does, so a sender learns
// why it ended rather than meeting two holders.
#[tokio::test]
async fn an_ended_launchs_session_yields_its_role_to_the_relaunch() {
    use std::collections::BTreeMap;

    use flotilla_messaging::resolve_message_receiver;
    use flotilla_resources::{
        Convoy, ConvoySpec, TerminalSession, TerminalSessionPhase, TerminalSessionStatus, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
        VESSEL_REF_LABEL,
    };
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder().name("convoy".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build(),
        )
        .await
        .unwrap();
    let sessions = backend.using::<TerminalSession>("flotilla");
    for (name, launch, phase) in [("aide-1", 1, TerminalSessionPhase::Failed), ("aide-2", 2, TerminalSessionPhase::Running)] {
        let mut intent = holder_spec("convoy");
        intent.role = "aide".into();
        let created = sessions
            .create(
                &InputMeta::builder()
                    .name(name.into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "convoy".into()),
                        (VESSEL_LABEL.into(), "assist".into()),
                        (ROLE_LABEL.into(), "aide".into()),
                        (VESSEL_REF_LABEL.into(), format!("convoy-assist#{launch}")),
                    ]))
                    .build(),
                &intent,
            )
            .await
            .unwrap();
        let status = TerminalSessionStatus { phase, ..Default::default() };
        sessions.update_status(name, &created.metadata.resource_version, &status).await.unwrap();
    }
    let holder = resolve_message_receiver(&backend, "flotilla", "flotilla/convoy/assist/aide").await.unwrap().expect("holder");
    assert_eq!(holder.object.metadata.name, "aide-2");

    let relaunched = sessions.get("aide-2").await.unwrap();
    let status = TerminalSessionStatus { phase: TerminalSessionPhase::Failed, ..Default::default() };
    sessions.update_status("aide-2", &relaunched.metadata.resource_version, &status).await.unwrap();
    let holder = resolve_message_receiver(&backend, "flotilla", "flotilla/convoy/assist/aide").await.unwrap().expect("holder");
    assert_eq!(holder.object.metadata.name, "aide-2");
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
    let ensure = format!("{project}-holder");
    // Each holder is the role's newest generation, and so its current one.
    let earlier = backend
        .using::<Convoy>("flotilla")
        .list()
        .await
        .expect("convoys")
        .items
        .iter()
        .filter(|convoy| convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure))
        .count();
    backend
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder()
                .name(name.clone())
                .annotations(std::collections::BTreeMap::from([(ENSURED_FROM_ANNOTATION.into(), ensure.clone())]))
                .build(),
            &ConvoySpec::builder()
                .workflow_ref("workflow".into())
                .project_ref(project.into())
                .role("guide".into())
                .generation(earlier as u64 + 1)
                .build(),
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
}

// A system address is already qualified: sender context leaves it as it is.
#[test]
fn system_addresses_are_distinct() {
    use flotilla_resources::{qualify_message_address, MessageAddressContext};
    let context = MessageAddressContext { project: "flotilla".into(), convoy: "convoy".into(), vessel: "work".into() };
    assert_eq!(qualify_message_address("system:checks", &context).expect("system address"), "system:checks");
}

// Supervision skips unoccupied parent levels, stops at the nearest subscribed
// holder, and excludes the sender. Every local/parent presence pair.
#[tokio::test]
async fn subscription_supervision_follows_live_parent_chain() {
    for (local, parent) in [(false, false), (false, true), (true, false), (true, true)] {
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
        let declarations = CascadeDeclarations::load(&backend, "flotilla").await.expect("declarations");
        let receivers = MessageReceiverSnapshot::load(&backend, "flotilla").await.expect("receivers");
        let path = supervision_path(&declarations, &receivers, "child", "child/task/work/coder").expect("path");
        let expected = if local {
            "child/guide"
        } else if parent {
            "parent/guide"
        } else {
            "root/guide"
        };
        assert_eq!(path[0].address, expected);
        let path = supervision_path(&declarations, &receivers, "child", "child/child-one/work/guide").expect("exclude sender");
        assert!(!path.iter().any(|contact| contact.address == "child/guide"));
        let before = memory.read_snapshot();
        let book = crew_address_book(&backend, "flotilla", "child/task/work/coder").await.expect("contacts");
        assert_eq!(book.supervision[0].address, expected);
        // Every presence combination shares one local-plus-replica snapshot,
        // even when supervision and peer contacts resolve several addresses.
        let calls = memory.read_snapshot().since(&before).calls_by_kind();
        for kind in ["Convoy", "TerminalSession"] {
            assert_eq!(calls.get(kind).copied().unwrap_or_default(), 2, "one snapshot for {kind}");
        }
        // The standing roles' holders come from the convoys alone.
        assert_eq!(calls.get("ConvoyEnsure").copied().unwrap_or_default(), 0, "no ConvoyEnsure reads");
        routing_holder(&backend, "root", "two").await;
        let next = crew_address_book(&backend, "flotilla", "child/task/work/coder").await.expect("refreshed contacts");
        assert_eq!(next.supervision.last().expect("root").terminal.as_deref(), Some("terminal-root-two"));
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
    use flotilla_resources::{Convoy, ConvoyEnsure, ConvoyEnsureSpec, ConvoySpec, TerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
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
            let ensure_name = if n == 0 { "ensure".into() } else { format!("ensure-{n}") };
            // Each convoy is its ensure's current generation.
            backend
                .using::<Convoy>("flotilla")
                .create(
                    &InputMeta::builder()
                        .name(name.clone())
                        .annotations(BTreeMap::from([(flotilla_resources::ENSURED_FROM_ANNOTATION.into(), ensure_name.clone())]))
                        .build(),
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
            backend
                .using::<ConvoyEnsure>("flotilla")
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
        for (index, address) in ["flotilla/convoy/work/coder", "flotilla/coder"].into_iter().enumerate() {
            let mut intent = spec(None, MessageExpectation::Reply);
            intent.receiver = address.into();
            backend
                .using::<Message>("flotilla")
                .create(&InputMeta::builder().name(format!("addressed-{index}")).build(), &intent)
                .await
                .expect("addressed message");
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
        // A standing role's holder is its newest live generation: no ConvoyEnsure is read.
        assert_eq!(kinds(&batch), [(1, 1), (1, 1), (0, 0), (0, 0)]);
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
