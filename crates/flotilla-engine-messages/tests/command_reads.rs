//! The reads behind crew commands (resume, supervision rulings, withdrawal) read one inbox's open
//! or pending messages and convert none (#3108): what they cost does not grow with the messages
//! held elsewhere, nor with the inbox's own settled history.

use flotilla_address::Address;
use flotilla_engine_messages::MessagePrimitives;
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_inbox::InboxBackend;
use flotilla_resources::{MessageExpectation, MessageReference, MessageRelation, MessageSpec};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{machines::message::Deliver, Transport};
use flotilla_store::{
    load_metrics::{measure, ReadCounts},
    ResourceBackend,
};
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const CODER: &str = "flotilla/coder";
const REVIEWER: &str = "flotilla/reviewer";

fn message(sender: &str, receiver: &str, relation: MessageRelation, body: &str) -> MessageSpec {
    MessageSpec::builder().sender(sender.into()).receiver(receiver.into()).relation(relation).body(body.into()).build()
}

/// A reference to the project: converted with no record read.
fn about_the_project() -> MessageReference {
    MessageReference::ControlRecord {
        resource: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Project", NAMESPACE, "flotilla"),
        revision: "1".into(),
    }
}

/// The reviewer's escalation to the coder that expects a ruling, about the project.
fn escalation(body: &str) -> MessageSpec {
    MessageSpec {
        expectation: MessageExpectation::Reply,
        references: vec![about_the_project()],
        ..message(REVIEWER, CODER, MessageRelation::Supervisor, body)
    }
}

/// A home whose coder's inbox holds two briefs from the reviewer (the second awaiting a ruling), a
/// fleet role's turn and a fleet role's supervision, all undelivered; with `others` messages delivered to the coder before them
/// and `others` waiting in other inboxes.
async fn seeded(others: usize) -> (EngineMessagesHome, ResourceBackend) {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let home = EngineMessagesHome::open("home-a", NAMESPACE, &backend);
    let primitives = home.primitives();
    let inbox: Address = "project:flotilla/role:coder".parse().expect("inbox");
    let session = primitives.register("session-coder", Timestamp::from_datetime(&chrono::Utc::now())).await.expect("register");
    primitives.inbox().bind(inbox.clone(), session.clone()).await.expect("bind");
    for n in 0..others {
        let name = format!("history-{n}");
        primitives.admit(NAMESPACE, &name, &message(REVIEWER, CODER, MessageRelation::Supervisor, &name)).await.expect("admit");
        let (id, _) = primitives.locate_in(&inbox, &name).expect("locate").expect("admitted");
        primitives.inbox().receive(&session, &id, Transport::Turn).await.expect("receipt");
        primitives.inbox().deliver(&id, Deliver { receiver: session.clone(), transport: Transport::Turn }).await.expect("deliver");
        let other = format!("flotilla/other-{n}");
        primitives
            .admit(NAMESPACE, &format!("elsewhere-{n}"), &message(REVIEWER, &other, MessageRelation::Supervisor, "elsewhere"))
            .await
            .expect("admit");
    }
    primitives.admit(NAMESPACE, "brief", &message(REVIEWER, CODER, MessageRelation::Supervisor, "first brief")).await.expect("admit");
    primitives.admit(NAMESPACE, "escalation", &escalation("second brief")).await.expect("admit");
    primitives.admit(NAMESPACE, "turn", &message("system:turn-rules", CODER, MessageRelation::System, "a turn")).await.expect("admit");
    let stall = message("system:stall-judge", CODER, MessageRelation::Supervisor, "a stall");
    primitives.admit(NAMESPACE, "stall", &stall).await.expect("admit");
    (home, backend)
}

/// What each command read answers and costs at a home seeded with `others`.
async fn command_reads(others: usize) -> Vec<(String, ReadCounts)> {
    let (home, _backend) = seeded(others).await;
    let primitives: &MessagePrimitives = home.primitives();
    let mut costs = Vec::new();

    let (undelivered, reads) =
        measure(primitives.undelivered_from(NAMESPACE, &message(REVIEWER, CODER, MessageRelation::Supervisor, "next"))).await;
    let undelivered = undelivered.expect("undelivered read").expect("an undelivered brief");
    assert_eq!((undelivered.standing.name.as_str(), undelivered.body.as_str()), ("escalation", "second brief"));
    costs.push((format!("undelivered_from: {}", undelivered.standing.name), reads));

    let (awaited, reads) = measure(primitives.awaited_reply_from(NAMESPACE, &escalation(""))).await;
    assert_eq!(awaited.expect("awaited read").as_deref(), Some("escalation"));
    costs.push(("awaited_reply_from".into(), reads));

    let (briefs, reads) = measure(primitives.queued_briefs(NAMESPACE, &[CODER.to_owned()])).await;
    let briefs = briefs.expect("queued briefs").into_iter().map(|brief| brief.standing.name).collect::<Vec<_>>();
    assert_eq!(briefs, ["brief", "escalation"], "the reviewer's briefs, in admission order; nothing a fleet role sent");
    costs.push(("queued_briefs".into(), reads));

    let (sent, reads) = measure(primitives.sent_messages(NAMESPACE, &[(CODER.to_owned(), "brief".to_owned())])).await;
    assert_eq!(sent.expect("sent read").len(), 1);
    costs.push(("sent_messages".into(), reads));
    costs
}

// Behaviour: each command read answers the same, and reads the same Messages, rows and other
// records, whether 2 or 20 messages are held elsewhere and in the inbox's history; it converts none.
#[tokio::test]
async fn command_reads_do_not_grow_with_the_messages_held() {
    let small = command_reads(2).await;
    let large = command_reads(20).await;
    for ((read, small), (_, large)) in small.iter().zip(&large) {
        let message = small.get("Message").copied().unwrap_or_default();
        assert_eq!(message.converted, 0, "{read} converts no message: {small:#?}");
        assert_eq!(message.whole_kind, 0, "{read} lists no Message table: {small:#?}");
        assert_eq!(small, large, "{read} costs the same at 2 and 20 messages held");
    }
}
