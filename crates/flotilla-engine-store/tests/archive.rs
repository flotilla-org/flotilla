//! Archiving ended resources (#3939): selection from the home's log, the bundle and its replay,
//! and removal in two writes that followers tail without a fresh snapshot.

use std::sync::{Arc, Mutex};

use flotilla_engine_store::{
    archive::{bundle_key, ArchiveKinds, ArchiveNote, Archiver, Bundle, Due, Prepared, Stage, ARCHIVE_AFTER_MICROS, ARCHIVE_WRITER},
    registry, MessageLog,
};
use flotilla_schema::{Address, Cause, Identity, Kind, Timestamp};
use flotilla_schema_sqlite::{
    stream::{self, Follower, Frame},
    Store,
};
use flotilla_state_machines::{
    machines::{
        agent_session::AgentSession,
        message::{DropMessage, DropReason, Expectation, InboxBinding, MemoryMessageWorld, Message, MessageBody, MessageSpec, Relation},
    },
    Home, Ref, Resource,
};
use rusqlite::Connection;

const HOME: &str = "alpha";
const DROPPED_AT: Timestamp = Timestamp(50);
const GRACE: i64 = 10 * 60 * 1_000_000;

fn store(home: &str) -> Store {
    Store::open(Connection::open_in_memory().expect("SQLite"), home, registry().expect("registry")).expect("store")
}

fn inbox() -> Address {
    "project:fleet/role:infra".parse().expect("inbox")
}

fn name(id: &str) -> String {
    Address::message(&inbox(), id).expect("message address").as_str().to_owned()
}

fn spec() -> MessageSpec {
    MessageSpec {
        expectation: Expectation::Nothing,
        unbound_after: 10,
        ..MessageSpec::new(
            "project:fleet/role:turn-rules".parse().expect("sender"),
            inbox(),
            Relation::System,
            MessageBody::text("a standing-inbox notice").expect("small body"),
        )
    }
}

fn kinds() -> ArchiveKinds {
    ArchiveKinds::new().with::<Message>().with::<AgentSession>()
}

fn identity(name: &str) -> Identity {
    Identity { kind: Resource::<Message>::CODE, name: name.to_owned(), home: HOME.to_owned() }
}

/// Follows `source`'s home from `after` into `target`; whether the round sent a fresh snapshot.
fn follow(source: &mut Store, target: &mut Store, after: Option<u64>) -> bool {
    let mut snapshot = false;
    Follower::new(target)
        .receive(HOME, |sink| {
            stream::serve(source, after, Timestamp(1), |bytes| {
                snapshot |= matches!(Frame::decode(bytes)?, Frame::Begin { .. });
                sink(bytes)
            })
            .map(|_| ())
        })
        .expect("follow");
    snapshot
}

// Behaviour (#3939, done-when): a standing inbox's message that dropped is shipped a week after
// the drop, not before. Its bundle holds its end state and every entry, and replaying the entries
// into another home's store rebuilds that state. Once shipped it is deleted, then expunged a pass
// later, so a follower that applied the delete tails the expunge without a fresh snapshot, and
// only the expunge entry, naming the bundle, stays. An open message beside it is untouched, and a
// restart between the two writes resumes at the expunge.
#[test]
fn a_dropped_message_ships_a_week_later_and_leaves_in_two_writes() {
    let shared = Arc::new(Mutex::new(store(HOME)));
    let mut messages = Home::<Message, MessageLog>::with_log(HOME, MessageLog::new(shared.clone())).expect("message home");
    let mut world = MemoryMessageWorld::default();
    world.binding.put(&Ref::new(inbox().as_str()), InboxBinding::Unbound { since: Timestamp(0) });
    let (dropped, open) = (name("dropped"), name("open"));
    messages.create(&dropped, spec(), &world, Timestamp(1)).expect("create");
    messages.create(&open, spec(), &world, Timestamp(2)).expect("create");
    messages.apply(&dropped, DropMessage { reason: DropReason::Undeliverable }, &world, DROPPED_AT).expect("drop");

    let mut follower = store("beta");
    let mut store = shared.lock().expect("store");
    assert!(follow(&mut store, &mut follower, None), "the first round is a snapshot");
    let mut cursor = follower.read().expect("read").watermark(HOME).expect("watermark").home_seq;

    let mut archiver = Archiver::new(HOME, kinds());
    archiver.refresh(&mut store).expect("refresh");
    let candidates: Vec<_> = archiver.candidates().cloned().collect();
    assert_eq!(candidates.len(), 1, "only the dropped message: {candidates:?}");
    assert_eq!((candidates[0].identity.clone(), candidates[0].ended_at), (identity(&dropped), DROPPED_AT));
    let due = DROPPED_AT.saturating_add(ARCHIVE_AFTER_MICROS);
    assert!(archiver.due(Timestamp(due.0 - 1), GRACE).is_empty(), "not before a week");
    assert_eq!(archiver.due(due, GRACE), [Due::Ship(identity(&dropped))]);

    let Prepared::Ready { bundle, version } = archiver.prepare(&mut store, &identity(&dropped)).expect("prepare") else {
        panic!("an ended message is ready")
    };
    let stored = {
        let mut read = store.read().expect("read");
        let id = read.resolve(Resource::<Message>::CODE, &dropped, HOME).expect("resolve").expect("stored");
        read.load_snapshot(id).expect("snapshot").expect("body")
    };
    assert_eq!((bundle.snapshot.clone(), bundle.identity(), version), (stored.clone(), identity(&dropped), stored.version));
    assert_eq!(bundle.entries.iter().map(|entry| entry.cause.name()).collect::<Vec<_>>(), ["create", "edge"]);
    let bytes = bundle.encode().expect("encode");
    assert_eq!(Bundle::decode(&bytes).expect("decode"), bundle);
    let mut elsewhere = self::store("gamma");
    assert_eq!(bundle.replay(&mut elsewhere).expect("replay"), Some(stored), "the entries rebuild the end state");

    let note = ArchiveNote { key: bundle_key(&identity(&dropped)), digest: "0".repeat(64) };
    let deleted_at = due.saturating_add(1);
    archiver.delete(&mut store, &identity(&dropped), version, &note, deleted_at).expect("delete");
    archiver.refresh(&mut store).expect("refresh");
    assert_eq!(
        archiver.candidates().next().map(|candidate| &candidate.stage),
        Some(&Stage::Deleted { at: deleted_at, note: note.clone() })
    );
    assert!(archiver.due(Timestamp(deleted_at.0 + GRACE - 1), GRACE).is_empty(), "the expunge waits a pass");
    assert!(!follow(&mut store, &mut follower, Some(cursor)), "the delete is tailed");
    cursor = follower.read().expect("read").watermark(HOME).expect("watermark").home_seq;

    let mut restarted = Archiver::new(HOME, kinds());
    restarted.refresh(&mut store).expect("startup scan");
    let expunge_at = deleted_at.saturating_add(GRACE);
    assert_eq!(restarted.due(expunge_at, GRACE), [Due::Expunge(identity(&dropped), note.clone())], "a restart resumes at the expunge");
    assert!(restarted.expunge(&mut store, &identity(&dropped), &note, expunge_at).expect("expunge"));
    restarted.refresh(&mut store).expect("refresh");
    assert_eq!(restarted.candidates().count(), 0);

    let floor = store.read().expect("read").resume_floor(HOME).expect("floor");
    assert!(floor <= cursor, "the follower's cursor is servable: floor {floor}, cursor {cursor}");
    assert!(!follow(&mut store, &mut follower, Some(cursor)), "the expunge is tailed");
    for host in [&mut *store, &mut follower] {
        let mut read = host.read().expect("read");
        assert!(read.resolve(Resource::<Message>::CODE, &dropped, HOME).expect("resolve").is_none(), "expunged");
        assert!(read.resolve(Resource::<Message>::CODE, &open, HOME).expect("resolve").is_some(), "the open message stays");
        let entries = read.changes_since(HOME, 0, 64).expect("log");
        let tombstone = entries.iter().find(|entry| entry.cause == Cause::Expunge).expect("expunge entry");
        assert_eq!(tombstone.effect.expunged, Some(identity(&dropped)));
        assert_eq!(tombstone.writer, ARCHIVE_WRITER);
        let logged: ArchiveNote =
            flotilla_cbor::from_slice(tombstone.payload.as_deref().expect("note"), &flotilla_schema::INLINE_LIMITS).expect("note");
        assert_eq!(logged, note);
        assert!(!entries.iter().any(|entry| entry.name == dropped && entry.cause != Cause::Expunge), "its history went with it");
    }
}

// Bundle keys name the identity, escaping everything outside the URI-unreserved characters so
// any name maps to one key and back.
#[test]
fn bundle_keys_are_derived_from_the_identity() {
    let message = identity(&name("dropped"));
    assert_eq!(bundle_key(&message), format!("archive/alpha/ms/{}", name("dropped").replace(':', "%3A").replace('/', "%2F")));
    let other = Identity { home: "alpha/beta".into(), ..message.clone() };
    assert_ne!(bundle_key(&other), bundle_key(&message));
    assert!(bundle_key(&other).starts_with("archive/alpha%2Fbeta/"));
}
