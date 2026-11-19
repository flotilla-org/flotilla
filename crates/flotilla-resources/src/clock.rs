use chrono::{DateTime, Utc};

/// Supplies wall-clock time to decisions whose freshness behavior must be
/// deterministic under test.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;

    /// Signals each time the clock moves other than by elapsing, so a wait for a deadline measured
    /// on it measures again. `None` for a clock that only elapses.
    fn jumps(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        None
    }
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
