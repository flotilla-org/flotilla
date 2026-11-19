//! The StandingRole port (#3108 retirement): a standing role's running values, folded from each
//! generation's end by the home that admits the role, so nothing walks or keeps old generations
//! to count strikes or time a backoff.
//!
//! The record is named by its ConvoyEnsure and written only by its admitting home's ensure pass,
//! under the ensure's reconciliation lock. Other homes read its replica.

use chrono::{DateTime, Utc};

/// A standing role's running values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunningValues {
    /// Generations in a row that failed under `strike_config`; a failed generation that ran past
    /// the reset window is the first of a new run.
    pub strikes: u32,
    /// The admitted configuration the strikes were counted under: a role that now declares
    /// another has no strikes.
    pub strike_config: Option<String>,
    /// When the last strike ended: the backoff runs from here.
    pub backoff_from: Option<DateTime<Utc>>,
    /// The last strike's reason.
    pub last_failure: Option<String>,
    /// The launch number of the last generation whose end is folded in.
    pub counted: Option<u64>,
}

/// The StandingRole port.
#[async_trait::async_trait]
pub trait StandingRoles: Send + Sync {
    /// Role `ensure`'s values: this home's where it admits the role, else the copy of the home
    /// that does. `None` before its first fold.
    async fn get(&self, ensure: &str) -> Result<Option<RunningValues>, String>;
    /// Writes role `ensure`'s values at this home: only its admitting home's ensure pass does.
    async fn put(&self, ensure: &str, values: &RunningValues) -> Result<(), String>;
}
