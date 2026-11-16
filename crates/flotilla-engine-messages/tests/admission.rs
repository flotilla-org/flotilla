//! Admission through the Message path (#3108), ported from messaging's old-inbox admission tests:
//! a name's repeat, a different intent under a name, explicit supersession, interruption, and a
//! name admitted again after its message was archived.

use flotilla_engine_messages::MessagePrimitives;
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_engine_store::archive::{ArchiveKinds, Archiver, Due, Prepared};
use flotilla_resources::{MessageExpectation, MessageRelation, MessageSpec};
use flotilla_schema::Timestamp;
use flotilla_state_machines::machines::message::{DropReason, Message, MessageState};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const RECEIVER: &str = "flotilla/convoy/work/coder";

fn spec(expectation: MessageExpectation) -> MessageSpec {
    MessageSpec::builder()
        .sender("flotilla/checks".into())
        .receiver(RECEIVER.into())
        .relation(MessageRelation::System)
        .body("review the settled checks".into())
        .expectation(expectation)
        .build()
}

async fn home() -> EngineMessagesHome {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    EngineMessagesHome::open("home", NAMESPACE, &backend)
}

/// The state of message `record` in the receiver's inbox.
async fn state(primitives: &MessagePrimitives, record: &str) -> MessageState {
    let inbox = primitives.inbox_for(NAMESPACE, RECEIVER).await.expect("inbox");
    let (id, _) = primitives.locate_in(&inbox, record).expect("locate").expect("admitted");
    primitives.message(&id).await.expect("load").expect("held").state().clone()
}

/// How many messages the receiver's inbox holds, in every state.
async fn held(primitives: &MessagePrimitives) -> usize {
    let inbox = primitives.inbox_for(NAMESPACE, RECEIVER).await.expect("inbox");
    primitives.inbox_messages(&inbox).expect("messages").len()
}

fn superseded(state: &MessageState) -> bool {
    matches!(state, MessageState::Dropped { reason: DropReason::Superseded { .. }, .. })
}

// Behaviour (ported from messaging's explicit_supersession_and_creation_retries_are_scoped_and_idempotent):
// a name's repeat is the message already admitted, and another intent under it is refused; a
// message naming its predecessor replaces it whoever sends it; only a supervisor interrupts.
#[tokio::test]
async fn explicit_supersession_and_creation_retries_are_scoped_and_idempotent() {
    let home = home().await;
    let primitives = home.primitives();
    let original = spec(MessageExpectation::None);
    let first = primitives.admit_old(NAMESPACE, "original", &original).await.expect("create");
    let retry = primitives.admit_old(NAMESPACE, "original", &original).await.expect("retry");
    assert_eq!((first.record.as_str(), retry.record.as_str()), ("original", "original"));
    assert_eq!(held(primitives).await, 1, "a repeat admits nothing more");
    let mut replacement = original.clone();
    replacement.body = "different".into();
    assert!(primitives.admit_old(NAMESPACE, "original", &replacement).await.is_err(), "another intent under the name");
    replacement.supersedes = Some("original".into());
    replacement.sender = "flotilla/review".into();
    let successor = primitives.admit_old(NAMESPACE, "replacement", &replacement).await.expect("explicit successor");
    assert_eq!(successor.record, "replacement");
    assert!(superseded(&state(primitives, "original").await));
    replacement.interrupting = true;
    assert!(primitives.admit_old(NAMESPACE, "interrupt", &replacement).await.is_err(), "an interrupt from no supervisor");
}

// Behaviour (ported from messaging's explicit_supersession_replaces_open_expectations): a message
// naming a delivered predecessor that awaits its answer replaces it, under the successor's own
// name. One that awaited nothing ended at delivery and stays settled.
#[tokio::test]
async fn explicit_supersession_replaces_open_expectations() {
    for expectation in [MessageExpectation::Reply, MessageExpectation::None] {
        let home = home().await;
        let primitives = home.primitives();
        let original = spec(expectation.clone());
        primitives.admit_old(NAMESPACE, "original", &original).await.expect("original admission");
        home.deliver_turn(RECEIVER, "original", "session").await;
        // A delivered message that awaits nothing has ended with its delivery.
        let awaits = !matches!(expectation, MessageExpectation::None);
        let delivered = state(primitives, "original").await;
        assert_eq!(matches!(delivered, MessageState::Awaiting { .. }), awaits, "{delivered:?}");
        let mut replacement = original;
        replacement.supersedes = Some("original".into());
        let admitted = primitives.admit_old(NAMESPACE, "replacement", &replacement).await.expect("explicit replacement");
        assert_eq!(admitted.record, "replacement", "{expectation:?}");
        let after = state(primitives, "original").await;
        assert!(if awaits { superseded(&after) } else { matches!(after, MessageState::Settled { .. }) }, "{after:?}");
    }
}

// Behaviour (ported from messaging's indexed_message_history_and_retention_contract, its replay):
// a producer's repeat of a name whose message has ended and been archived is taken as admitted,
// and admits nothing again.
#[tokio::test]
async fn a_name_admitted_again_after_its_message_is_archived_admits_nothing() {
    let home = home().await;
    let primitives = home.primitives();
    let original = spec(MessageExpectation::None);
    primitives.admit_old(NAMESPACE, "history", &original).await.expect("admit");
    let mut successor = original.clone();
    successor.supersedes = Some("history".into());
    primitives.admit_old(NAMESPACE, "successor", &successor).await.expect("supersede");
    assert!(superseded(&state(primitives, "history").await));
    let inbox = primitives.inbox_for(NAMESPACE, RECEIVER).await.expect("inbox");
    let (id, _) = primitives.locate_in(&inbox, "history").expect("locate").expect("admitted");

    // Ship the ended message, then delete and expunge it, as the daemon's archive pass does.
    let mut archiver = Archiver::new("home", ArchiveKinds::new().with::<Message>());
    let later = Timestamp(i64::MAX / 2);
    home.service()
        .with_store(|store| {
            let archive = |error| flotilla_schema_sqlite::StoreError::Stream(format!("{error}"));
            archiver.refresh(store).map_err(archive)?;
            let due = archiver.due(later, 0);
            let [Due::Ship(identity)] = due.as_slice() else { panic!("the ended message is due: {due:?}") };
            assert!(identity.name.ends_with(&id) || id.ends_with(&identity.name), "{identity:?} is {id}");
            let Prepared::Ready { version, .. } = archiver.prepare(store, identity).map_err(archive)? else { panic!("ready") };
            archiver.delete(store, identity, version, later).map_err(archive)?;
            archiver.expunge(store, identity, Timestamp(later.0 + 1)).map_err(archive)?;
            Ok(())
        })
        .expect("archive");
    assert!(primitives.message(&id).await.expect("load").is_none(), "no longer held here");

    let again = primitives.admit_old(NAMESPACE, "history", &original).await.expect("the repeat is taken as admitted");
    assert_eq!(again.record, "history");
    assert!(primitives.message(&id).await.expect("load").is_none(), "nothing admitted again");
    assert_eq!(held(primitives).await, 1, "only the successor");
}
