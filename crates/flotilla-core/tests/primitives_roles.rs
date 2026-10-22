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
            &FleetDesignationSpec { project: "fleet".into(), image_cache: None },
        )
        .await
        .expect("designation");
    let service = Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), "home-a", flotilla_engine_store::registry().expect("registry")).expect("store"),
    );
    MessagePrimitives::open("home-a", service, backend, Arc::new(SystemClock)).expect("primitives")
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
// the designated project, and a held turn sends as turn-rules; `system:hold`
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
        assert_eq!(message.spec().sender.as_str(), format!("project:fleet/role:{role}"));
        assert_eq!(message.spec().inbox.as_str(), "project:flotilla/convoy:demo/role:coder");
        assert_eq!(message.name(), format!("project:flotilla/convoy:demo/role:coder/message:{record}"));
    }
    // The held turn's name keys on `system:hold`; it sends as turn-rules.
    let held = flotilla_resources::message_record_name("system:hold", "system:turn-rules", "hold:demo:work:coder:rule:1");
    primitives.admit(NAMESPACE, &held, &intent("system:turn-rules", CREW)).await.expect("held turn");
    assert_eq!(engine_message(&primitives, &held).spec().sender.as_str(), "project:fleet/role:turn-rules");
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
    assert_eq!(stored.spec().inbox.as_str(), "project:fleet/role:nudge");
    assert_eq!(stored.spec().sender.as_str(), "project:flotilla/convoy:demo/role:coder");
    let (original_id, home) = primitives.locate(&original).expect("locate").expect("original");
    let in_reply_to = stored.spec().in_reply_to.as_ref().expect("names the original");
    assert_eq!((in_reply_to.name.as_str(), in_reply_to.home.as_str()), (original_id.as_str(), home.as_str()));
    assert_eq!(*stored.state(), MessageState::Pending);
    let inbox: flotilla_address::Address = "project:fleet/role:nudge".parse().expect("inbox");
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
    primitives.await_reply(NAMESPACE, &late, &stall, "home-a", "stall", Some(CREW)).await.expect("recorded");
    let awaited = MessageRef::new(format!("project:fleet/role:governor/message:{late}"), "home-a");
    assert_eq!(primitives.awaiting().get(&awaited).expect("record").label(), AwaitingReplyLabel::Answered);
    // A send expecting nothing records nothing.
    let (plain, mut note) = send("note");
    note.expectation = flotilla_resources::MessageExpectation::None;
    assert!(!primitives.await_reply(NAMESPACE, &plain, &note, "home-a", "reply", None).await.expect("nothing"));
}

// Behaviour (#3730 review): a message superseded at this home withdraws its sender's record here,
// and the ended record is evicted from memory yet still read from the store.
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
    assert_eq!(primitives.awaiting().get(&awaited).expect("read from the store").label(), AwaitingReplyLabel::Withdrawn);
    assert!(!primitives.awaiting().stalled(&seat).expect("read"), "the withdrawn stall is not open");
}
