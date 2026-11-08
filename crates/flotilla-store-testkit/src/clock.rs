use chrono::{DateTime, Duration, Utc};
use flotilla_resources::Clock;
use std::sync::Mutex;
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

/// Wall time that moves with tokio's time: its start plus the time elapsed since, so under paused
/// time it moves only as tokio's does, and a deadline sleep computed from it wakes when the clock
/// reaches the deadline.
#[derive(Debug)]
pub struct PausedClock {
    origin: DateTime<Utc>,
    started: tokio::time::Instant,
}

impl PausedClock {
    /// Starts at the wall time now.
    pub fn start() -> Self {
        Self { origin: Utc::now(), started: tokio::time::Instant::now() }
    }
}

impl Clock for PausedClock {
    fn now(&self) -> DateTime<Utc> {
        self.origin + Duration::from_std(self.started.elapsed()).expect("paused time fits")
    }
}
