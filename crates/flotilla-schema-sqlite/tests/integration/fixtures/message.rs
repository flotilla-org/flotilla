//! The Message-shaped fixture. Layer 1 carries only the scalar fields of
//! `flotilla_resources::message::MessageSpec` (`body`, `body_digest`, `interrupting`, `deadline`),
//! the accepted sequence from its status, and an `ordinal` for a narrow integer. Later layers add
//! the interned sender, references, expectation, state, edges and attempts.

use chrono::{DateTime, Utc};
use flotilla_schema::Stored;
use hegel::{generators as gs, TestCase};

use super::{floor, text, time};

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Message", code = 1)]
pub struct Message {
    pub body: String,
    pub body_digest: Option<String>,
    pub interrupting: bool,
    pub accepted_seq: u64,
    pub deadline: Option<DateTime<Utc>>,
    pub ordinal: u16,
}

impl Message {
    /// Spans every field's boundaries: empty and quoting text, absent and present options, the
    /// whole storable `u64` range (0 and `i64::MAX` included), times with sub-microsecond digits
    /// either side of the epoch, and the whole `u16` range.
    pub fn generate(tc: &TestCase) -> Message {
        Message {
            body: text(tc),
            body_digest: if tc.draw(gs::booleans()) { Some(text(tc)) } else { None },
            interrupting: tc.draw(gs::booleans()),
            accepted_seq: tc.draw(gs::integers::<u64>().min_value(0).max_value(i64::MAX as u64)),
            deadline: if tc.draw(gs::booleans()) { Some(time(tc)) } else { None },
            ordinal: tc.draw(gs::integers::<u16>()),
        }
    }

    /// The value a load returns: times floored to microseconds.
    pub fn stored(&self) -> Message {
        Message { deadline: self.deadline.map(floor), ..self.clone() }
    }
}
