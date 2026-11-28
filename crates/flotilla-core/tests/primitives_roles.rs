//! Producers send as their fleet roles on the engine path, and a reply to a
//! role reaches that role's inbox (#3317).

use flotilla_engine_messages_testkit::agent_session_request;
use std::sync::Arc;

use flotilla_engine_messages::MessagePrimitives;
use flotilla_engine_messages_testkit::Served;
use flotilla_inbox::InboxBackend;
use flotilla_messaging::{Intent, IntentExpectation, IntentReference, StallOf};
use flotilla_provisioning_testkit::EngineBackend;
use flotilla_resources::{
    FleetDesignation, FleetDesignationSpec, InputMeta, MessageRelation, Project, ProjectSpec, SystemClock, FLEET_DESIGNATION_NAME,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::machines::message::{Message, MessageState};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const CREW: &str = "flotilla/demo/work/coder";

/// An inferred stall about `about`: no crew declared it, so it carries no evidence.
fn stall_of(about: &str) -> StallOf {
    StallOf { about: about.to_owned(), report: None, reason: None, proposed_disposition: None }
}

async fn primitives(dir: &tempfile::TempDir) -> Served<MessagePrimitives> {
    primitives_at(dir, Arc::new(SystemClock)).await
}

async fn primitives_at(dir: &tempfile::TempDir, clock: Arc<dyn flotilla_resources::Clock>) -> Served<MessagePrimitives> {
    let home = EngineBackend::open("home-a");
    let backend = home.backend.clone();
    primitives_over(dir, backend, home, clock).await
}

/// The Message path over `backend`, whose records the conditions read, held with `held`, which
/// owns that backend.
async fn primitives_over(
    dir: &tempfile::TempDir,
    backend: ResourceBackend,
    held: impl std::any::Any + Send + Sync,
    clock: Arc<dyn flotilla_resources::Clock>,
) -> Served<MessagePrimitives> {
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
    Served::holding(MessagePrimitives::open("home-a", service, backend, clock).expect("primitives"), held)
}

/// A message from `sender` to `receiver`, each a crew seat, a `system:` producer or an address.
fn intent(sender: &str, receiver: &str) -> Intent {
    flotilla_engine_messages_testkit::intent(sender, receiver, MessageRelation::System, format!("from {sender}"))
}

/// The engine record of `record`, admitted to `receiver` (an old address), read in its inbox.
async fn engine_message(primitives: &MessagePrimitives, receiver: &str, record: &str) -> flotilla_state_machines::Resource<Message> {
    let inbox = primitives.inbox_for(NAMESPACE, receiver).await.expect("the receiver's inbox");
    let (id, _) = primitives.locate_in(&inbox, record).expect("locate").expect("admitted");
    let messages = primitives.inbox_messages(&inbox).expect("messages");
    messages.into_iter().map(|(_, message)| message).find(|message| message.name() == id).expect("stored")
}

/// The id of `record`, admitted to `receiver` (an old address), and its home.
async fn located(primitives: &MessagePrimitives, receiver: &str, record: &str) -> (String, String) {
    let inbox = primitives.inbox_for(NAMESPACE, receiver).await.expect("the receiver's inbox");
    primitives.locate_in(&inbox, record).expect("locate").expect("admitted")
}

// Behaviour: each `system:` producer sends as its declared fleet role under
// the fleet name, and a held turn sends as turn-rules; `system:hold`
// survives only in its de-duplication key (the record name).
#[tokio::test]
async fn system_producers_send_as_their_fleet_roles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    for role in ["turn-rules", "nudge", "stall-judge", "capabilities", "fleet-store", "standing", "teardown"] {
        let sender = format!("system:{role}");
        let record = flotilla_resources::message_record_name(CREW, &sender, role);
        primitives.admit(&record, &intent(&sender, CREW)).await.expect("admitted");
        let message = engine_message(&primitives, CREW, &record).await;
        assert_eq!(message.spec().sender.as_str(), format!("fleet:lab/role:{role}"));
        assert_eq!(message.spec().inbox.as_str(), "project:flotilla/convoy:demo/role:coder");
        assert_eq!(message.name(), format!("project:flotilla/convoy:demo/role:coder/message:{record}"));
    }
    // The held turn's name keys on `system:hold`; it sends as turn-rules.
    let held = flotilla_resources::message_record_name("system:hold", "system:turn-rules", "hold:demo:work:coder:rule:1");
    primitives.admit(&held, &intent("system:turn-rules", CREW)).await.expect("held turn");
    assert_eq!(engine_message(&primitives, CREW, &held).await.spec().sender.as_str(), "fleet:lab/role:turn-rules");
    // A repeated key is the same message: nothing more is admitted.
    let inbox = primitives.inbox_for(NAMESPACE, CREW).await.expect("the crew's inbox");
    let count = primitives.inbox_messages(&inbox).expect("messages").len();
    primitives.admit(&held, &intent("system:turn-rules", CREW)).await.expect("repeat");
    assert_eq!(primitives.inbox_messages(&inbox).expect("messages").len(), count);
}

// Behaviour: a crew's reply to a fleet role's message lands in that role's
// own inbox, naming the original; nobody is bound there yet, so it waits.
#[tokio::test]
async fn a_reply_to_a_fleet_role_reaches_its_inbox() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let original = flotilla_resources::message_record_name(CREW, "system:nudge", "stall-nudge-1");
    let mut nudge = intent("system:nudge", CREW);
    let issue = flotilla_resources::address::stored_issue_address("github", "flotilla-org/flotilla", 7, &[]).expect("issue");
    nudge.references = vec![IntentReference::new(issue, flotilla_address::Revision::new("1".to_owned()).expect("revision"))];
    primitives.admit(&original, &nudge).await.expect("nudge");
    let reply_name = flotilla_resources::message_record_name("system:nudge", CREW, "reply-1");
    let mut reply = intent(CREW, "system:nudge");
    reply.relation = MessageRelation::Peer;
    reply.in_reply_to = Some(original.clone());
    primitives.admit(&reply_name, &reply).await.expect("reply");
    let stored = engine_message(&primitives, "system:nudge", &reply_name).await;
    assert_eq!(stored.spec().inbox.as_str(), "fleet:lab/role:nudge");
    assert_eq!(stored.spec().sender.as_str(), "project:flotilla/convoy:demo/role:coder");
    let (original_id, home) = located(&primitives, CREW, &original).await;
    let in_reply_to = stored.spec().in_reply_to.as_ref().expect("names the original");
    assert_eq!((in_reply_to.name.as_str(), in_reply_to.home.as_str()), (original_id.as_str(), home.as_str()));
    assert_eq!(*stored.state(), MessageState::Pending);
    let inbox: flotilla_address::Address = "fleet:lab/role:nudge".parse().expect("inbox");
    let consumer =
        primitives.register("fleet-nudge-handler", agent_session_request(), flotilla_schema::Timestamp(1)).await.expect("consumer");
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
    use flotilla_engine_messages::sweep::OpenMessages;
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let mut turn = intent("system:turn-rules", CREW);
    turn.expectation = IntentExpectation::Reply;
    let record = flotilla_resources::message_record_name(CREW, "system:turn-rules", "rule@1");
    primitives.admit(&record, &turn).await.expect("turn");
    let message = engine_message(&primitives, CREW, &record).await;
    let consumer = primitives.register("crew-session", agent_session_request(), flotilla_schema::Timestamp(1)).await.expect("consumer");
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
    assert!(matches!(engine_message(&primitives, CREW, &record).await.state(), MessageState::Awaiting { .. }));
    // The crew answers; the sweep settles the original from the log since its last pass.
    let mut answer = intent(CREW, "system:turn-rules");
    answer.relation = MessageRelation::Peer;
    answer.in_reply_to = Some(record.clone());
    let answer_name = flotilla_resources::message_record_name("system:turn-rules", CREW, "reply@1");
    primitives.admit(&answer_name, &answer).await.expect("answer");
    let report = primitives.sweep(NAMESPACE, now, &mut open).await.expect("sweep");
    assert_eq!(report.answered, 1);
    assert!(matches!(engine_message(&primitives, CREW, &record).await.state(), MessageState::Settled { .. }));
    let (answer_id, _) = located(&primitives, "system:turn-rules", &answer_name).await;
    assert_eq!(open.open().collect::<Vec<_>>(), [answer_id.as_str()], "only open messages are swept again");
}

// Behaviour (#3317): the sweep settles a message whose outcome condition holds, leaves one whose
// condition does not, and withdraws a pending message once its delivery condition is observed false,
// naming the condition as the authority.
#[tokio::test]
async fn the_sweep_settles_held_outcomes_and_withdraws_on_a_false_delivery_condition() {
    use flotilla_engine_messages::sweep::OpenMessages;
    use flotilla_resources::{Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus};
    use flotilla_state_machines::machines::message::DropReason;
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &InputMeta::builder().name("demo".to_string()).build(),
            &ConvoySpec::builder().project_ref(NAMESPACE.into()).workflow_ref("w".into()).build(),
        )
        .await
        .expect("convoy");
    convoys
        .update_status("demo", &convoy.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() })
        .await
        .expect("active convoy");
    let primitives = primitives_over(&dir, backend.clone(), backend, Arc::new(SystemClock)).await;
    // A condition as the crew's project reads it.
    let leaf = |text: &str| {
        let leaf = text.parse::<flotilla_protocol::Leaf>().expect("condition");
        flotilla_resources::address::leaf_condition(&leaf, Some("flotilla"), &[]).expect("an ADR 0066 condition")
    };
    let send = |key: &str, edit: &dyn Fn(&mut Intent)| {
        let mut spec = intent("system:turn-rules", CREW);
        edit(&mut spec);
        (flotilla_resources::message_record_name(CREW, "system:turn-rules", key), spec)
    };
    let (met, met_spec) =
        send("met", &|spec| spec.expectation = IntentExpectation::Outcome { condition: leaf("convoy/demo .status.phase == Active") });
    let (unmet, unmet_spec) =
        send("unmet", &|spec| spec.expectation = IntentExpectation::Outcome { condition: leaf("convoy/demo .status.phase == Completed") });
    let (moot, moot_spec) = send("moot", &|spec| spec.delivery_condition = Some(leaf("convoy/demo .status.phase == Completed")));
    for (name, spec) in [(&met, &met_spec), (&unmet, &unmet_spec), (&moot, &moot_spec)] {
        primitives.admit(name, spec).await.expect("admitted");
    }
    let mut open = OpenMessages::default();
    let now = flotilla_schema::Timestamp::from_datetime(&chrono::Utc::now());
    let report = primitives.sweep(NAMESPACE, now, &mut open).await.expect("sweep");
    assert_eq!((report.outcomes, report.withdrawn), (1, 1));
    assert!(matches!(engine_message(&primitives, CREW, &met).await.state(), MessageState::Settled { .. }), "the outcome holds");
    assert_eq!(*engine_message(&primitives, CREW, &unmet).await.state(), MessageState::Pending, "the outcome does not hold yet");
    let moot = engine_message(&primitives, CREW, &moot).await;
    let Some(DropReason::Withdrawn { by, authority }) = moot.state().drop_reason() else { panic!("withdrawn: {:?}", moot.state()) };
    assert_eq!(by.as_str(), "fleet:lab/role:turn-rules");
    assert!(authority.as_str().contains("demo"), "the condition is the authority: {authority}");
}

// Behaviour: a message dropped undelivered, its inbox unbound past the threshold, tells the
// operator of its sender's context with a dead letter sent as turn-rules about it: a crew's, the
// operator role of the crew's project; a fleet role's, the fleet's operator.
#[tokio::test]
async fn a_message_dropped_undelivered_sends_a_dead_letter_to_its_senders_operator() {
    use flotilla_engine_messages::sweep::OpenMessages;
    let dir = tempfile::tempdir().expect("tempdir");
    let start = chrono::Utc::now();
    let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(start));
    let primitives = primitives_at(&dir, Arc::clone(&clock) as Arc<dyn flotilla_resources::Clock>).await;
    const REVIEWER: &str = "flotilla/demo/review/reviewer";
    let from_crew = flotilla_resources::message_record_name(REVIEWER, CREW, "handoff");
    primitives.admit(&from_crew, &intent(CREW, REVIEWER)).await.expect("handoff");
    let from_rules = flotilla_resources::message_record_name(CREW, "system:turn-rules", "rule@1");
    primitives.admit(&from_rules, &intent("system:turn-rules", CREW)).await.expect("turn");
    let mut open = OpenMessages::default();
    let now = flotilla_schema::Timestamp::from_datetime(&start);
    assert_eq!(primitives.sweep(NAMESPACE, now, &mut open).await.expect("sweep").undeliverable, 0);

    let later = start + chrono::Duration::hours(24) + chrono::Duration::seconds(1);
    clock.set(later);
    let report = primitives.sweep(NAMESPACE, flotilla_schema::Timestamp::from_datetime(&later), &mut open).await.expect("sweep");
    assert_eq!(report.undeliverable, 2);
    for (receiver, record, operator) in
        [(REVIEWER, &from_crew, "project:flotilla/role:operator"), (CREW, &from_rules, "fleet:lab/role:operator")]
    {
        let (dropped, _) = located(&primitives, receiver, record).await;
        let operator: flotilla_address::Address = operator.parse().expect("operator inbox");
        let letter = flotilla_inbox::dead_letter_name(&operator, &dropped).expect("dead letter name");
        let letters = primitives.inbox_messages(&operator).expect("operator's messages");
        let (_, letter) =
            letters.iter().find(|(_, message)| message.name() == letter).unwrap_or_else(|| panic!("no dead letter at {operator}"));
        assert_eq!(letter.spec().sender.as_str(), "fleet:lab/role:turn-rules");
        assert!(letter.spec().references.iter().any(|reference| reference.subject && reference.address.as_str() == dropped), "{letter:?}");
    }
}

// Behaviour (#3108): the sweep's next wake is the earliest of several open messages' deadlines, and
// once that message expires, the next earliest.
#[tokio::test]
async fn the_sweep_wakes_at_the_earliest_of_several_deadlines() {
    use flotilla_engine_messages::sweep::OpenMessages;
    let dir = tempfile::tempdir().expect("tempdir");
    let start = chrono::Utc::now();
    // The expiry gate reads the store's clock: it moves with the sweep.
    let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(start));
    let primitives = primitives_at(&dir, Arc::clone(&clock) as Arc<dyn flotilla_resources::Clock>).await;
    for (key, seconds) in [("late", 30), ("first", 10), ("second", 20)] {
        let mut spec = intent("system:nudge", CREW);
        spec.deadline = Some(start + chrono::Duration::seconds(seconds));
        let record = flotilla_resources::message_record_name(CREW, "system:nudge", key);
        primitives.admit(&record, &spec).await.expect("admitted");
    }
    let at = |seconds: i64| flotilla_schema::Timestamp::from_datetime(&(start + chrono::Duration::seconds(seconds)));
    let mut open = OpenMessages::default();
    let report = primitives.sweep(NAMESPACE, at(0), &mut open).await.expect("sweep");
    assert_eq!(report.next_due, Some(at(10).saturating_add(1)), "just past the earliest deadline");
    clock.set(start + chrono::Duration::seconds(10) + chrono::Duration::microseconds(1));
    let report = primitives.sweep(NAMESPACE, at(10).saturating_add(1), &mut open).await.expect("sweep at the first deadline");
    assert_eq!(report.expired, 1);
    assert_eq!(report.next_due, Some(at(20).saturating_add(1)), "then the next earliest");
}

// Behaviour (#3317): an index that meets a home for the first time (a
// restart), or whose cursor is below the home's resume floor, seeds its
// open messages from one scan of that home rather than its log, so an open
// message last written long ago is still swept.
#[tokio::test]
async fn a_fresh_sweep_index_seeds_open_messages_from_a_scan() {
    use flotilla_engine_messages::sweep::OpenMessages;
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let mut names = Vec::new();
    for key in ["early@1", "early@2"] {
        let record = flotilla_resources::message_record_name(CREW, "system:nudge", key);
        primitives.admit(&record, &intent("system:nudge", CREW)).await.expect("admit");
        names.push(located(&primitives, CREW, &record).await.0);
    }
    let mut restarted = OpenMessages::default();
    restarted.refresh(&primitives).expect("seed");
    let mut open: Vec<_> = restarted.open().map(str::to_owned).collect();
    open.sort();
    names.sort();
    assert_eq!(open, names);
    // Later writes are read from the log, from the seeded cursor on.
    let later = flotilla_resources::message_record_name(CREW, "system:nudge", "later@1");
    primitives.admit(&later, &intent("system:nudge", CREW)).await.expect("admit");
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
        stall.expectation = IntentExpectation::Reply;
        (flotilla_resources::message_record_name(supervisor, CREW, id), stall)
    };
    let (name, stall) = send("stall-1");
    primitives.admit(&name, &stall).await.expect("stall sent");
    // The crew declared this stall: its record carries what the crew said, so every home can show it.
    let declared = StallOf {
        report: Some("needs scope".into()),
        reason: Some(flotilla_protocol::StallReason::Scope),
        proposed_disposition: Some(flotilla_protocol::StallProposedDisposition::ReduceScope),
        ..stall_of(CREW)
    };
    assert!(primitives.await_reply(NAMESPACE, &name, &stall, "home-a", "stall", Some(&declared)).await.expect("recorded"));
    assert!(primitives.awaiting().stalled(&seat).expect("read"), "the seat is stalled");
    let awaited = MessageRef::new(format!("project:fleet/role:governor/message:{name}"), "home-a");
    assert_eq!(primitives.awaiting().get(&awaited).expect("record").spec().recipient.as_str(), "project:fleet/role:governor");
    let evidence = primitives.awaiting().open_records().stall(&seat).expect("the seat's stall");
    assert_eq!(evidence.report.as_deref(), Some("needs scope"));
    assert_eq!(evidence.reason, Some(flotilla_protocol::StallReason::Scope));
    assert_eq!(evidence.proposed_disposition, Some(flotilla_protocol::StallProposedDisposition::ReduceScope));

    let resume = |id: &str, original: &str| {
        let mut reply = intent("fleet/governor", CREW);
        reply.relation = MessageRelation::Supervisor;
        reply.in_reply_to = Some(original.to_owned());
        (flotilla_resources::message_record_name(CREW, supervisor, id), reply)
    };
    let (reply_name, reply) = resume("resume-1", &name);
    primitives.admit(&reply_name, &reply).await.expect("reply");
    assert_eq!(primitives.awaiting().get(&awaited).expect("record").label(), AwaitingReplyLabel::Answered);
    assert!(!primitives.awaiting().stalled(&seat).expect("read"), "answered");

    // The reply first, then the record: answered as it is written.
    let (late, stall) = send("stall-2");
    primitives.admit(&late, &stall).await.expect("stall sent");
    let (reply_name, reply) = resume("resume-2", &late);
    primitives.admit(&reply_name, &reply).await.expect("reply");
    // A send may have persisted its record before a later reconciliation read failed.
    let mut partial = primitives.awaiting().get(&awaited).expect("earlier record").spec().clone();
    partial.message = MessageRef::new(format!("project:fleet/role:governor/message:{late}"), "home-a");
    primitives.awaiting().record(partial.clone(), flotilla_schema::Timestamp(1)).expect("record persisted before retry");
    assert_eq!(primitives.awaiting().get(&partial.message).expect("partial record").label(), AwaitingReplyLabel::Open);
    // The retry repeats the send that persisted the record, the same stall's evidence included.
    assert!(!primitives.await_reply(NAMESPACE, &late, &stall, "home-a", "stall", Some(&declared)).await.expect("retry reconciles reply"));
    let awaited = MessageRef::new(format!("project:fleet/role:governor/message:{late}"), "home-a");
    assert_eq!(primitives.awaiting().get(&awaited).expect("record").label(), AwaitingReplyLabel::Answered);
    // A send expecting nothing records nothing.
    let (plain, mut note) = send("note");
    note.expectation = IntentExpectation::Nothing;
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
    stall.expectation = IntentExpectation::Reply;
    let first = flotilla_resources::message_record_name(supervisor, CREW, "stall-1");
    primitives.admit(&first, &stall).await.expect("first");
    primitives.await_reply(NAMESPACE, &first, &stall, "home-a", "stall", Some(&stall_of(CREW))).await.expect("recorded");
    let mut again = stall.clone();
    again.supersedes = Some(first.clone());
    let second = flotilla_resources::message_record_name(supervisor, CREW, "stall-2");
    primitives.admit(&second, &again).await.expect("second, superseding the first");
    let awaited = MessageRef::new(format!("project:fleet/role:governor/message:{first}"), "home-a");
    assert_eq!(primitives.awaiting().get(&awaited).expect("open").label(), AwaitingReplyLabel::Open, "kept until the successor's record");
    assert!(primitives.awaiting().stalled(&seat).expect("read"), "still stalled between admission and record");
    primitives.await_reply(NAMESPACE, &second, &again, "home-a", "stall", Some(&stall_of(CREW))).await.expect("the successor's record");
    assert_eq!(primitives.awaiting().get(&awaited).expect("read from the store").label(), AwaitingReplyLabel::Withdrawn);
    assert_eq!(primitives.awaiting().open_about(&seat).expect("read").len(), 1, "only the successor's stall is open");

    // A successor that awaits nothing withdraws the predecessor's record as it is admitted.
    let mut note = stall.clone();
    note.expectation = IntentExpectation::Nothing;
    note.supersedes = Some(second.clone());
    let third = flotilla_resources::message_record_name(supervisor, CREW, "note");
    primitives.admit(&third, &note).await.expect("a note superseding the second");
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
        stall.expectation = IntentExpectation::Reply;
        (flotilla_resources::message_record_name(receiver, sender, id), stall)
    };
    let record = |receiver: &str, name: &str| MessageRef::new(format!("project:fleet/role:{receiver}/message:{name}"), "home-a");
    // The crew's stall to its bosun, then the judge's escalation to the governor.
    let (to_bosun, first) = stall(CREW, "fleet/bosun", "stall-1");
    primitives.admit(&to_bosun, &first).await.expect("to the bosun");
    primitives.await_reply(NAMESPACE, &to_bosun, &first, "home-a", "stall", Some(&stall_of(CREW))).await.expect("recorded");
    let (to_governor, second) = stall(CREW, "fleet/governor", "stall-2");
    primitives.admit(&to_governor, &second).await.expect("to the governor");
    assert_eq!(primitives.awaiting().open_about(&seat).expect("read").len(), 1, "admission alone replaces nothing");
    primitives.await_reply(NAMESPACE, &to_governor, &second, "home-a", "stall", Some(&stall_of(CREW))).await.expect("recorded");
    assert_eq!(primitives.awaiting().get(&record("bosun", &to_bosun)).expect("bosun").label(), AwaitingReplyLabel::Withdrawn);
    assert_eq!(primitives.awaiting().get(&record("governor", &to_governor)).expect("governor").label(), AwaitingReplyLabel::Open);
    // The bosun escalating it in turn: its record, about the crew's seat, replaces the crew's.
    let (from_bosun, third) = stall("fleet/bosun", "fleet/admiral", "stall-3");
    primitives.admit(&from_bosun, &third).await.expect("to the admiral");
    primitives.await_reply(NAMESPACE, &from_bosun, &third, "home-a", "stall", Some(&stall_of(CREW))).await.expect("recorded");
    assert_eq!(primitives.awaiting().get(&record("governor", &to_governor)).expect("governor").label(), AwaitingReplyLabel::Withdrawn);
    let open = primitives.awaiting().open_about(&seat).expect("read");
    assert_eq!(open.iter().map(|record| record.spec().sender.as_str()).collect::<Vec<_>>(), vec!["project:fleet/role:bosun"]);
    // A repeat of an earlier send records nothing more and leaves the newest stall open.
    primitives.await_reply(NAMESPACE, &from_bosun, &third, "home-a", "stall", Some(&stall_of(CREW))).await.expect("repeat");
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
        assert!(primitives.end_stalls(NAMESPACE, &[subject.to_owned()], &[], "cleared").await.expect("nothing open").is_empty());
        let mut stall = intent("system:stall-judge", "fleet/governor");
        stall.relation = MessageRelation::Supervisor;
        stall.expectation = IntentExpectation::Reply;
        let name = flotilla_resources::message_record_name("fleet/governor", "system:stall-judge", "stall-1");
        primitives.admit(&name, &stall).await.expect("sent");
        primitives.await_reply(NAMESPACE, &name, &stall, "home-a", "stall", Some(&stall_of(subject))).await.expect("recorded");
        // Another vessel's seat of the same role shares the record's subject: it keeps the records.
        let twin = "flotilla/demo/other/coder";
        assert!(primitives.end_stalls(NAMESPACE, &[twin.to_owned()], &[subject.to_owned()], "cleared").await.expect("kept").is_empty());
        assert!(primitives.awaiting().stalled(&seat).expect("read"));
        assert_eq!(primitives.end_stalls(NAMESPACE, &[subject.to_owned()], &[], "cleared").await.expect("ended").len(), 1);
        assert!(!primitives.awaiting().stalled(&seat).expect("read"), "the cleared stall is not open");
        let sender = "fleet:lab/role:stall-judge".parse().expect("sender");
        assert!(
            primitives.awaiting().open_from(&sender).expect("sender records").is_empty(),
            "clearing without a reply leaves no open record"
        );
    }
}

// Behaviour (#3730 review nit): the stall judge is a fleet role and a person holds the operator role
// of the project (#3938), so their sends are recorded as theirs.
#[tokio::test]
async fn a_role_senders_awaiting_is_recorded_as_theirs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let ask = |sender: &str| {
        let mut ask = intent(sender, "fleet/governor");
        ask.expectation = IntentExpectation::Reply;
        (flotilla_resources::message_record_name("fleet/governor", sender, "ask"), ask)
    };
    for sender in ["system:stall-judge", "project:fleet/role:operator"] {
        let (name, mut recorded) = ask(sender);
        if sender.starts_with("project:") {
            recorded.principal = Some("alice".into());
        }
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
    use flotilla_engine_messages::roles::designated_fleet_roles;
    let home = EngineBackend::open("home-a");
    let backend = home.backend.clone();
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
async fn an_undelivered_turn_holds_delivery_for_one_hold() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(chrono::Utc::now()));
    let primitives = primitives_at(&dir, clock.clone()).await;
    let other = "flotilla/demo/work/reviewer";
    assert_eq!(primitives.delivery_hold(NAMESPACE, CREW).await.expect("read").until, None, "nothing admitted");
    let record = flotilla_resources::message_record_name(CREW, "system:nudge", "late");
    primitives.admit(&record, &intent("system:nudge", CREW)).await.expect("admitted");
    let admitted = flotilla_schema::Timestamp::from_datetime(&flotilla_resources::Clock::now(clock.as_ref())).to_datetime().expect("now");
    assert_eq!(
        primitives.delivery_hold(NAMESPACE, CREW).await.expect("read").until,
        Some(admitted + flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR),
        "an undelivered turn holds its crew for one hold from admission"
    );
    assert_eq!(primitives.delivery_hold(NAMESPACE, other).await.expect("read").until, None, "only its own crew");
    clock.advance(flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR);
    assert_eq!(primitives.delivery_hold(NAMESPACE, CREW).await.expect("read").until, None, "the hold ends after one delivery hold");

    let delivered = flotilla_resources::message_record_name(other, "system:nudge", "delivered");
    primitives.admit(&delivered, &intent("system:nudge", other)).await.expect("admitted");
    assert!(primitives.delivery_hold(NAMESPACE, other).await.expect("read").until.is_some());
    let message = engine_message(&primitives, other, &delivered).await;
    let consumer = primitives.register("reviewer-session", agent_session_request(), flotilla_schema::Timestamp(1)).await.expect("consumer");
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
    assert_eq!(primitives.delivery_hold(NAMESPACE, other).await.expect("read").until, None, "a delivered turn no longer holds its crew");
}
