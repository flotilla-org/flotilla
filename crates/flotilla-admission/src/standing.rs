//! A standing role's restart decision, from its running values (#3108 retirement).
//!
//! The role's admitting home folds each generation's end into its running values once
//! ([`fold`]), and the decision is read from those values and the open standing attention
//! ([`derive`]); no generation is walked or kept for it:
//! - **Strikes** count the Failed generations in a row under one admitted configuration. A
//!   generation that ended any other way (delivered, abandoned by an operator) ends the run, as a
//!   person's acknowledgement of the hold does. A Failed generation that ran for [`RESET_AFTER`]
//!   or more is the first strike of a new run, and one under another configuration starts one
//!   too. A role that now declares a configuration other than its strikes' has none.
//! - **Backoff** runs from the last strike's end, by [`PROVISIONING_RETRY_BACKOFF`] at the strike
//!   count.
//! - **Hold**: [`STRIKE_LIMIT`] strikes hold the role until a person acknowledges the hold.
//!
//! The declared-driver path and the driverless path read the same functions: the driverless
//! path's reset after a long run is the run-time rule, and the driver path's strike count is
//! the Failed run.

use chrono::{DateTime, Duration, Utc};
use flotilla_resources::{standing_roles::RunningValues, PROVISIONING_RETRY_BACKOFF};

/// A Failed generation that ran this long is still a strike, the first of a new run.
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

/// One generation's end, as its record says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationEnd {
    /// Its launch number.
    pub launch: u64,
    /// The configuration it was admitted under.
    pub config: String,
    /// When it was admitted.
    pub admitted_at: DateTime<Utc>,
    /// How and when it ended.
    pub ending: Ending,
    pub finished_at: DateTime<Utc>,
    /// Why, when it says.
    pub reason: Option<String>,
}

impl GenerationEnd {
    fn ran_long(&self) -> bool {
        self.finished_at - self.admitted_at >= RESET_AFTER
    }
}

/// `values` with `end` folded in: a Failed end is a strike, continuing the run under its own
/// configuration unless it ran long; any other end ends the run. An end already counted, or older
/// than the last counted, changes nothing.
pub fn fold(values: &RunningValues, end: &GenerationEnd) -> RunningValues {
    if values.counted.is_some_and(|counted| counted >= end.launch) {
        return values.clone();
    }
    let mut next = RunningValues { counted: Some(end.launch), ..values.clone() };
    if end.ending == Ending::Failed {
        let continues = values.strike_config.as_deref() == Some(end.config.as_str()) && !end.ran_long();
        next.strikes = if continues { values.strikes + 1 } else { 1 };
        next.strike_config = Some(end.config.clone());
        next.backoff_from = Some(end.finished_at);
        next.last_failure = end.reason.clone();
    } else {
        next.strikes = 0;
    }
    next
}

/// The open standing attention about the role's hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HoldAttention {
    /// Whether one is raised and unsettled.
    pub open: bool,
    /// Whether a person acknowledged the hold on the role's last strike: the run ends, by
    /// [`acknowledge`].
    pub acknowledged: bool,
}

/// `values` once a person acknowledged the role's hold: the run ends, so the next generation
/// starts from no strikes.
pub fn acknowledge(values: &RunningValues) -> RunningValues {
    RunningValues { strikes: 0, ..values.clone() }
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

/// A standing role as every host reads it: derived from its running values, its live generation,
/// its hold attention and its project's declaration, with the refusal note of the host that
/// admits it when that is the reading host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandingRole {
    /// Its current generation: the newest live one, or the ended one it is held on.
    pub convoy: Option<String>,
    pub hold: Option<StandingHold>,
    /// Consecutive failed generations in its current strike run.
    pub strikes: u32,
    /// When it will next try to admit: its backoff, or the reading host's refusal note.
    pub next_attempt: Option<DateTime<Utc>>,
    /// Why it is not running as declared, the most pressing first.
    pub last_failure: Option<String>,
}

/// Why a standing role is held for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandingHold {
    /// Its ended generation's backing could not be verified dead.
    BackingUnverified,
    /// Its strike run reached the limit.
    RestartLimit,
}

/// Reads a standing role from its running values, whether a generation of it is `live`, the
/// configuration it declares now, its hold attention, and `now`. `forced` is an operator's
/// reconcile-now: it admits whatever the strikes say.
pub fn derive(
    values: &RunningValues,
    live: bool,
    declared_config: &str,
    attention: HoldAttention,
    now: DateTime<Utc>,
    forced: bool,
) -> StandingReading {
    if live {
        return StandingReading { strikes: 0, decision: Decision::Running };
    }
    let strikes = if values.strike_config.as_deref() == Some(declared_config) { values.strikes } else { 0 };
    let decision = if forced || strikes == 0 {
        Decision::Admit
    } else if strikes >= STRIKE_LIMIT {
        Decision::Held { raise: !attention.open }
    } else {
        let finished_at = values.backoff_from.expect("a strike has ended");
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
