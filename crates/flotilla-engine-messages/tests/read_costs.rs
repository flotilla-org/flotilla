//! A receiver's waiting messages and a message by name are read without loading the rest of the
//! replica (#3108): with thousands of settled messages held, each read through an inbox's index or
//! by name loads and converts only the messages it answers with. The generic verb's listing is the
//! one read that loads the whole table.

use std::future::Future;

use flotilla_address::Address;
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_inbox::InboxBackend;
use flotilla_resources::{MessageRelation, MessageSpec};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{machines::message::Deliver, Transport};
use flotilla_store::{
    load_metrics::{measure, measure_now, ReadCounts},
    ResourceBackend,
};
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const BUSY: &str = "flotilla/coder";
const QUIET: &str = "flotilla/reviewer";
const PERSON: &str = "principal:alice";
const SETTLED: usize = 2000;

fn intent(receiver: &str, body: &str) -> MessageSpec {
    MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver(receiver.into())
        .relation(MessageRelation::System)
        .body(body.into())
        .build()
}

/// The Messages decoded and converted.
fn cost(reads: &ReadCounts) -> (u64, u64) {
    reads.get("Message").map_or((0, 0), |counts| (counts.decoded, counts.converted))
}

/// `work`'s result and the Messages it decoded and converted.
async fn measured<T>(work: impl Future<Output = T>) -> (T, (u64, u64)) {
    let (value, reads) = measure(work).await;
    (value, cost(&reads))
}

/// `work`'s result and the Messages it decoded and converted, read on this thread.
fn measured_now<T>(work: impl FnOnce() -> T) -> (T, (u64, u64)) {
    let (value, reads) = measure_now(work);
    (value, cost(&reads))
}

#[tokio::test]
async fn reads_by_receiver_and_by_name_load_only_their_messages() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let home = EngineMessagesHome::open("home-a", NAMESPACE, &backend);
    let primitives = home.primitives();
    let inbox: Address = "project:flotilla/role:coder".parse().expect("inbox");
    let session = primitives.register("session-coder", Timestamp::from_datetime(&chrono::Utc::now())).await.expect("register");
    primitives.inbox().bind(inbox.clone(), session.clone()).await.expect("bind");

    for n in 0..SETTLED {
        let name = format!("settled-{n}");
        primitives.admit(NAMESPACE, &name, &intent(BUSY, &name)).await.expect("admit");
        let (id, _) = primitives.locate_in(&inbox, &name).expect("locate").expect("admitted");
        primitives.inbox().receive(&session, &id, Transport::Turn).await.expect("receipt");
        primitives.inbox().deliver(&id, Deliver { receiver: session.clone(), transport: Transport::Turn }).await.expect("deliver");
    }
    // Two more settle after a mark, for the read of those settled since.
    let mark = Timestamp::from_datetime(&chrono::Utc::now());
    for n in 0..2 {
        let name = format!("late-{n}");
        primitives.admit(NAMESPACE, &name, &intent(BUSY, &name)).await.expect("admit");
        let (id, _) = primitives.locate_in(&inbox, &name).expect("locate").expect("admitted");
        primitives.inbox().receive(&session, &id, Transport::Turn).await.expect("receipt");
        primitives.inbox().deliver(&id, Deliver { receiver: session.clone(), transport: Transport::Turn }).await.expect("deliver");
    }
    for n in 0..3 {
        let name = format!("waiting-{n}");
        primitives.admit(NAMESPACE, &name, &intent(BUSY, &name)).await.expect("admit");
    }
    for n in 0..2 {
        let name = format!("quiet-{n}");
        primitives.admit(NAMESPACE, &name, &intent(QUIET, &name)).await.expect("admit");
    }
    let to_person = MessageSpec { sender: "system:stall-judge".into(), ..intent(PERSON, "stalled") };
    primitives.admit(NAMESPACE, "for-a-person", &to_person).await.expect("admit");
    let total = SETTLED + 2 + 3 + 2 + 1;

    let (waiting, cost) = measured(primitives.pending_to(NAMESPACE, &[BUSY.to_owned()])).await;
    let mut names = waiting.expect("pending read").iter().map(|read| read.object.metadata.name.clone()).collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["waiting-0", "waiting-1", "waiting-2"]);
    assert_eq!(cost, (3, 3), "a pending read loads and converts only the waiting messages");

    let (settled, cost) = measured(primitives.view_to(NAMESPACE, BUSY, "settled-17")).await;
    assert_eq!(settled.expect("by-name read").expect("held").object.metadata.name, "settled-17");
    assert_eq!(cost, (1, 1), "a by-name read loads and converts one message");

    let (named, cost) = measured(primitives.admitted(NAMESPACE, &intent(QUIET, "quiet-1"), "quiet-1")).await;
    assert_eq!(named.expect("by-name read").expect("held").superseded_by, None);
    assert_eq!(cost, (1, 0), "a by-name read for an intent loads one message and converts none");

    let (missing, cost) = measured(primitives.view_to(NAMESPACE, QUIET, "settled-17")).await;
    assert!(missing.expect("by-name read").is_none(), "another inbox holds none");
    assert_eq!(cost, (0, 0), "a miss loads nothing");

    let (held, cost) = measured(primitives.delivery_held_until(NAMESPACE, BUSY)).await;
    assert!(held.expect("hold read").is_some(), "the waiting messages hold delivery");
    assert_eq!(cost.0, 3, "a delivery hold loads only the waiting messages");

    let (open, cost) = measured(primitives.open_to(NAMESPACE, &[BUSY.to_owned()])).await;
    assert_eq!(open.expect("open read").len(), 3, "the waiting messages are the open ones");
    assert_eq!(cost, (3, 3), "an open read loads and converts only the open messages");

    // A seat's open messages, its newest settled and delivered (here one message), and a named one.
    let quiet = [(QUIET.to_owned(), "quiet-0".to_owned())];
    let (seat, cost) = measured(primitives.seat_messages(NAMESPACE, &[BUSY.to_owned()], &quiet)).await;
    let mut names = seat.expect("seat read").iter().map(|read| read.object.metadata.name.clone()).collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["late-1", "quiet-0", "waiting-0", "waiting-1", "waiting-2"]);
    assert_eq!(cost, (6, 5), "a seat read loads each row it answers with once per read of it");

    let (newest, cost) = measured_now(|| primitives.newest_delivered(&inbox));
    let newest = newest.expect("newest delivered").expect("delivered");
    assert!(newest.1.name().ends_with("/message:late-1"), "{}", newest.1.name());
    assert_eq!(cost.0, 1, "the newest delivered loads one message");

    let (late, cost) = measured_now(|| primitives.settled_since(&inbox, mark));
    assert_eq!(late.expect("settled since").len(), 2, "two settled after the mark");
    assert_eq!(cost.0, 3, "a settled-since read loads those and the one that ends it");

    let (person, cost) = measured(primitives.person_messages(NAMESPACE)).await;
    assert_eq!(person.expect("person messages").len(), 1);
    assert_eq!(cost.0, 1, "a person's waiting messages load only those");

    let (named, cost) = measured(primitives.view_for_person(NAMESPACE, "for-a-person")).await;
    assert_eq!(named.expect("a person's message").object.metadata.name, "for-a-person");
    assert_eq!(cost, (1, 1), "a person's message by name loads and converts one");

    // The generic verb's listing is the one Message read proportional to the table.
    let (listed, cost) = measured(backend.using::<flotilla_resources::Message>(NAMESPACE).list()).await;
    assert_eq!(listed.expect("verb listing").items.len(), total);
    assert_eq!(cost.0, total as u64, "the verb's listing loads every message once");
}
