use chrono::DateTime;
use flotilla_schema::{EdgeEvent, EncodeError, Kind, Records, Ref, Timestamp};
use flotilla_schema_sqlite::StoreError;

use crate::fixtures::{
    agent_session, store,
    synthetic::{AgentName, Phase, Reference, ReferralEdge, ReferralEvent},
    AgentSessionRow, Payload, Referral, LOCAL,
};

fn referral(seq: u64) -> Referral {
    Referral { seq, ordinal: u16::MAX, ..Referral::sample() }
}

// Behaviour (#3320 representation): a u64 above i64::MAX is refused with a typed error naming
// the column, never wrapped; i64::MAX itself is stored and loads back.
#[test]
fn u64_above_i64_max_is_refused() {
    let mut store = store();
    let mut write = store.write().expect("write");
    assert!(matches!(
        write.insert("max", &referral(u64::MAX)),
        Err(StoreError::Encode(EncodeError::OutOfRange { column, value })) if column == "seq" && value == u64::MAX
    ));
    assert!(matches!(write.insert("over", &referral(i64::MAX as u64 + 1)), Err(StoreError::Encode(EncodeError::OutOfRange { .. }))));
    let stamp = write.insert("edge", &referral(i64::MAX as u64)).expect("i64::MAX fits");
    write.commit().expect("commit");
    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Referral>(stamp.id).expect("load").expect("present").value.seq, i64::MAX as u64);
}

// Behaviour (#3320 representation): timestamps are stored as INTEGER microseconds, floored, so a
// load returns the truncated time (towards the past, also before the epoch).
#[test]
fn timestamps_truncate_to_microseconds() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let after = DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("in range");
    let before = DateTime::from_timestamp(-1, 999_999_999).expect("in range");
    let late = write.insert("late", &Referral { sent_at: Some(after), ..referral(1) }).expect("insert");
    let early = write.insert("early", &Referral { sent_at: Some(before), ..referral(1) }).expect("insert");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    let late = read.load::<Referral>(late.id).expect("load").expect("present").value.sent_at;
    assert_eq!(late, Some(DateTime::from_timestamp(1_700_000_000, 123_456_000).expect("in range")));
    let early = read.load::<Referral>(early.id).expect("load").expect("present").value.sent_at;
    assert_eq!(early, Some(DateTime::from_timestamp(-1, 999_999_000).expect("in range")));
    drop(read);

    let conn = store.into_connection();
    let stored: i64 = conn.query_row("SELECT sent_at FROM referral ORDER BY id LIMIT 1", [], |row| row.get(0)).expect("raw");
    assert_eq!(stored, 1_700_000_000_123_456);
}

// Behaviour (#3320 references): a reference may precede its target. Writing it creates the
// target's identity row, and inserting the target later (here an engine kind, the session)
// takes that same identity.
#[test]
fn references_to_missing_targets_create_their_identity() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let value = Referral { session: Ref::new("later", LOCAL), in_reply_to: Some(Ref::new("gone", "elsewhere")), ..Referral::sample() };
    let stamp = write.insert("m", &value).expect("insert");
    let session = write.resolve(AgentSessionRow::CODE, "later", LOCAL).expect("resolve").expect("identity created by the reference");
    let reply = write.resolve(Referral::CODE, "gone", "elsewhere").expect("resolve").expect("identity created by the reference");
    assert_eq!(write.load::<Referral>(reply).expect("load"), None, "an identity is not a resource");
    let target = write.insert("later", &agent_session::sample()).expect("target inserted after the reference");
    assert_eq!(target.id, session);
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Referral>(stamp.id).expect("load").expect("present").value, value);
    drop(read);
    let conn = store.into_connection();
    let stored: (i64, i64) = conn
        .query_row("SELECT session, in_reply_to FROM referral WHERE id = ?1", [stamp.id.0], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("raw");
    assert_eq!(stored, (session.0, reply.0));
}

// Behaviour (#3320 interning): equal interned strings share one `strings` row, whichever field,
// variant, table or resource holds them.
#[test]
fn equal_interned_strings_share_one_row() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let event: ReferralEvent = EdgeEvent {
        edge: ReferralEdge::Send,
        from: Phase::Draft,
        to: Phase::Sent,
        writer: "carol".into(),
        at: Timestamp(1),
        payload: Payload { args: String::new(), reads: Vec::new() },
    };
    let reference = Reference::ChangeRequest { service: "carol".into(), scope: "s".into(), number: 1, revision: "r".into() };
    for name in ["a", "b"] {
        let mut edges = Records::new();
        edges.append(0, event.clone());
        let value = Referral { sender: AgentName("carol".into()), references: vec![reference.clone()], edges, ..Referral::sample() };
        write.insert(name, &value).expect("insert");
    }
    write.commit().expect("commit");
    let conn = store.into_connection();
    let rows: i64 = conn.query_row("SELECT count(*) FROM strings WHERE s = 'carol'", [], |row| row.get(0)).expect("count");
    assert_eq!(rows, 1);
    let distinct: i64 = conn
        .query_row(
            "SELECT count(DISTINCT m.sender) + count(DISTINCT r.service) + count(DISTINCT e.writer) \
             FROM referral m JOIN referral_edges e ON e.owner = m.id JOIN referral_references r ON r.owner = m.id",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(distinct, 3, "every column of every row names the one row");
    let same: i64 = conn
        .query_row(
            "SELECT count(*) FROM referral m JOIN referral_edges e ON e.owner = m.id JOIN referral_references r ON r.owner = m.id \
             WHERE m.sender = r.service AND m.sender = e.writer",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(same, 2);
}
