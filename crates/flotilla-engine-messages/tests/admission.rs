//! Admission through the Message path (#3108), ported from messaging's old-inbox admission tests:
//! a name's repeat, a different intent under a name, explicit supersession, and a
//! name admitted again after its message was archived.

use flotilla_engine_messages::MessagePrimitives;
use flotilla_engine_messages_testkit::{designate_fleet, party, EngineMessagesHome};
use flotilla_engine_store::archive::{ArchiveKinds, Archiver, Due, Prepared};
use flotilla_messaging::{Intent, IntentExpectation};
use flotilla_resources::MessageRelation;
use flotilla_schema::Timestamp;
use flotilla_state_machines::machines::message::{DropReason, Message, MessageState};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const RECEIVER: &str = "flotilla/convoy/work/coder";

fn spec(expectation: IntentExpectation) -> Intent {
    let mut intent =
        flotilla_engine_messages_testkit::intent("flotilla/checks", RECEIVER, MessageRelation::System, "review the settled checks");
    intent.expectation = expectation;
    intent
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
// message naming its predecessor replaces it whoever sends it.
#[tokio::test]
async fn explicit_supersession_and_creation_retries_are_scoped_and_idempotent() {
    let home = home().await;
    let primitives = home.primitives();
    let original = spec(IntentExpectation::Nothing);
    let first = primitives.admit("original", &original).await.expect("create");
    let retry = primitives.admit("original", &original).await.expect("retry");
    assert_eq!((first.record.as_str(), retry.record.as_str()), ("original", "original"));
    assert_eq!(held(primitives).await, 1, "a repeat admits nothing more");
    let mut replacement = original.clone();
    replacement.body = "different".into();
    assert!(primitives.admit("original", &replacement).await.is_err(), "another intent under the name");
    replacement.supersedes = Some("original".into());
    replacement.sender = party("flotilla/review");
    let successor = primitives.admit("replacement", &replacement).await.expect("explicit successor");
    assert_eq!(successor.record, "replacement");
    assert!(superseded(&state(primitives, "original").await));
}

// Behaviour (ported from messaging's explicit_supersession_replaces_open_expectations): a message
// naming a delivered predecessor that awaits its answer replaces it, under the successor's own
// name. One that awaited nothing ended at delivery and stays settled.
#[tokio::test]
async fn explicit_supersession_replaces_open_expectations() {
    for expectation in [IntentExpectation::Reply, IntentExpectation::Nothing] {
        let home = home().await;
        let primitives = home.primitives();
        let original = spec(expectation.clone());
        primitives.admit("original", &original).await.expect("original admission");
        home.deliver_turn(RECEIVER, "original", "session").await;
        // A delivered message that awaits nothing has ended with its delivery.
        let awaits = !matches!(expectation, IntentExpectation::Nothing);
        let delivered = state(primitives, "original").await;
        assert_eq!(matches!(delivered, MessageState::Awaiting { .. }), awaits, "{delivered:?}");
        let mut replacement = original;
        replacement.supersedes = Some("original".into());
        let admitted = primitives.admit("replacement", &replacement).await.expect("explicit replacement");
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
    let original = spec(IntentExpectation::Nothing);
    primitives.admit("history", &original).await.expect("admit");
    let mut successor = original.clone();
    successor.supersedes = Some("history".into());
    primitives.admit("successor", &successor).await.expect("supersede");
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

    let again = primitives.admit("history", &original).await.expect("the repeat is taken as admitted");
    assert_eq!(again.record, "history");
    assert!(primitives.message(&id).await.expect("load").is_none(), "nothing admitted again");
    assert_eq!(held(primitives).await, 1, "only the successor");
}

// Behaviour (#3108): a message is admitted only from its sender's intent. One written in the old
// shape through the generic resource verbs is refused, and nothing is admitted.
#[tokio::test]
async fn an_old_shaped_message_written_through_the_resource_verbs_is_refused() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let home = EngineMessagesHome::open("home", NAMESPACE, &backend);
    let old = flotilla_resources::MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver(RECEIVER.into())
        .relation(MessageRelation::System)
        .body("written in the old shape".into())
        .build();
    let meta = flotilla_resources::InputMeta::builder().name("old-shape".into()).build();
    let refused = backend.using::<flotilla_resources::Message>(NAMESPACE).create(&meta, &old).await.expect_err("refused");
    assert!(refused.to_string().contains("sender's intent"), "{refused}");
    assert_eq!(held(home.primitives()).await, 0, "nothing admitted");
}
