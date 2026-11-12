//! A receiver's waiting messages and a message by name are read without loading the rest of the
//! replica (#3108): with thousands of settled messages held, a pending read, a by-name read and a
//! delivery hold each load and convert only the messages they answer with.

use flotilla_address::Address;
use flotilla_engine_messages::RowsRead;
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_inbox::InboxBackend;
use flotilla_resources::{MessageRelation, MessageSpec};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{machines::message::Deliver, Transport};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const BUSY: &str = "flotilla/coder";
const QUIET: &str = "flotilla/reviewer";
const SETTLED: usize = 2000;

fn intent(receiver: &str, body: &str) -> MessageSpec {
    MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver(receiver.into())
        .relation(MessageRelation::System)
        .body(body.into())
        .build()
}

fn cost(before: RowsRead, after: RowsRead) -> (u64, u64) {
    (after.loaded - before.loaded, after.converted - before.converted)
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
    for n in 0..3 {
        let name = format!("waiting-{n}");
        primitives.admit(NAMESPACE, &name, &intent(BUSY, &name)).await.expect("admit");
    }
    for n in 0..2 {
        let name = format!("quiet-{n}");
        primitives.admit(NAMESPACE, &name, &intent(QUIET, &name)).await.expect("admit");
    }

    let before = primitives.rows_read();
    let waiting = primitives.pending_to(NAMESPACE, &[BUSY.to_owned()]).await.expect("pending read");
    let mut names = waiting.iter().map(|read| read.object.metadata.name.clone()).collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["waiting-0", "waiting-1", "waiting-2"]);
    assert_eq!(cost(before, primitives.rows_read()), (3, 3), "a pending read loads and converts only the waiting messages");

    let before = primitives.rows_read();
    let settled = primitives.view_to(NAMESPACE, BUSY, "settled-17").await.expect("by-name read").expect("held");
    assert_eq!(settled.object.metadata.name, "settled-17");
    assert_eq!(cost(before, primitives.rows_read()), (1, 1), "a by-name read loads and converts one message");

    let before = primitives.rows_read();
    let named = primitives.view_for(NAMESPACE, &intent(QUIET, "quiet-1"), "quiet-1").await.expect("by-name read").expect("held");
    assert_eq!(named.object.spec.receiver, QUIET);
    assert_eq!(cost(before, primitives.rows_read()), (1, 1), "a by-name read for an intent loads and converts one message");

    let before = primitives.rows_read();
    assert!(primitives.view_to(NAMESPACE, QUIET, "settled-17").await.expect("by-name read").is_none(), "another inbox holds none");
    assert_eq!(cost(before, primitives.rows_read()), (0, 0), "a miss loads nothing");

    let before = primitives.rows_read();
    let held = primitives.delivery_held_until(NAMESPACE, BUSY).await.expect("hold read");
    assert!(held.is_some(), "the waiting messages hold delivery");
    assert_eq!(cost(before, primitives.rows_read()).0, 3, "a delivery hold loads only the waiting messages");
}
