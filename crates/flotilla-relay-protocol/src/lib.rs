//! Wire contract shared by the event relay and its consumers (ADR 0041).
//!
//! The relay reduces producer events to [`Hint`]s, retains the latest hint per subject, and
//! streams them to consumers as [`StreamFrame`]s. Consumers acknowledge with [`ConsumerFrame`]s.
//! Installs are provisioned through the operator-authenticated [`admin`] API.
pub mod admin;
pub mod github;
mod subject;

use serde::{Deserialize, Serialize};

pub use crate::subject::{Subject, SubjectKind, SubjectParseError};

/// A reduced producer event: "something about `subject` changed". Never carries forge content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hint {
    pub source: String,
    pub subject: String,
    pub kind: String,
    pub delivery_id: String,
}

/// A hint together with the mailbox cursor it was stored at.
///
/// Cursors increase strictly within one install's mailbox. A subject keeps only its latest
/// delivery, so a consumer resuming from a cursor sees each changed subject once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub cursor: u64,
    pub hint: Hint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamFrame {
    Hint {
        delivery: Delivery,
    },
    /// The consumer's cursor is behind the pruned horizon: a subject it has not seen may have
    /// been dropped. The consumer must refresh everything it demands, then resume from
    /// `latest_cursor`. `oldest_cursor` is the oldest retained delivery, absent when none remain.
    Gap {
        oldest_cursor: Option<u64>,
        latest_cursor: u64,
    },
    Acked {
        cursor: u64,
    },
    /// Sent on websocket connect when the consumer is already caught up.
    Ready {
        cursor: u64,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConsumerFrame {
    Ack { cursor: u64 },
}

/// A producer the relay has an adapter for. Its path segment in `POST /i/<install>/<source>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Github,
}

impl Source {
    pub const ALL: [Source; 1] = [Source::Github];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.as_str() == name)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
        }
    }
}
