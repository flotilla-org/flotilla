//! Attempts are history, not counters. The retry count, next attempt time and
//! giving up are *computed* from the attempt records and a policy
//! ([`retry`]); nothing here is stored except the attempts themselves.

use std::time::Duration;

use crate::base::Time;
use crate::record::{Column, ColumnType, Record};

/// One shared category (open question 3, lean) with per-kind typed detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CauseCategory {
    Transient,
    Refused,
    Misconfigured,
    MissingCapability,
    External,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cause {
    pub category: CauseCategory,
    pub detail: String,
}

impl Cause {
    pub fn new(category: CauseCategory, detail: impl Into<String>) -> Self {
        Cause { category, detail: detail.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure { cause: Cause, retryable: bool },
}

/// What was tried, its target, when, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub what: String,
    pub target: String,
    pub at: Time,
    pub outcome: Outcome,
}

// What `#[derive(Record)]` would generate for `Attempt`. `outcome` is a tagged
// enum: tag column plus sparse columns for the failure variant.
impl Record for Attempt {
    const TABLE: &'static str = "attempts";
    const COLUMNS: &'static [Column] = &[
        Column { name: "what", ty: ColumnType::Tag },
        Column { name: "target", ty: ColumnType::Text },
        Column { name: "at", ty: ColumnType::Time },
        Column { name: "outcome", ty: ColumnType::Tag },
        Column { name: "cause_category", ty: ColumnType::Tag },
        Column { name: "cause_detail", ty: ColumnType::Text },
        Column { name: "retryable", ty: ColumnType::Bool },
    ];
}

/// Exponential backoff with a bounded number of attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttemptPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub factor: u32,
    pub cap: Duration,
}

impl AttemptPolicy {
    /// Delay after the `failures`-th consecutive failure (1-based).
    pub fn backoff(&self, failures: u32) -> Duration {
        let mut d = self.base;
        for _ in 1..failures {
            d = d.saturating_mul(self.factor.max(1));
            if d >= self.cap {
                return self.cap;
            }
        }
        d.min(self.cap)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// No failures since the last success: try whenever wanted.
    Now,
    /// Retry no earlier than this.
    At(Time),
    /// Policy spent, or the last failure was not retryable: give up.
    Exhausted,
}

/// A judgement. Never stored; recompute from the log whenever needed (a
/// controller rebuilds its schedule this way on start).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryJudgement {
    /// Consecutive failures since the last success.
    pub failed_attempts: u32,
    pub schedule: Schedule,
    pub last_cause: Option<Cause>,
}

impl RetryJudgement {
    pub fn exhausted(&self) -> bool {
        self.schedule == Schedule::Exhausted
    }

    pub fn due(&self, now: Time) -> bool {
        match self.schedule {
            Schedule::Now => true,
            Schedule::At(t) => now >= t,
            Schedule::Exhausted => false,
        }
    }
}

pub fn retry(attempts: &[Attempt], policy: &AttemptPolicy) -> RetryJudgement {
    let failures: Vec<&Attempt> = attempts.iter().rev().take_while(|a| matches!(a.outcome, Outcome::Failure { .. })).collect();
    let failed_attempts = failures.len() as u32;
    let Some(last) = failures.first() else {
        return RetryJudgement { failed_attempts: 0, schedule: Schedule::Now, last_cause: None };
    };
    let Outcome::Failure { cause, retryable } = &last.outcome else { unreachable!("take_while kept only failures") };
    let schedule = if !retryable || failed_attempts >= policy.max_attempts {
        Schedule::Exhausted
    } else {
        Schedule::At(last.at.plus(policy.backoff(failed_attempts)))
    };
    RetryJudgement { failed_attempts, schedule, last_cause: Some(cause.clone()) }
}
