use chrono::DateTime;
use flotilla_schema::{EncodeError, Identity, Kind, KindCode, Ref};
use flotilla_schema_sqlite::StoreError;

use crate::fixtures::{
    message::{DropReason, MessageEdge},
    store, Inbox, Message, LOCAL,
};

fn message(accepted_seq: u64) -> Message {
    Message { interrupting: true, accepted_seq, ordinal: u16::MAX, ..Message::sample() }
}

// Behaviour (#3320 representation): a u64 above i64::MAX is refused with a typed error naming
// the column, never wrapped; i64::MAX itself is stored and loads back.
#[test]
fn u64_above_i64_max_is_refused() {
    let mut store = store();
    let mut write = store.write().expect("write");
    assert!(matches!(
        write.insert("max", &message(u64::MAX)),
        Err(StoreError::Encode(EncodeError::OutOfRange { column, value })) if column == "accepted_seq" && value == u64::MAX
    ));
    assert!(matches!(write.insert("over", &message(i64::MAX as u64 + 1)), Err(StoreError::Encode(EncodeError::OutOfRange { .. }))));
    let stamp = write.insert("edge", &message(i64::MAX as u64)).expect("i64::MAX fits");
    write.commit().expect("commit");
    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Message>(stamp.id).expect("load").expect("present").value.accepted_seq, i64::MAX as u64);
}

// Behaviour (#3320 representation): timestamps are stored as INTEGER microseconds, floored, so a
// load returns the truncated time (towards the past, also before the epoch).
#[test]
fn timestamps_truncate_to_microseconds() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let after = DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("in range");
    let before = DateTime::from_timestamp(-1, 999_999_999).expect("in range");
    let late = write.insert("late", &Message { deadline: Some(after), ..message(1) }).expect("insert");
    let early = write.insert("early", &Message { deadline: Some(before), ..message(1) }).expect("insert");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    let late = read.load::<Message>(late.id).expect("load").expect("present").value.deadline;
    assert_eq!(late, Some(DateTime::from_timestamp(1_700_000_000, 123_456_000).expect("in range")));
    let early = read.load::<Message>(early.id).expect("load").expect("present").value.deadline;
    assert_eq!(early, Some(DateTime::from_timestamp(-1, 999_999_000).expect("in range")));
    drop(read);

    let conn = store.into_connection();
    let stored: i64 = conn.query_row("SELECT deadline FROM message ORDER BY id LIMIT 1", [], |row| row.get(0)).expect("raw");
    assert_eq!(stored, 1_700_000_000_123_456);
}

// Behaviour (#3320 references): a reference may precede its target. Writing it creates the
// target's identity row, and inserting the target later takes that same identity.
#[test]
fn references_to_missing_targets_create_their_identity() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let message = Message { inbox: Ref::new("later", LOCAL), in_reply_to: Some(Ref::new("gone", "elsewhere")), ..Message::sample() };
    let stamp = write.insert("m", &message).expect("insert");
    let inbox = write.resolve(Inbox::CODE, "later", LOCAL).expect("resolve").expect("identity created by the reference");
    let reply = write.resolve(Message::CODE, "gone", "elsewhere").expect("resolve").expect("identity created by the reference");
    assert_eq!(write.load::<Message>(reply).expect("load"), None, "an identity is not a resource");
    let target = write.insert("later", &Inbox { role: "r".into() }).expect("target inserted after the reference");
    assert_eq!(target.id, inbox);
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Message>(stamp.id).expect("load").expect("present").value, message);
    drop(read);
    let conn = store.into_connection();
    let stored: (i64, i64) = conn
        .query_row("SELECT inbox, in_reply_to FROM message WHERE id = ?1", [stamp.id.0], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("raw");
    assert_eq!(stored, (inbox.0, reply.0));
}

// Behaviour (#3320 interning): equal interned strings share one `strings` row, whichever field,
// variant or resource holds them.
#[test]
fn equal_interned_strings_share_one_row() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let edge = MessageEdge::Drop {
        reason: DropReason::Withdrawn {
            by: "carol".into(),
            authority: Identity { kind: KindCode(9), name: "x".into(), home: LOCAL.into() },
        },
    };
    for name in ["a", "b"] {
        write.insert(name, &Message { sender: "carol".into(), last_edge: Some(edge.clone()), ..Message::sample() }).expect("insert");
    }
    write.commit().expect("commit");
    let conn = store.into_connection();
    let rows: i64 = conn.query_row("SELECT count(*) FROM strings WHERE s = 'carol'", [], |row| row.get(0)).expect("count");
    assert_eq!(rows, 1);
    let distinct: i64 = conn
        .query_row("SELECT count(DISTINCT sender) + count(DISTINCT last_edge_drop_reason_withdrawn_by) FROM message", [], |row| row.get(0))
        .expect("count");
    assert_eq!(distinct, 2, "both columns of both rows name the one row");
    let same: i64 = conn
        .query_row("SELECT count(*) FROM message WHERE sender = last_edge_drop_reason_withdrawn_by", [], |row| row.get(0))
        .expect("count");
    assert_eq!(same, 2);
}
