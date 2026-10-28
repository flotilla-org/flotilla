//! The Message import (#3317): each row of the phase table, named refusals,
//! a property over generated old records, and the stored-record corpus.

use std::{collections::BTreeMap, fs, path::Path};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_core::primitives::{
    convert::{Conversion, NoRecords},
    import::{
        admission_order, check, import_all, import_message, ImportDrop, ImportRefusal, Imported, ImportedMessage, OldMessage, ReadRecords,
    },
    roles::FleetRoles,
    view::ViewLookups,
    OldStoreRecords,
};
use flotilla_protocol::ResourceRef;
use flotilla_resources::{
    Message, MessageExpectation, MessagePhase, MessageReference, MessageRelation, MessageSpec, MessageStatus, ObjectMeta,
    ResolvedMessageReceiver, ResourceObject,
};
use flotilla_state_machines::machines::{
    consumer::{ConsumerKind, Receiver},
    message::{DropReason, MessageState},
};
use hegel::{generators as gs, generators::Generator, TestCase};

const HOME: &str = "alpha";
const RECEIVER: &str = "flotilla/demo/work/coder";
const INBOX: &str = "project:flotilla/convoy:demo/role:coder";

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("timestamp")
}

fn meta(name: &str, created: i64) -> ObjectMeta {
    ObjectMeta {
        name: name.into(),
        namespace: "flotilla".into(),
        resource_version: "1".into(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
        creation_timestamp: at(created),
        merge: None,
    }
}

fn spec(receiver: &str, sender: &str, expectation: MessageExpectation) -> MessageSpec {
    MessageSpec::builder()
        .sender(sender.into())
        .receiver(receiver.into())
        .relation(MessageRelation::System)
        .body("take a turn".into())
        .expectation(expectation)
        .build()
}

fn delivered() -> ResolvedMessageReceiver {
    ResolvedMessageReceiver {
        role_address: Some(RECEIVER.into()),
        crew_id: "crew-1".into(),
        session: "session-1".into(),
        delivered_at: at(20),
        evidence: "turn confirmed".into(),
        transport: None,
    }
}

fn status(phase: MessagePhase, sequence: Option<u64>, receiver: Option<ResolvedMessageReceiver>) -> MessageStatus {
    MessageStatus {
        phase,
        accepted_sequence: sequence,
        since: at(30),
        reason: phase.is_waiting().then(|| "waiting for delivery".into()),
        canonical_predecessor: None,
        resolved_receiver: receiver,
        retry: None,
        submission: None,
        failed_batch_members: Vec::new(),
        ..Default::default()
    }
}

fn record(name: &str, spec: MessageSpec, status: MessageStatus) -> ResourceObject<Message> {
    ResourceObject { metadata: meta(name, 1), spec, status: Some(status) }
}

fn sessions() -> BTreeMap<(String, String), (String, String)> {
    BTreeMap::from([(("crew-1".into(), "session-1".into()), ("terminal-demo-work-coder".into(), HOME.into()))])
}

fn session_receiver() -> Receiver {
    Receiver { kind: ConsumerKind::AgentSession, id: "terminal-demo-work-coder".into(), host: HOME.into() }
}

fn world(records: &[ResourceObject<Message>]) -> ReadRecords<'_> {
    ReadRecords {
        messages: records.iter().map(|record| (record.metadata.name.clone(), (record, HOME.to_owned()))).collect(),
        sessions: sessions(),
    }
}

/// Imports `records` and returns the result for the named record.
fn outcome(records: &[ResourceObject<Message>], name: &str) -> Result<Imported, ImportRefusal> {
    let roles = FleetRoles::declared("fleet").expect("roles");
    let conversion = Conversion { roles: &roles, forges: &[], records: &NoRecords };
    let world = world(records);
    let old = records.iter().find(|record| record.metadata.name == name).expect("named record");
    import_message(&OldMessage { record: old, home: HOME.into() }, 1, &conversion, &world)
}

fn import_named(records: &[ResourceObject<Message>], name: &str) -> Result<ImportedMessage, ImportRefusal> {
    match outcome(records, name)? {
        Imported::Message(message) => Ok(*message),
        Imported::Dropped(drop) => panic!("{name} dropped: {}", drop.reason),
    }
}

fn dropped(result: &Result<Imported, ImportRefusal>) -> Option<&'static str> {
    match result {
        Ok(Imported::Dropped(ImportDrop { reason, .. })) => Some(reason),
        _ => None,
    }
}

fn imported(records: &[ResourceObject<Message>], name: &str) -> ImportedMessage {
    let imported = import_named(records, name).unwrap_or_else(|refusal| panic!("{refusal}"));
    assert_eq!(imported.resource.name(), format!("{INBOX}/message:{name}"));
    assert_eq!(imported.home, HOME);
    assert_eq!(imported.record, name);
    check(&imported.resource).expect("the machine accepts the restored state");
    imported
}

fn leaf() -> flotilla_protocol::Leaf {
    "convoy/demo .status.phase == Landed".parse().expect("leaf")
}

// Behaviour: Accepted, WaitingOnReferences and Deliverable become Pending.
#[test]
fn waiting_phases_import_as_pending() {
    for phase in [MessagePhase::Accepted, MessagePhase::WaitingOnReferences, MessagePhase::Deliverable] {
        let records = [record("message-a", spec(RECEIVER, "system:turn-rules", MessageExpectation::None), status(phase, Some(1), None))];
        assert_eq!(*imported(&records, "message-a").resource.state(), MessageState::Pending, "{phase:?}");
    }
}

// Behaviour: Delivered becomes Awaiting its delivered session, named by its TerminalSession.
#[test]
fn delivered_imports_as_awaiting_its_session() {
    let records = [record(
        "message-a",
        spec(RECEIVER, "system:nudge", MessageExpectation::Reply),
        status(MessagePhase::Delivered, Some(1), Some(delivered())),
    )];
    assert_eq!(*imported(&records, "message-a").resource.state(), MessageState::Awaiting { receiver: session_receiver() });
}

// Behaviour: Satisfied, Answered and OutcomeMet become Settled, keeping their receiver.
#[test]
fn settled_phases_import_as_settled() {
    for (phase, expectation) in [
        (MessagePhase::Satisfied, MessageExpectation::None),
        (MessagePhase::Answered, MessageExpectation::Reply),
        (MessagePhase::OutcomeMet, MessageExpectation::Outcome { condition: leaf() }),
    ] {
        let records = [record("message-a", spec(RECEIVER, "system:turn-rules", expectation), status(phase, Some(1), Some(delivered())))];
        assert_eq!(
            *imported(&records, "message-a").resource.state(),
            MessageState::Settled { receiver: Some(session_receiver()) },
            "{phase:?}"
        );
    }
}

// Behaviour: Expired becomes Dropped(Expired) with no watermark the old store never recorded.
#[test]
fn expired_imports_as_dropped_expired() {
    let mut expiring = spec(RECEIVER, "system:turn-rules", MessageExpectation::Reply);
    expiring.deadline = Some(at(25));
    let records = [record("message-a", expiring, status(MessagePhase::Expired, Some(1), None))];
    assert_eq!(
        *imported(&records, "message-a").resource.state(),
        MessageState::Dropped { reason: DropReason::Expired, receiver: None, watermark: None }
    );
}

// Behaviour: Superseded becomes Dropped(Superseded) by the successor that names it.
#[test]
fn superseded_imports_as_dropped_by_its_successor() {
    let mut newer = spec(RECEIVER, "system:turn-rules", MessageExpectation::None);
    newer.supersedes = Some("message-old".into());
    let records = [
        record(
            "message-old",
            spec(RECEIVER, "system:turn-rules", MessageExpectation::None),
            status(MessagePhase::Superseded, Some(1), None),
        ),
        record("message-new", newer, status(MessagePhase::Deliverable, Some(2), None)),
    ];
    let by = format!("{INBOX}/message:message-new").parse().expect("successor address");
    assert_eq!(
        *imported(&records, "message-old").resource.state(),
        MessageState::Dropped { reason: DropReason::Superseded { by }, receiver: None, watermark: None }
    );
    let successor = imported(&records, "message-new");
    assert_eq!(
        successor.resource.spec().supersedes.as_ref().map(|reference| reference.name.as_str()),
        Some(&*format!("{INBOX}/message:message-old"))
    );
}

// Behaviour: DeadLettered becomes Dropped(Undeliverable).
#[test]
fn dead_lettered_imports_as_dropped_undeliverable() {
    let records = [record(
        "message-a",
        spec(RECEIVER, "system:turn-rules", MessageExpectation::None),
        status(MessagePhase::DeadLettered, Some(1), None),
    )];
    assert_eq!(
        *imported(&records, "message-a").resource.state(),
        MessageState::Dropped { reason: DropReason::Undeliverable, receiver: None, watermark: None }
    );
}

// Behaviour (#3938): a person at either end imports as the role they hold
// in the message's context: the operator role of the crew's project, or of
// the fleet when neither end has a project.
#[test]
fn person_records_import_as_the_operator_role_of_their_context() {
    let records = [
        record("message-to", spec(RECEIVER, "principal:implicit", MessageExpectation::None), status(MessagePhase::Accepted, Some(1), None)),
        record(
            "message-from",
            spec("principal:operator", RECEIVER, MessageExpectation::None),
            status(MessagePhase::Deliverable, Some(2), None),
        ),
        record(
            "message-fleet",
            spec("principal:alice", "system:stall-judge", MessageExpectation::None),
            status(MessagePhase::DeadLettered, Some(3), None),
        ),
    ];
    let to = import_named(&records, "message-to").unwrap_or_else(|refusal| panic!("{refusal}"));
    assert_eq!(to.resource.spec().sender.as_str(), "project:flotilla/role:operator");
    let from = import_named(&records, "message-from").unwrap_or_else(|refusal| panic!("{refusal}"));
    assert_eq!(from.resource.name(), "project:flotilla/role:operator/message:message-from");
    let fleet = import_named(&records, "message-fleet").unwrap_or_else(|refusal| panic!("{refusal}"));
    assert_eq!(fleet.resource.spec().inbox.as_str(), "fleet:fleet/role:operator");
    for imported in [to, from, fleet] {
        check(&imported.resource).expect("the machine accepts the restored state");
    }
}

// Behaviour: Delivered expecting nothing is settled at its delivered session.
#[test]
fn delivered_expecting_nothing_imports_as_settled() {
    let records = [record(
        "message-a",
        spec(RECEIVER, "system:turn-rules", MessageExpectation::None),
        status(MessagePhase::Delivered, Some(1), Some(delivered())),
    )];
    assert_eq!(*imported(&records, "message-a").resource.state(), MessageState::Settled { receiver: Some(session_receiver()) });
}

fn settled(record: &ResourceObject<Message>) -> bool {
    let phase = record.status.as_ref().expect("status").phase;
    !matches!(phase, MessagePhase::Accepted | MessagePhase::WaitingOnReferences | MessagePhase::Deliverable | MessagePhase::Delivered)
        || record.spec.expectation == MessageExpectation::None && phase == MessagePhase::Delivered
}

fn gone_session() -> ResolvedMessageReceiver {
    ResolvedMessageReceiver { session: "session-gone".into(), ..delivered() }
}

fn expectation(tc: &TestCase) -> MessageExpectation {
    match tc.draw(gs::integers::<u8>().max_value(2)) {
        0 => MessageExpectation::None,
        1 => MessageExpectation::Reply,
        _ => MessageExpectation::Outcome { condition: leaf() },
    }
}

// Behaviour: a record delivered to a session that is gone is dropped when
// settled and refused, by name, when it still awaits its receiver.
#[hegel::test(test_cases = 100)]
fn a_gone_session_drops_settled_records_and_refuses_open_ones(tc: TestCase) {
    let phase = tc.draw(
        gs::sampled_from(MessagePhase::ALL.iter().copied().filter(|phase| phase.has_delivery_evidence()).collect::<Vec<_>>())
            .print_as_debug(),
    );
    let records =
        [record("message-a", spec(RECEIVER, "system:turn-rules", expectation(&tc)), status(phase, Some(1), Some(gone_session())))];
    let result = outcome(&records, "message-a");
    if settled(&records[0]) {
        assert_eq!(dropped(&result), Some("its delivered session is gone"), "{phase:?}: {result:?}");
    } else {
        let refusal = result.expect_err("an awaiting record with a gone session is refused");
        assert_eq!(refusal.record, "message-a");
        assert!(refusal.reason.contains("session-gone"), "{refusal}");
    }
}

// Behaviour: a supersession whose successor is unrecorded or gone is dropped.
#[test]
fn superseded_records_without_their_successor_are_dropped() {
    let orphan = [record(
        "message-s",
        spec(RECEIVER, "system:turn-rules", MessageExpectation::None),
        status(MessagePhase::Superseded, Some(1), None),
    )];
    assert_eq!(dropped(&outcome(&orphan, "message-s")), Some("superseded with no recorded successor"));
    let mut named = status(MessagePhase::Superseded, Some(1), None);
    named.canonical_predecessor = Some(ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "message-gone"));
    let lost = [record("message-s", spec(RECEIVER, "system:turn-rules", MessageExpectation::None), named)];
    assert_eq!(dropped(&outcome(&lost, "message-s")), Some("its successor is gone"));
}

// Behaviour: an expiry the old store recorded with no deadline is dropped.
#[test]
fn expired_without_a_deadline_is_dropped() {
    let records =
        [record("message-a", spec(RECEIVER, "system:turn-rules", MessageExpectation::Reply), status(MessagePhase::Expired, Some(1), None))];
    assert_eq!(dropped(&outcome(&records, "message-a")), Some("expired without a deadline"));
}

// Behaviour: a reply to a message that was not read is dropped when settled
// and refused when open.
#[test]
fn a_gone_replied_to_message_drops_settled_records_and_refuses_open_ones() {
    let mut reply = spec(RECEIVER, "system:turn-rules", MessageExpectation::None);
    reply.in_reply_to = Some("message-gone".into());
    let records = [
        record("message-settled", reply.clone(), status(MessagePhase::Satisfied, Some(1), Some(delivered()))),
        record("message-open", reply, status(MessagePhase::Deliverable, Some(2), None)),
    ];
    assert_eq!(dropped(&outcome(&records, "message-settled")), Some("a message it names is gone"));
    let refusal = outcome(&records, "message-open").expect_err("an open reply to a gone message is refused");
    assert_eq!(refusal.record, "message-open");
    assert!(refusal.reason.contains("message-gone"), "{refusal}");
}

fn control_record(kind: &str, name: &str) -> MessageReference {
    MessageReference::ControlRecord { resource: ResourceRef::new("flotilla.work/v1", kind, "flotilla", name), revision: "7".into() }
}

// Behaviour: a Project reference maps to its project address and back; a
// TerminalSession reference has no address, so it drops a settled record and
// refuses an open one.
#[test]
fn project_references_map_and_terminal_session_references_drop_or_refuse() {
    let roles = FleetRoles::declared("fleet").expect("roles");
    let records = OldStoreRecords::from_records("flotilla", Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let conversion = Conversion { roles: &roles, forges: &[], records: &records };
    let referring = |name: &str, reference: MessageReference, phase: MessagePhase, receiver: Option<ResolvedMessageReceiver>| {
        let mut spec = spec(RECEIVER, "system:turn-rules", MessageExpectation::None);
        spec.references = vec![reference];
        record(name, spec, status(phase, Some(1), receiver))
    };
    let olds = [
        referring("to-project", control_record("Project", "flotilla"), MessagePhase::Satisfied, Some(delivered())),
        referring("settled-to-session", control_record("TerminalSession", "terminal-1"), MessagePhase::Satisfied, Some(delivered())),
        referring("open-to-session", control_record("TerminalSession", "terminal-1"), MessagePhase::Accepted, None),
    ];
    let world = world(&olds);
    let import = |name: &str| {
        let old = olds.iter().find(|record| record.metadata.name == name).expect("named");
        import_message(&OldMessage { record: old, home: HOME.into() }, 1, &conversion, &world)
    };
    let Ok(Imported::Message(project)) = import("to-project") else { panic!("the Project reference maps") };
    let reference = &project.resource.spec().references[0];
    assert_eq!(reference.address.as_str(), "project:flotilla");
    assert_eq!(records.record(&reference.address, &reference.revision), Some(control_record("Project", "flotilla")));
    assert_eq!(dropped(&import("settled-to-session")), Some("a record it references is gone or has no address"));
    let refusal = import("open-to-session").expect_err("an open record with an unaddressable reference is refused");
    assert_eq!(refusal.record, "open-to-session");
}

// Behaviour: a settled record naming a dropped message is dropped in turn;
// an open one naming it is refused.
#[test]
fn records_naming_a_dropped_message_drop_when_settled_and_refuse_when_open() {
    let mut reply = spec(RECEIVER, "system:turn-rules", MessageExpectation::None);
    reply.in_reply_to = Some("message-old".into());
    let records = [
        record(
            "message-old",
            spec(RECEIVER, "system:nudge", MessageExpectation::Reply),
            status(MessagePhase::Answered, Some(1), Some(gone_session())),
        ),
        record("message-settled", reply.clone(), status(MessagePhase::Satisfied, Some(2), Some(delivered()))),
        record("message-open", reply, status(MessagePhase::Accepted, Some(3), None)),
    ];
    let roles = FleetRoles::declared("fleet").expect("roles");
    let conversion = Conversion { roles: &roles, forges: &[], records: &NoRecords };
    let world = world(&records);
    let results = import_all(records.iter().map(|record| OldMessage { record, home: HOME.into() }), &conversion, &world);
    let named = |name: &str| {
        results
            .iter()
            .find(|result| match result {
                Ok(Imported::Message(message)) => message.record == name,
                Ok(Imported::Dropped(ImportDrop { record, .. })) | Err(ImportRefusal { record, .. }) => record == name,
            })
            .expect("named result")
    };
    assert_eq!(dropped(named("message-old")), Some("its delivered session is gone"));
    assert_eq!(dropped(named("message-settled")), Some("a message it names is gone"));
    assert!(named("message-open").is_err(), "{:?}", named("message-open"));
}

fn generated(tc: &TestCase, index: usize) -> ResourceObject<Message> {
    let receiver = tc.draw(gs::sampled_from(vec![
        RECEIVER,
        "flotilla/demo/review/reviewer",
        "flotilla/gui-approver",
        "principal:x",
        "topic:flotilla/reviews",
        "system:turn-rules",
    ]));
    let sender =
        tc.draw(gs::sampled_from(vec!["system:turn-rules", "system:nudge", "system:hold", "flotilla/demo/work/coder", "principal:op"]));
    let mut spec = spec(receiver, sender, expectation(tc));
    if tc.draw(gs::booleans()) {
        spec.deadline = Some(at(tc.draw(gs::integers::<i64>().min_value(1).max_value(100))));
    }
    if index > 0 && tc.draw(gs::booleans()) {
        spec.supersedes = Some(format!("message-{}", tc.draw(gs::integers::<usize>().max_value(index - 1))));
    }
    let phase = tc.draw(gs::sampled_from(MessagePhase::ALL.to_vec()).print_as_debug());
    let receiver = (phase.has_delivery_evidence() || tc.draw(gs::booleans())).then(|| {
        let mut receiver = delivered();
        if tc.draw(gs::integers::<u8>().max_value(4)) == 0 {
            receiver.session = "session-gone".into();
        }
        receiver
    });
    let sequence = tc.draw(gs::booleans()).then(|| tc.draw(gs::integers::<u64>().max_value(1000)));
    record(&format!("message-{index}"), spec, status(phase, sequence, receiver))
}

// Behaviour: every generated old record imports to a state the machine
// accepts, is dropped only when settled, or is refused with its own name;
// each home's admission order is 1..n in old acceptance order.
#[hegel::test(test_cases = 200)]
fn generated_records_import_or_are_refused_by_name(tc: TestCase) {
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let records: Vec<_> = (0..count).map(|index| generated(&tc, index)).collect();
    let roles = FleetRoles::declared("fleet").expect("roles");
    let conversion = Conversion { roles: &roles, forges: &[], records: &NoRecords };
    let world = world(&records);
    let olds = || records.iter().map(|record| OldMessage { record, home: HOME.into() });
    let results = import_all(olds(), &conversion, &world);
    assert_eq!(results.len(), records.len());
    for result in &results {
        match result {
            Ok(Imported::Dropped(drop)) => {
                let record = records.iter().find(|record| record.metadata.name == drop.record).expect("named");
                assert!(settled(record), "an open record was dropped: {drop:?}");
            }
            Ok(Imported::Message(imported)) => {
                check(&imported.resource).expect("an imported state is one the machine accepts");
                assert!(records.iter().any(|record| record.metadata.name == imported.record));
                assert!(imported.resource.name().ends_with(&format!("/message:{}", imported.record)));
            }
            Err(refusal) => {
                assert!(records.iter().any(|record| record.metadata.name == refusal.record), "{refusal}");
                // A waiting record needs no evidence: only an unmappable address refuses it.
                let record = records.iter().find(|record| record.metadata.name == refusal.record).expect("named");
                let mappable = |address: &str| !address.starts_with("topic:");
                let waiting = record.status.as_ref().is_some_and(|status| status.phase.is_waiting());
                assert!(
                    !(waiting && mappable(&record.spec.receiver) && mappable(&record.spec.sender) && record.spec.supersedes.is_none()),
                    "a mappable waiting record was refused: {refusal}"
                );
            }
        }
    }
    let order = admission_order(olds());
    let homed = &order[HOME];
    assert_eq!(homed.iter().map(|(version, _)| *version).collect::<Vec<_>>(), (1..=records.len() as u64).collect::<Vec<_>>());
    let accepted: Vec<_> = homed.iter().filter_map(|(_, old)| old.record.status.as_ref()?.accepted_sequence).collect();
    assert!(accepted.windows(2).all(|pair| pair[0] <= pair[1]), "accepted records keep their order");
    let first_unaccepted = homed.iter().position(|(_, old)| old.record.status.as_ref().is_some_and(|s| s.accepted_sequence.is_none()));
    if let Some(first) = first_unaccepted {
        assert!(homed[first..].iter().all(|(_, old)| old.record.status.as_ref().is_some_and(|s| s.accepted_sequence.is_none())));
    }
}

// Behaviour: every Message the stored-record corpus holds imports with no
// refusals (the current generation predates Message capture and holds none).
#[test]
fn stored_record_corpus_imports_with_no_refusals() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../flotilla-store-testkit/tests/fixtures/stored-records");
    let mut records = Vec::new();
    for generation in fs::read_dir(&root).expect("stored-record corpus") {
        let path = generation.expect("generation").path().join("Message.json");
        if !path.exists() {
            continue;
        }
        let documents: Vec<serde_json::Value> = serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("documents");
        for document in documents {
            let object = serde_json::json!({
                "metadata": document["metadata"],
                "spec": document["spec"],
                "status": document.get("status").cloned().unwrap_or_default(),
            });
            records.push(serde_json::from_value::<ResourceObject<Message>>(object).expect("stored Message decodes"));
        }
    }
    let roles = FleetRoles::declared("fleet").expect("roles");
    let conversion = Conversion { roles: &roles, forges: &[], records: &NoRecords };
    let world = ReadRecords {
        messages: records.iter().map(|record| (record.metadata.name.clone(), (record, HOME.to_owned()))).collect(),
        sessions: BTreeMap::new(),
    };
    let results = import_all(records.iter().map(|record| OldMessage { record, home: HOME.into() }), &conversion, &world);
    let refused: Vec<_> = results.iter().filter_map(|result| result.as_ref().err()).collect();
    assert!(refused.is_empty(), "corpus refusals: {refused:?}");
    assert_eq!(results.len(), records.len());
}

// A Message import rehearsal reads the replicated fleet identity and restores
// fleet-role senders and inboxes under fleet:, independently of the charter Project.
#[tokio::test]
async fn named_designation_imports_fleet_role_messages() {
    use flotilla_core::primitives::{import::rehearse, roles::designated_fleet_roles};
    use flotilla_resources::{FleetDesignation, FleetDesignationSpec, InputMeta, Project, ProjectSpec};
    use flotilla_store::ResourceBackend;
    use flotilla_store_memory::InMemoryBackend;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend
        .definitions::<Project>("flotilla")
        .apply(
            &InputMeta::builder().name("project-map".into()).build(),
            &ProjectSpec::builder().display_name("Charter".into()).default_workflow_ref("w".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("charter Project");
    backend
        .definitions::<FleetDesignation>("flotilla")
        .apply(
            &InputMeta::builder().name("fleet".into()).build(),
            &FleetDesignationSpec { fleet: Some("lab".into()), project: "project-map".into(), image_cache: None },
        )
        .await
        .expect("designation");
    let records = [
        record("to-crew", spec(RECEIVER, "system:nudge", MessageExpectation::None), status(MessagePhase::Accepted, Some(1), None)),
        record("to-fleet", spec("system:nudge", RECEIVER, MessageExpectation::None), status(MessagePhase::Accepted, Some(2), None)),
    ];
    let roles = designated_fleet_roles(&backend, "flotilla").await.expect("fleet roles");
    let conversion = Conversion { roles: &roles, forges: &[], records: &NoRecords };
    let world = world(&records);
    let imported = import_all(records.iter().map(|record| OldMessage { record, home: HOME.into() }), &conversion, &world)
        .into_iter()
        .map(|result| match result {
            Ok(Imported::Message(message)) => *message,
            other => panic!("not imported: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(imported.len(), 2);
    let to_crew = imported.iter().find(|item| item.record == "to-crew").expect("crew message");
    assert_eq!(to_crew.resource.spec().sender.as_str(), "fleet:lab/role:nudge");
    let to_fleet = imported.iter().find(|item| item.record == "to-fleet").expect("fleet message");
    assert_eq!(to_fleet.resource.spec().inbox.as_str(), "fleet:lab/role:nudge");
    assert_eq!(to_fleet.resource.name(), "fleet:lab/role:nudge/message:to-fleet");
    assert!(rehearse(&imported).is_empty(), "all imported records restore into the engine store");
}
