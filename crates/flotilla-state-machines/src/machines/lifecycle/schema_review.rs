//! Stored-shape declarations for review before the running machine or its consumers change.
//! An absent ending represents preceding-generation information that cannot be recovered;
//! new lifecycle transitions must always supply a known ending.

use flotilla_schema::Stored;

use super::LifecycleLabel;
use crate::{Cause, Failure};

/// How a realised resource disappeared.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum VanishCause {
    #[store(code = "lo")]
    Lost,
    #[store(code = "fa")]
    Failed { reason: String },
}

/// How a resource ended after release or disappearance.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Ending {
    #[store(code = "rl")]
    Released,
    #[store(code = "vn")]
    Vanished { cause: VanishCause },
}

/// Candidate storage representation, deliberately not the running machine's state.
/// `None` in Gone preserves an unknowable historical ending, not a new model transition.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(labels = LifecycleLabel)]
pub enum LifecycleState {
    #[store(code = "rq")]
    Requested {
        failures: u32,
        #[store(flatten)]
        last_failure: Option<Failure>,
    },
    #[store(code = "rl")]
    Realised { identity: String },
    #[store(code = "go")]
    Gone { ending: Option<Ending> },
    #[store(code = "gu")]
    GaveUp {
        #[store(flatten)]
        cause: Cause,
    },
}
