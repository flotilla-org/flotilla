//! Producers send as their fleet roles on the engine path, and a reply to a
//! role reaches that role's inbox (#3317).

use std::sync::Arc;

use flotilla_core::primitives::MessagePrimitives;
use flotilla_inbox::InboxBackend;
use flotilla_resources::{
    FleetDesignation, FleetDesignationSpec, InputMeta, MessageReference, MessageRelation, MessageSpec, Project, ProjectSpec, SystemClock,
    FLEET_DESIGNATION_NAME,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::machines::message::{Message, MessageState};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const CREW: &str = "flotilla/demo/work/coder";

async fn primitives(dir: &tempfile::TempDir) -> MessagePrimitives {
    primitives_at(dir, Arc::new(SystemClock)).await
}

async fn primitives_at(dir: &tempfile::TempDir, clock: Arc<dyn flotilla_resources::Clock>) -> MessagePrimitives {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend
        .clone()
        .using::<Project>(NAMESPACE)
        .create(
            &InputMeta::builder().name("fleet".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("Fleet".to_string())
                .default_workflow_ref("w".to_string())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("fleet project");
    backend
        .definitions::<FleetDesignation>(NAMESPACE)
        .create(
            &InputMeta::builder().name(FLEET_DESIGNATION_NAME.to_string()).build(),
            &FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
        )
        .await
        .expect("designation");
    let service = Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), "home-a", flotilla_engine_store::registry().expect("registry")).expect("store"),
    );
    MessagePrimitives::open("home-a", service, backend, clock).expect("primitives")
}

fn intent(sender: &str, receiver: &str) -> MessageSpec {
    MessageSpec::builder()
        .sender(sender.into())
        .receiver(receiver.into())
        .relation(MessageRelation::System)
        .body(format!("from {sender}"))
        .build()
}

fn engine_message(primitives: &MessagePrimitives, record: &str) -> flotilla_state_machines::Resource<Message> {
    let (id, _) = primitives.locate(record).expect("locate").expect("admitted");
    primitives.messages().expect("messages").into_iter().map(|(_, message)| message).find(|message| message.name() == id).expect("stored")
}

// Behaviour: each `system:` producer sends as its declared fleet role under
// the fleet name, and a held turn sends as turn-rules; `system:hold`
// survives only in its de-duplication key (the record name).
#[tokio::test]
async fn system_producers_send_as_their_fleet_roles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    for role in ["turn-rules", "nudge", "stall-judge", "capabilities", "fleet-store"] {
        let sender = format!("system:{role}");
        let record = flotilla_resources::message_record_name(CREW, &sender, role);
        primitives.admit(NAMESPACE, &record, &intent(&sender, CREW)).await.expect("admitted");
        let message = engine_message(&primitives, &record);
        assert_eq!(message.spec().sender.as_str(), format!("fleet:lab/role:{role}"));
        assert_eq!(message.spec().inbox.as_str(), "project:flotilla/convoy:demo/role:coder");
        assert_eq!(message.name(), format!("project:flotilla/convoy:demo/role:coder/message:{record}"));
    }
    // The held turn's name keys on `system:hold`; it sends as turn-rules.
    let held = flotilla_resources::message_record_name("system:hold", "system:turn-rules", "hold:demo:work:coder:rule:1");
    primitives.admit(NAMESPACE, &held, &intent("system:turn-rules", CREW)).await.expect("held turn");
    assert_eq!(engine_message(&primitives, &held).spec().sender.as_str(), "fleet:lab/role:turn-rules");
    // A repeated key is the same message: nothing more is admitted.
    let count = primitives.messages().expect("messages").len();
    primitives.admit(NAMESPACE, &held, &intent("system:turn-rules", CREW)).await.expect("repeat");
    assert_eq!(primitives.messages().expect("messages").len(), count);
}

// Behaviour: a crew's reply to a fleet role's message lands in that role's
// own inbox, naming the original; nobody is bound there yet, so it waits.
#[tokio::test]
async fn a_reply_to_a_fleet_role_reaches_its_inbox() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let original = flotilla_resources::message_record_name(CREW, "system:nudge", "stall-nudge-1");
    let mut nudge = intent("system:nudge", CREW);
    nudge.references =
        vec![MessageReference::Issue { service: "github".into(), scope: "flotilla-org/flotilla".into(), number: 7, revision: "1".into() }];
    primitives.admit(NAMESPACE, &original, &nudge).await.expect("nudge");
    let reply_name = flotilla_resources::message_record_name("system:nudge", CREW, "reply-1");
    let mut reply = intent(CREW, "system:nudge");
    reply.relation = MessageRelation::Peer;
    reply.in_reply_to = Some(original.clone());
    primitives.admit(NAMESPACE, &reply_name, &reply).await.expect("reply");
    let stored = engine_message(&primitives, &reply_name);
    assert_eq!(stored.spec().inbox.as_str(), "fleet:lab/role:nudge");
    assert_eq!(stored.spec().sender.as_str(), "project:flotilla/convoy:demo/role:coder");
    let (original_id, home) = primitives.locate(&original).expect("locate").expect("original");
    let in_reply_to = stored.spec().in_reply_to.as_ref().expect("names the original");
    assert_eq!((in_reply_to.name.as_str(), in_reply_to.home.as_str()), (original_id.as_str(), home.as_str()));
    assert_eq!(*stored.state(), MessageState::Pending);
    let inbox: flotilla_address::Address = "fleet:lab/role:nudge".parse().expect("inbox");
    let consumer = primitives.register("fleet-nudge-handler", flotilla_schema::Timestamp(1)).await.expect("consumer");
    primitives.inbox().bind(inbox.clone(), consumer.clone()).await.expect("bind a handler later");
    let pending: Vec<_> = futures::TryStreamExt::try_collect(
        primitives.inbox().pending(&inbox, &consumer, &flotilla_inbox::Rules::default()).await.expect("pending"),
    )
    .await
    .expect("items");
    assert_eq!(pending.iter().map(|item| item.message.as_str()).collect::<Vec<_>>(), [stored.name()]);
}

// Behaviour (#3317): a turn whose receipt committed but whose deliver was
// refused or interrupted is no longer pending for its consumer, so it is never
// retyped, and the sweep records its deliver. A reply settles its original
// through the sweep's incremental log index, which holds only open messages.
#[tokio::test]
async fn the_sweep_reconciles_receipts_and_settles_replies_from_open_messages_only() {
    use flotilla_core::primitives::sweep::OpenMessages;
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let mut turn = intent("system:turn-rules", CREW);
    turn.expectation = flotilla_resources::MessageExpectation::Reply;
    let record = flotilla_resources::message_record_name(CREW, "system:turn-rules", "rule@1");
    primitives.admit(NAMESPACE, &record, &turn).await.expect("turn");
    let message = engine_message(&primitives, &record);
    let consumer = primitives.register("crew-session", flotilla_schema::Timestamp(1)).await.expect("consumer");
    let inbox = primitives.inbox();
    inbox.bind(message.spec().inbox.clone(), consumer.clone()).await.expect("bind");
    // The turn was confirmed and its receipt written; its deliver never was.
    inbox.receive(&consumer, message.name(), flotilla_state_machines::Transport::Turn).await.expect("receipt");
    let pending: Vec<_> = futures::TryStreamExt::try_collect(
        inbox.pending(&message.spec().inbox, &consumer, &flotilla_inbox::Rules::default()).await.expect("pending"),
    )
    .await
    .expect("items");
    assert!(pending.is_empty(), "a received message is not offered again");
    let mut open = OpenMessages::default();
    let now = flotilla_schema::Timestamp::from_datetime(&chrono::Utc::now());
    let report = primitives.sweep(NAMESPACE, now, &mut open).await.expect("sweep");
    assert_eq!(report.reconciled, 1);
    assert!(matches!(engine_message(&primitives, &record).state(), MessageState::Awaiting { .. }));
    // The crew answers; the sweep settles the original from the log since its last pass.
    let mut answer = intent(CREW, "system:turn-rules");
    answer.relation = MessageRelation::Peer;
    answer.in_reply_to = Some(record.clone());
    let answer_name = flotilla_resources::message_record_name("system:turn-rules", CREW, "reply@1");
    primitives.admit(NAMESPACE, &answer_name, &answer).await.expect("answer");
    let report = primitives.sweep(NAMESPACE, now, &mut open).await.expect("sweep");
    assert_eq!(report.answered, 1);
    assert!(matches!(engine_message(&primitives, &record).state(), MessageState::Settled { .. }));
    let (answer_id, _) = primitives.locate(&answer_name).expect("locate").expect("answer");
    assert_eq!(open.open().collect::<Vec<_>>(), [answer_id.as_str()], "only open messages are swept again");
}

// Behaviour (#3317): an index that meets a home for the first time (a
// restart), or whose cursor is below the home's resume floor, seeds its
// open messages from one scan of that home rather than its log, so an open
// message last written long ago is still swept.
#[tokio::test]
async fn a_fresh_sweep_index_seeds_open_messages_from_a_scan() {
    use flotilla_core::primitives::sweep::OpenMessages;
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let mut names = Vec::new();
    for key in ["early@1", "early@2"] {
        let record = flotilla_resources::message_record_name(CREW, "system:nudge", key);
        primitives.admit(NAMESPACE, &record, &intent("system:nudge", CREW)).await.expect("admit");
        names.push(primitives.locate(&record).expect("locate").expect("admitted").0);
    }
    let mut restarted = OpenMessages::default();
    restarted.refresh(&primitives).expect("seed");
    let mut open: Vec<_> = restarted.open().map(str::to_owned).collect();
    open.sort();
    names.sort();
    assert_eq!(open, names);
    // Later writes are read from the log, from the seeded cursor on.
    let later = flotilla_resources::message_record_name(CREW, "system:nudge", "later@1");
    primitives.admit(NAMESPACE, &later, &intent("system:nudge", CREW)).await.expect("admit");
    restarted.refresh(&primitives).expect("tail");
    assert_eq!(restarted.open().count(), 3);
}

// Behaviour (#3587 layer 7): a crew's stall to its supervisor records the sender's half at this
// home, about the crew's seat; the seat is stalled until the supervisor's reply lands in the crew's
// own inbox, which answers the record. A record written after its reply landed is answered at once.
#[tokio::test]
async fn a_stall_is_open_until_the_supervisors_reply_lands() {
    use flotilla_state_machines::machines::{awaiting_reply::AwaitingReplyLabel, message::MessageRef};
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let seat: flotilla_address::Address = "project:flotilla/convoy:demo/role:coder".parse().expect("seat");
    let supervisor = "fleet/governor";
    let send = |id: &str| {
        let mut stall = intent(CREW, supervisor);
        stall.relation = MessageRelation::Supervisor;
        stall.expectation = flotilla_resources::MessageExpectation::Reply;
        (flotilla_resources::message_record_name(supervisor, CREW, id), stall)
    };
    let (name, stall) = send("stall-1");
    primitives.admit(NAMESPACE, &name, &stall).await.expect("stall sent");
    assert!(primitives.await_reply(NAMESPACE, &name, &stall, "home-a", "stall", Some(CREW)).await.expect("recorded"));
    assert!(primitives.awaiting().stalled(&seat).expect("read"), "the seat is stalled");
    let awaited = MessageRef::new(format!("project:fleet/role:governor/message:{name}"), "home-a");
    assert_eq!(primitives.awaiting().get(&awaited).expect("record").spec().recipient.as_str(), "project:fleet/role:governor");

    let resume = |id: &str, original: &str| {
        let mut reply = intent("fleet/governor", CREW);
        reply.relation = MessageRelation::Supervisor;
        reply.in_reply_to = Some(original.to_owned());
        (flotilla_resources::message_record_name(CREW, supervisor, id), reply)
    };
    let (reply_name, reply) = resume("resume-1", &name);
    primitives.admit(NAMESPACE, &reply_name, &reply).await.expect("reply");
    assert_eq!(primitives.awaiting().get(&awaited).expect("record").label(), AwaitingReplyLabel::Answered);
    assert!(!primitives.awaiting().stalled(&seat).expect("read"), "answered");

    // The reply first, then the record: answered as it is written.
    let (late, stall) = send("stall-2");
    primitives.admit(NAMESPACE, &late, &stall).await.expect("stall sent");
    let (reply_name, reply) = resume("resume-2", &late);
    primitives.admit(NAMESPACE, &reply_name, &reply).await.expect("reply");
    // A send may have persisted its record before a later reconciliation read failed.
    let mut partial = primitives.awaiting().get(&awaited).expect("earlier record").spec().clone();
    partial.message = MessageRef::new(format!("project:fleet/role:governor/message:{late}"), "home-a");
    primitives.awaiting().record(partial.clone(), flotilla_schema::Timestamp(1)).expect("record persisted before retry");
    assert_eq!(primitives.awaiting().get(&partial.message).expect("partial record").label(), AwaitingReplyLabel::Open);
    assert!(!primitives.await_reply(NAMESPACE, &late, &stall, "home-a", "stall", Some(CREW)).await.expect("retry reconciles reply"));
    let awaited = MessageRef::new(format!("project:fleet/role:governor/message:{late}"), "home-a");
    assert_eq!(primitives.awaiting().get(&awaited).expect("record").label(), AwaitingReplyLabel::Answered);
    // A send expecting nothing records nothing.
    let (plain, mut note) = send("note");
    note.expectation = flotilla_resources::MessageExpectation::None;
    assert!(!primitives.await_reply(NAMESPACE, &plain, &note, "home-a", "reply", None).await.expect("nothing"));
}

// Behaviour (#3730 review): a message superseded at this home withdraws its sender's record here,
// and the ended record is evicted from memory yet still read from the store. A successor that
// awaits a reply withdraws it only once its own record is written, so the seat stays stalled.
#[tokio::test]
async fn a_superseded_send_withdraws_its_awaiting_record() {
    use flotilla_state_machines::machines::{awaiting_reply::AwaitingReplyLabel, message::MessageRef};
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let seat: flotilla_address::Address = "project:flotilla/convoy:demo/role:coder".parse().expect("seat");
    let supervisor = "fleet/governor";
    let mut stall = intent(CREW, supervisor);
    stall.relation = MessageRelation::Supervisor;
    stall.expectation = flotilla_resources::MessageExpectation::Reply;
    let first = flotilla_resources::message_record_name(supervisor, CREW, "stall-1");
    primitives.admit(NAMESPACE, &first, &stall).await.expect("first");
    primitives.await_reply(NAMESPACE, &first, &stall, "home-a", "stall", Some(CREW)).await.expect("recorded");
    let mut again = stall.clone();
    again.supersedes = Some(first.clone());
    let second = flotilla_resources::message_record_name(supervisor, CREW, "stall-2");
    primitives.admit(NAMESPACE, &second, &again).await.expect("second, superseding the first");
    let awaited = MessageRef::new(format!("project:fleet/role:governor/message:{first}"), "home-a");
    assert_eq!(primitives.awaiting().get(&awaited).expect("open").label(), AwaitingReplyLabel::Open, "kept until the successor's record");
    assert!(primitives.awaiting().stalled(&seat).expect("read"), "still stalled between admission and record");
    primitives.await_reply(NAMESPACE, &second, &again, "home-a", "stall", Some(CREW)).await.expect("the successor's record");
    assert_eq!(primitives.awaiting().get(&awaited).expect("read from the store").label(), AwaitingReplyLabel::Withdrawn);
    assert_eq!(primitives.awaiting().open_about(&seat).expect("read").len(), 1, "only the successor's stall is open");

    // A successor that awaits nothing withdraws the predecessor's record as it is admitted.
    let mut note = stall.clone();
    note.expectation = flotilla_resources::MessageExpectation::None;
    note.supersedes = Some(second.clone());
    let third = flotilla_resources::message_record_name(supervisor, CREW, "note");
    primitives.admit(NAMESPACE, &third, &note).await.expect("a note superseding the second");
    assert!(!primitives.awaiting().stalled(&seat).expect("read"), "the withdrawn stall is not open");
}

// Behaviour (#3730 review, a judge or a supervisor escalating a rung): a newer stall about a seat,
// whoever sends it, replaces the seat's older stall records once it is written, without a moment
// in which the seat reads as not stalled.
#[tokio::test]
async fn a_newer_stall_about_the_seat_replaces_the_older_once_written() {
    use flotilla_state_machines::machines::{awaiting_reply::AwaitingReplyLabel, message::MessageRef};
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let seat: flotilla_address::Address = "project:flotilla/convoy:demo/role:coder".parse().expect("seat");
    let stall = |sender: &str, receiver: &str, id: &str| {
        let mut stall = intent(sender, receiver);
        stall.relation = MessageRelation::Supervisor;
        stall.expectation = flotilla_resources::MessageExpectation::Reply;
        (flotilla_resources::message_record_name(receiver, sender, id), stall)
    };
    let record = |receiver: &str, name: &str| MessageRef::new(format!("project:fleet/role:{receiver}/message:{name}"), "home-a");
    // The crew's stall to its bosun, then the judge's escalation to the governor.
    let (to_bosun, first) = stall(CREW, "fleet/bosun", "stall-1");
    primitives.admit(NAMESPACE, &to_bosun, &first).await.expect("to the bosun");
    primitives.await_reply(NAMESPACE, &to_bosun, &first, "home-a", "stall", Some(CREW)).await.expect("recorded");
    let (to_governor, second) = stall(CREW, "fleet/governor", "stall-2");
    primitives.admit(NAMESPACE, &to_governor, &second).await.expect("to the governor");
    assert_eq!(primitives.awaiting().open_about(&seat).expect("read").len(), 1, "admission alone replaces nothing");
    primitives.await_reply(NAMESPACE, &to_governor, &second, "home-a", "stall", Some(CREW)).await.expect("recorded");
    assert_eq!(primitives.awaiting().get(&record("bosun", &to_bosun)).expect("bosun").label(), AwaitingReplyLabel::Withdrawn);
    assert_eq!(primitives.awaiting().get(&record("governor", &to_governor)).expect("governor").label(), AwaitingReplyLabel::Open);
    // The bosun escalating it in turn: its record, about the crew's seat, replaces the crew's.
    let (from_bosun, third) = stall("fleet/bosun", "fleet/admiral", "stall-3");
    primitives.admit(NAMESPACE, &from_bosun, &third).await.expect("to the admiral");
    primitives.await_reply(NAMESPACE, &from_bosun, &third, "home-a", "stall", Some(CREW)).await.expect("recorded");
    assert_eq!(primitives.awaiting().get(&record("governor", &to_governor)).expect("governor").label(), AwaitingReplyLabel::Withdrawn);
    let open = primitives.awaiting().open_about(&seat).expect("read");
    assert_eq!(open.iter().map(|record| record.spec().sender.as_str()).collect::<Vec<_>>(), vec!["project:fleet/role:bosun"]);
    // A repeat of an earlier send records nothing more and leaves the newest stall open.
    primitives.await_reply(NAMESPACE, &from_bosun, &third, "home-a", "stall", Some(CREW)).await.expect("repeat");
    assert!(primitives.awaiting().stalled(&seat).expect("read"));
}

// Clearing a crew or convoy stall withdraws its records without a reply; a still-stalled
// subject keeps its records, and a home with no open stall reads nothing.
#[tokio::test]
async fn a_cleared_stall_withdraws_its_subjects_records() {
    for (subject, canonical) in
        [(CREW, "project:flotilla/convoy:demo/role:coder"), ("project:flotilla/convoy:demo", "project:flotilla/convoy:demo")]
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let primitives = primitives(&dir).await;
        let seat: flotilla_address::Address = canonical.parse().expect("subject");
        assert!(primitives.end_stalls(NAMESPACE, &[subject.to_owned()], None, "cleared").await.expect("nothing open").is_empty());
        let mut stall = intent("system:stall-judge", "fleet/governor");
        stall.relation = MessageRelation::Supervisor;
        stall.expectation = flotilla_resources::MessageExpectation::Reply;
        let name = flotilla_resources::message_record_name("fleet/governor", "system:stall-judge", "stall-1");
        primitives.admit(NAMESPACE, &name, &stall).await.expect("sent");
        primitives.await_reply(NAMESPACE, &name, &stall, "home-a", "stall", Some(subject)).await.expect("recorded");
        // Another vessel's seat of the same role shares the record's subject: it keeps the records.
        let twin = "flotilla/demo/other/coder";
        assert!(primitives.end_stalls(NAMESPACE, &[twin.to_owned()], Some(subject), "cleared").await.expect("kept").is_empty());
        assert!(primitives.awaiting().stalled(&seat).expect("read"));
        assert_eq!(primitives.end_stalls(NAMESPACE, &[subject.to_owned()], None, "cleared").await.expect("ended").len(), 1);
        assert!(!primitives.awaiting().stalled(&seat).expect("read"), "the cleared stall is not open");
        let sender = "fleet:lab/role:stall-judge".parse().expect("sender");
        assert!(
            primitives.awaiting().open_from(&sender).expect("sender records").is_empty(),
            "clearing without a reply leaves no open record"
        );
    }
}

// Behaviour (#3730 review nit): a system producer with no fleet role has no inbox for the reply, so
// its send records nothing, quietly, rather than failing every pass. The stall judge is a fleet role
// and a person holds the operator role of the project (#3938), so their sends are recorded as theirs.
#[tokio::test]
async fn a_sender_with_no_inbox_records_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let ask = |sender: &str| {
        let mut ask = intent(sender, "fleet/governor");
        ask.expectation = flotilla_resources::MessageExpectation::Reply;
        (flotilla_resources::message_record_name("fleet/governor", sender, "ask"), ask)
    };
    let (name, unknown) = ask("system:no-such-producer");
    assert!(!primitives.await_reply(NAMESPACE, &name, &unknown, "home-a", "reply", None).await.expect("nothing to await"));
    for sender in ["system:stall-judge", "principal:alice"] {
        let (name, recorded) = ask(sender);
        assert!(
            primitives.await_reply(NAMESPACE, &name, &recorded, "home-a", "reply", None).await.expect("the sender's record"),
            "{sender}"
        );
    }
    let operator = flotilla_address::Address::project("fleet").and_then(|fleet| fleet.child(flotilla_address::Kind::Role, "operator"));
    assert_eq!(primitives.awaiting().open_from(&operator.expect("operator")).expect("records").len(), 1, "a person awaits as their role");
}

// The replicated designation alone supplies the fleet name. Changing its
// charter Project preserves addresses; old unnamed records decode but refuse
// resolution rather than silently using the Project name.
#[tokio::test]
async fn fleet_identity_is_independent_of_the_designated_project() {
    use flotilla_core::primitives::roles::designated_fleet_roles;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    for project in ["project-map", "renamed-charter"] {
        backend
            .definitions::<Project>(NAMESPACE)
            .apply(
                &InputMeta::builder().name(project.into()).build(),
                &ProjectSpec::builder().display_name(project.into()).default_workflow_ref("w".into()).repositories(Vec::new()).build(),
            )
            .await
            .expect("charter Project");
    }
    let designations = backend.definitions::<FleetDesignation>(NAMESPACE);
    let meta = InputMeta::builder().name(FLEET_DESIGNATION_NAME.into()).build();
    let mut spec: FleetDesignationSpec = serde_json::from_value(serde_json::json!({"project": "project-map"})).expect("old shape decodes");
    designations.apply(&meta, &spec).await.expect("old designation");
    let error = designated_fleet_roles(&backend, NAMESPACE).await.expect_err("no fallback");
    assert!(error.to_string().contains("FleetDesignation.spec.fleet is missing"), "{error}");
    for name in ["", "   "] {
        spec.fleet = Some(name.into());
        designations.apply(&meta, &spec).await.expect("empty name stored");
        let error = designated_fleet_roles(&backend, NAMESPACE).await.expect_err("empty name refused");
        assert!(error.to_string().contains("nonempty fleet"), "{error}");
    }
    spec.fleet = Some("lab".into());
    for project in ["project-map", "renamed-charter"] {
        spec.project = project.into();
        designations.apply(&meta, &spec).await.expect("designation");
        let roles = designated_fleet_roles(&backend, NAMESPACE).await.expect("named fleet");
        assert_eq!(roles.address("nudge").expect("role").as_str(), "fleet:lab/role:nudge");
        assert_eq!(roles.fleet().as_str(), "fleet:lab");
    }
}

// Behaviour (#3894): a turn admitted to a crew and not yet delivered holds
// that crew's derived Done, for one delivery hold at most; a delivered turn,
// or a turn to another crew, does not.
#[tokio::test]
async fn an_undelivered_turn_awaits_delivery_for_one_hold() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(chrono::Utc::now()));
    let primitives = primitives_at(&dir, clock.clone()).await;
    let other = "flotilla/demo/work/reviewer";
    assert!(!primitives.awaits_delivery(NAMESPACE, CREW).await.expect("read"), "nothing admitted");
    let record = flotilla_resources::message_record_name(CREW, "system:nudge", "late");
    primitives.admit(NAMESPACE, &record, &intent("system:nudge", CREW)).await.expect("admitted");
    assert!(primitives.awaits_delivery(NAMESPACE, CREW).await.expect("read"), "an undelivered turn holds its crew");
    assert!(!primitives.awaits_delivery(NAMESPACE, other).await.expect("read"), "only its own crew");
    clock.advance(flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR);
    assert!(!primitives.awaits_delivery(NAMESPACE, CREW).await.expect("read"), "the hold ends after one delivery hold");

    let delivered = flotilla_resources::message_record_name(other, "system:nudge", "delivered");
    primitives.admit(NAMESPACE, &delivered, &intent("system:nudge", other)).await.expect("admitted");
    assert!(primitives.awaits_delivery(NAMESPACE, other).await.expect("read"));
    let message = engine_message(&primitives, &delivered);
    let consumer = primitives.register("reviewer-session", flotilla_schema::Timestamp(1)).await.expect("consumer");
    let inbox = primitives.inbox();
    inbox.bind(message.spec().inbox.clone(), consumer.clone()).await.expect("bind");
    inbox.receive(&consumer, message.name(), flotilla_state_machines::Transport::Turn).await.expect("receipt");
    inbox
        .deliver(
            message.name(),
            flotilla_state_machines::machines::message::Deliver { receiver: consumer, transport: flotilla_state_machines::Transport::Turn },
        )
        .await
        .expect("deliver");
    assert!(!primitives.awaits_delivery(NAMESPACE, other).await.expect("read"), "a delivered turn no longer holds its crew");
}
