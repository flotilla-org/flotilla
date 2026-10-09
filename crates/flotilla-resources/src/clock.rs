use chrono::{DateTime, Utc};

/// Supplies wall-clock time to decisions whose freshness behavior must be
/// deterministic under test.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
