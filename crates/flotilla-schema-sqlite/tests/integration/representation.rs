use chrono::DateTime;
use flotilla_schema::EncodeError;
use flotilla_schema_sqlite::StoreError;

use crate::fixtures::{store, Message};

fn message(accepted_seq: u64) -> Message {
    Message { body: "b".into(), body_digest: None, interrupting: true, accepted_seq, deadline: None, ordinal: u16::MAX }
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
