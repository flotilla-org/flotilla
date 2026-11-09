//! A standing role's restart decision, derived from its generations (#3108, ConvoyEnsure B).
//!
//! A standing role keeps no restart state of its own. Its strike run, its backoff and its hold
//! are read from the generations that fulfilled it, newest first, and from the open standing
//! attention about them:
//! - **Strikes** are the consecutive Failed generations at the head of the list. A generation
//!   that ended any other way (delivered, abandoned by an operator), was admitted under a
//!   different configuration than the role now declares, or began before a person acknowledged
//!   the hold, ends the run. A Failed generation that ran for [`RESET_AFTER`] or more is the
//!   first strike of a new run: nothing older counts.
//! - **Backoff** runs from the newest generation's `finished_at`, by
//!   [`PROVISIONING_RETRY_BACKOFF`] at the strike count.
//! - **Hold**: [`STRIKE_LIMIT`] strikes hold the role until a person acknowledges the hold.
//!   The acknowledgement ends the run, so the next generation starts from no strikes.
//!
//! The declared-driver path and the driverless path read the same function: the driverless
//! path's reset after a long run is the run-time rule, and the driver path's strike count is
//! the Failed run.

use chrono::{DateTime, Duration, Utc};
use flotilla_resources::PROVISIONING_RETRY_BACKOFF;

/// A generation that ran this long starts a new strike run when it ends.
pub const RESET_AFTER: Duration = Duration::minutes(10);
/// Consecutive strikes that hold the role for a person.
pub const STRIKE_LIMIT: u32 = 3;

/// How a generation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Its work failed: a strike, unless another rule ends the run.
    Failed,
    /// An operator abandoned it.
    Operator,
    /// It delivered.
    Delivered,
}

/// One generation of a standing role, as its record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation {
    /// When it was admitted.
    pub admitted_at: DateTime<Utc>,
    /// Whether its admitted configuration is the one the role declares now.
    pub current_config: bool,
    /// How and when it ended; `None` while it runs.
    pub ended: Option<(Ending, DateTime<Utc>)>,
}

impl Generation {
    fn strike(&self, acknowledged_at: Option<DateTime<Utc>>) -> bool {
        matches!(self.ended, Some((Ending::Failed, _))) && self.current_config && acknowledged_at.is_none_or(|at| self.admitted_at > at)
    }

    fn ran_long(&self) -> bool {
        self.ended.is_some_and(|(_, finished_at)| finished_at - self.admitted_at >= RESET_AFTER)
    }
}

/// The open standing attention about the role's hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HoldAttention {
    /// Whether one is raised and unsettled.
    pub open: bool,
    /// When a person last acknowledged one.
    pub acknowledged_at: Option<DateTime<Utc>>,
}

/// What the role does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Its newest generation is running.
    Running,
    /// Admit the next generation now.
    Admit,
    /// Admit the next generation at this time.
    BackOff { until: DateTime<Utc> },
    /// Held for a person; `raise` when no attention is open yet.
    Held { raise: bool },
}

/// The role's strike run and its decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandingReading {
    pub strikes: u32,
    pub decision: Decision,
}

/// Reads a standing role from its generations, newest first, its hold attention, and `now`.
/// `forced` is an operator's reconcile-now: it admits whatever the strikes say.
pub fn derive(generations: &[Generation], attention: HoldAttention, now: DateTime<Utc>, forced: bool) -> StandingReading {
    if generations.first().is_some_and(|newest| newest.ended.is_none()) {
        return StandingReading { strikes: 0, decision: Decision::Running };
    }
    let mut strikes = 0;
    for generation in generations.iter().take_while(|generation| generation.strike(attention.acknowledged_at)) {
        strikes += 1;
        if generation.ran_long() {
            break;
        }
    }
    let decision = if forced || strikes == 0 {
        Decision::Admit
    } else if strikes >= STRIKE_LIMIT {
        Decision::Held { raise: !attention.open }
    } else {
        let finished_at = generations[0].ended.map(|(_, at)| at).expect("a strike has ended");
        let until = finished_at + Duration::from_std(PROVISIONING_RETRY_BACKOFF.delay(strikes)).expect("the backoff fits chrono");
        if until > now {
            Decision::BackOff { until }
        } else {
            Decision::Admit
        }
    };
    StandingReading { strikes, decision }
}

#[cfg(test)]
mod tests;
