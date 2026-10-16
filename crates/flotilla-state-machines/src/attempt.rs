//! Attempts are history, not counters.
//!
//! An attempt is a record on the resource that made it. The retry count, the
//! schedule and giving up are judgements over those records and a policy,
//! counted within the current attempt window. Where a window starts is the
//! machine's own state: Ensure's `resume` stores how many attempts came
//! before it, and a machine without windows counts from the first attempt.
//! Older attempts stay in the history. A listing and a `give up` gate call
//! the same [`retry`], so they cannot disagree.

use flotilla_schema::{RecordRow, Stored, Timestamp};

use serde::{Deserialize, Serialize};

use crate::{Handle, Machine};

/// What kind of failure, shared across kinds; the detail is per kind. Stored
/// by code, so a rename keeps every stored category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
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

/// Stored flattened: an attempt's failure and a lifecycle's `GaveUp` carry
/// its category and detail as two columns.
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

#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Outcome {
    #[store(code = 1)]
    Succeeded,
    #[store(code = 2)]
    Failed {
        #[store(flatten)]
        cause: Cause,
        retryable: bool,
    },
}

/// One try at the resource's work, and how it ended: a row of the resource's
/// `history` table of attempts.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Attempt {
    pub at: Timestamp,
    pub outcome: Outcome,
}

impl Attempt {
    pub fn succeeded(at: Timestamp) -> Self {
        Attempt { at, outcome: Outcome::Succeeded }
    }

    pub fn failed(at: Timestamp, cause: Cause, retryable: bool) -> Self {
        Attempt { at, outcome: Outcome::Failed { cause, retryable } }
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

/// The attempts in the window that starts after the first `before`
/// attempts, read (and recorded) through the handle.
pub fn window<'a, M: Machine>(handle: &mut Handle<'a, M>, before: usize) -> &'a [RecordRow<Attempt>] {
    let attempts = handle.attempts();
    &attempts[before.min(attempts.len())..]
}

/// The retry judgement over the window after the first `before` attempts,
/// under `policy`. A pending retry makes the judgement valid until its
/// moment. The backoff counts from the newest attempt's recorded `at`:
/// attempts are appended in the order the home learns of them, and the
/// caller supplies their times.
pub fn retry<M: Machine>(handle: &mut Handle<'_, M>, policy: &AttemptPolicy, before: usize) -> Retry {
    let attempts = window(handle, before);
    let failed: Vec<(&Cause, bool)> = attempts
        .iter()
        .rev()
        .map_while(|row| match &row.record.outcome {
            Outcome::Failed { cause, retryable } => Some((cause, *retryable)),
            Outcome::Succeeded => None,
        })
        .collect();
    let failures = u32::try_from(failed.len()).unwrap_or(u32::MAX);
    let Some((cause, retryable)) = failed.first() else {
        return Retry { failures: 0, schedule: Schedule::Now, last_cause: None };
    };
    let schedule = if !retryable || failures >= policy.max_failures.max(1) {
        Schedule::Exhausted
    } else {
        // The newest row is a failure, because `failed` is non-empty.
        let last = attempts.last().map_or(Timestamp::ZERO, |row| row.record.at);
        let moment = last.saturating_add(policy.backoff(failures));
        handle.until(moment);
        Schedule::At(moment)
    };
    Retry { failures, schedule, last_cause: Some((*cause).clone()) }
}
