//! Attempts are edges, judged from the state they leave.
//!
//! An attempt is an edge on the resource that made it (R4, G10), carrying
//! its [`Outcome`]: success, or a failure with its cause and whether it may
//! be retried. Its log entry is the history; its effect keeps the current
//! window's tally in the state data, the consecutive failures since the last
//! success and the latest [`Failure`], so the decision path reads the state
//! and never the log. A machine with windows opens a new one by clearing the
//! tally (Ensure's `resume`). The retry count, the schedule and exhaustion
//! are judgements over that tally and a policy; a listing and a hold or
//! give-up gate call the same [`retry`], so they cannot disagree.

use flotilla_schema::{Stored, Timestamp};

use serde::{Deserialize, Serialize};

use crate::{Handle, Machine};

/// What kind of failure, shared across kinds; the detail is per kind. Stored
/// by code, so a rename keeps every stored category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub enum CauseCategory {
    #[store(code = "tr")]
    Transient,
    #[store(code = "rf")]
    Refused,
    #[store(code = "mc")]
    Misconfigured,
    #[store(code = "mx")]
    MissingCapability,
    #[store(code = "ex")]
    External,
}

/// Stored flattened: a [`Failure`] and a lifecycle's `GaveUp` carry its
/// category and detail as two columns.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub struct Cause {
    pub category: CauseCategory,
    pub detail: String,
}

impl Cause {
    pub fn new(category: CauseCategory, detail: impl Into<String>) -> Self {
        Cause { category, detail: detail.into() }
    }
}

/// How one attempt ended: the argument of an attempt edge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Succeeded,
    Failed { cause: Cause, retryable: bool },
}

/// The latest failure in the current attempt window, kept in the state data
/// of each label that judges attempts, beside the count of consecutive
/// failures. Stored flattened, with its cause flattened in turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct Failure {
    /// When the attempt edge recording it was applied.
    pub at: Timestamp,
    #[store(flatten)]
    pub cause: Cause,
    pub retryable: bool,
}

/// What an attempt applied at `at` leaves in the state: a success clears the
/// window's failures, and a failure adds one and becomes the latest. The
/// spec's counts are unbounded; this one saturates at `u32::MAX`, far past
/// any policy's `max_failures`, so exhaustion is unchanged.
pub fn tally(failures: u32, outcome: &Outcome, at: Timestamp) -> (u32, Option<Failure>) {
    match outcome {
        Outcome::Succeeded => (0, None),
        Outcome::Failed { cause, retryable } => {
            (failures.saturating_add(1), Some(Failure { at, cause: cause.clone(), retryable: *retryable }))
        }
    }
}

/// How many consecutive failures a window allows, and the backoff between
/// them. Stored flattened in Ensure's spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Stored)]
pub struct AttemptPolicy {
    /// Consecutive failures after which the window is exhausted; at least 1.
    pub max_failures: u32,
    /// Delay after the first failure, in microseconds.
    pub base: i64,
    /// Multiplier applied per further failure.
    pub factor: u32,
    /// Largest delay, in microseconds.
    pub cap: i64,
}

impl AttemptPolicy {
    /// Exhausted after `max_failures` failures, retrying at once in between.
    pub const fn immediate(max_failures: u32) -> Self {
        AttemptPolicy { max_failures, base: 0, factor: 1, cap: 0 }
    }

    /// The delay after the `failures`-th consecutive failure (1-based).
    pub fn backoff(&self, failures: u32) -> i64 {
        let mut delay = self.base.max(0);
        for _ in 1..failures {
            if delay >= self.cap {
                break;
            }
            delay = delay.saturating_mul(i64::from(self.factor.max(1)));
        }
        delay.min(self.cap.max(0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// No failure since the last success in this window: try when wanted.
    Now,
    /// Retry no earlier than this moment.
    At(Timestamp),
    /// The policy is spent, or the last failure cannot be retried.
    Exhausted,
}

/// The retry judgement for one resource's current attempt window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retry {
    /// Consecutive failures since the last success in the window.
    pub failures: u32,
    pub schedule: Schedule,
    /// The last failure's cause, if the window ends in a failure.
    pub last_cause: Option<Cause>,
}

impl Retry {
    pub fn exhausted(&self) -> bool {
        self.schedule == Schedule::Exhausted
    }

    /// Whether a retry may start at `now`.
    pub fn due(&self, now: Timestamp) -> bool {
        match self.schedule {
            Schedule::Now => true,
            Schedule::At(moment) => now >= moment,
            Schedule::Exhausted => false,
        }
    }
}

/// The retry judgement for the current window, from the state's tally:
/// `failures` consecutive failures since the last success (or since the
/// window opened), the latest being `last`. A pending retry makes the
/// judgement valid until its moment, which counts from the latest failure.
pub fn retry<M: Machine>(handle: &mut Handle<'_, M>, policy: &AttemptPolicy, failures: u32, last: Option<&Failure>) -> Retry {
    let Some(last) = last.filter(|_| failures > 0) else {
        return Retry { failures: 0, schedule: Schedule::Now, last_cause: None };
    };
    let schedule = if !last.retryable || failures >= policy.max_failures.max(1) {
        Schedule::Exhausted
    } else {
        let moment = last.at.saturating_add(policy.backoff(failures));
        handle.until(moment);
        Schedule::At(moment)
    };
    Retry { failures, schedule, last_cause: Some(last.cause.clone()) }
}
