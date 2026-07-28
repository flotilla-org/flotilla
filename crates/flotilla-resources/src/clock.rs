use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

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

/// A manually advanced clock for controller and decision-edge tests.
#[derive(Debug)]
pub struct VirtualClock {
    now: Mutex<DateTime<Utc>>,
}

impl VirtualClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self { now: Mutex::new(now) }
    }

    pub fn advance(&self, duration: Duration) -> DateTime<Utc> {
        let mut now = self.now.lock().expect("virtual clock lock poisoned");
        *now += duration;
        *now
    }

    pub fn set(&self, instant: DateTime<Utc>) {
        *self.now.lock().expect("virtual clock lock poisoned") = instant;
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("virtual clock lock poisoned")
    }
}
