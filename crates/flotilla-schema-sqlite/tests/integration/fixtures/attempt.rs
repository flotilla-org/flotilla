//! The shared attempt and failure-cause shapes, transcribed from the #3108 prototype's
//! `attempt.rs` (`Attempt`, `Outcome`, `Cause`, `CauseCategory`). `Attempt` is generic over what
//! was tried, as the #3208 sketches use it (`Attempt<TerminalAction>`); the prototype's own shape
//! is `Attempt<String>`.

use flotilla_schema::{Column, Stored, Timestamp};
use hegel::TestCase;
use serde::{Deserialize, Serialize};

use super::{code_enum, text, timestamp, Pick};

/// Stored as columns under an attempt's outcome, and inside edge documents (serde).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct Cause {
    pub category: CauseCategory,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum CauseCategory {
    #[store(code = 1)]
    Transient,
    #[store(code = 2)]
    Refused,
    #[store(code = 3)]
    Misconfigured,
    #[store(code = 4)]
    MissingCapability,
    #[store(code = 5)]
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub enum Outcome {
    #[store(code = 1)]
    Success,
    #[store(code = 2)]
    Failure {
        #[store(flatten)]
        cause: Cause,
        retryable: bool,
    },
}

/// What was tried, its target, when, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct Attempt<A> {
    pub what: A,
    pub target: String,
    pub at: Timestamp,
    pub outcome: Outcome,
}

impl Cause {
    pub fn generate(tc: &TestCase, pick: Pick) -> Cause {
        use CauseCategory::*;
        Cause { category: code_enum(tc, pick, &[Transient, Refused, Misconfigured, MissingCapability, External]), detail: text(tc) }
    }
}

impl<A: Column> Attempt<A> {
    /// Both outcomes in turn when covering, every cause category across rounds.
    pub fn generate(tc: &TestCase, pick: Pick, what: A) -> Attempt<A> {
        let outcome = match pick.variant(tc, 2, |index| index) {
            0 => Outcome::Success,
            _ => Outcome::Failure { cause: Cause::generate(tc, pick), retryable: pick.some(tc, 8) },
        };
        Attempt { what, target: text(tc), at: timestamp(tc), outcome }
    }
}
