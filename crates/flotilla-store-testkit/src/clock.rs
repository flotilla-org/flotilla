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
