//! Fixture kinds, transcribed from the real resource types rather than imported, and helpers.

pub mod message;

use chrono::{DateTime, Utc};
use flotilla_schema::{KindRegistry, Timestamp};
use flotilla_schema_sqlite::Store;
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;

pub use self::message::{AgentSession, Inbox, Message};

pub const LOCAL: &str = "local";

pub fn registry() -> KindRegistry {
    let mut registry = KindRegistry::new();
    registry.register::<Message>().expect("fixture kinds register");
    registry.register::<Inbox>().expect("fixture kinds register");
    registry.register::<AgentSession>().expect("fixture kinds register");
    registry
}

pub fn store() -> Store {
    Store::open(Connection::open_in_memory().expect("in-memory database"), LOCAL, registry()).expect("open store")
}

/// Text that spans the empty string, ASCII, multi-byte characters and SQL quoting characters.
pub fn text(tc: &TestCase) -> String {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
        0 => tc.draw(gs::text().max_size(12)),
        1 => {
            tc.draw(gs::sampled_from(vec![String::new(), "'".to_owned(), "\"; DROP TABLE message; --".to_owned(), "ünïcødé ✓".to_owned()]))
        }
        _ => tc.draw(gs::text().alphabet("ab").max_size(3)),
    }
}

/// A time with sub-microsecond digits, before or after the epoch.
pub fn time(tc: &TestCase) -> DateTime<Utc> {
    let seconds = tc.draw(gs::integers::<i64>().min_value(-2_000_000_000).max_value(4_000_000_000));
    let nanos = tc.draw(gs::integers::<u32>().min_value(0).max_value(999_999_999));
    DateTime::from_timestamp(seconds, nanos).expect("in range")
}

/// What a stored time loads back as: floored to whole microseconds.
pub fn floor(time: DateTime<Utc>) -> DateTime<Utc> {
    Timestamp::from_datetime(&time).to_datetime().expect("in range")
}
