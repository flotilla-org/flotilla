//! The stored time representation: INTEGER microseconds since the Unix epoch.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Microseconds since the Unix epoch, the unit every stored time uses.
///
/// The state-machine engine takes every time from its caller as a `Timestamp` and never reads a
/// clock, so a replay is deterministic; its models count from zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// The epoch.
    pub const ZERO: Timestamp = Timestamp(0);
    /// The latest representable time.
    pub const MAX: Timestamp = Timestamp(i64::MAX);

    pub const fn from_micros(micros: i64) -> Timestamp {
        Timestamp(micros)
    }

    /// `self` moved later by `micros`, saturating at the representable range.
    pub const fn saturating_add(self, micros: i64) -> Timestamp {
        Timestamp(self.0.saturating_add(micros))
    }

    /// Microseconds from `earlier` to `self`, saturating; negative if `earlier` is later.
    pub const fn since(self, earlier: Timestamp) -> i64 {
        self.0.saturating_sub(earlier.0)
    }

    /// Truncates towards the past to whole microseconds, so a stored time never lies in the future
    /// of the time it records.
    pub fn from_datetime(time: &DateTime<Utc>) -> Timestamp {
        Timestamp(time.timestamp_micros())
    }

    /// `None` when the value lies outside chrono's representable range.
    pub fn to_datetime(self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp_micros(self.0)
    }

    pub const fn micros(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}µs", self.0)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use hegel::generators as gs;

    use super::Timestamp;

    // Behaviour: a stored time is the input floored to whole microseconds, and converting that back
    // to a DateTime and storing it again is the identity. The generator spans times before and after
    // the epoch (negative seconds exercise floor rather than truncation towards zero) and every
    // nanosecond offset inside a second.
    #[hegel::test]
    fn timestamps_floor_to_microseconds_and_round_trip(tc: hegel::TestCase) {
        let seconds = tc.draw(gs::integers::<i64>().min_value(-4_000_000_000).max_value(4_000_000_000));
        let nanos = tc.draw(gs::integers::<u32>().min_value(0).max_value(999_999_999));
        let time = DateTime::<Utc>::from_timestamp(seconds, nanos).expect("in range");

        let stamp = Timestamp::from_datetime(&time);
        assert_eq!(stamp.micros(), seconds * 1_000_000 + i64::from(nanos / 1_000));
        let back = stamp.to_datetime().expect("representable");
        assert!(back <= time, "flooring never moves a time forward");
        assert_eq!(back.timestamp_subsec_nanos() % 1_000, 0);
        assert_eq!(Timestamp::from_datetime(&back), stamp);
    }

    /// A value within a thousand of zero, `i64::MAX` or `i64::MIN`, so draws reach both ends.
    fn near_an_end(tc: &hegel::TestCase) -> i64 {
        let anchor = [0, i64::MAX, i64::MIN][tc.draw(gs::integers::<usize>().max_value(2))];
        anchor.saturating_add(tc.draw(gs::integers::<i64>().min_value(-1_000).max_value(1_000)))
    }

    // Behaviour: saturating_add and since are inverse while in range and clamp at the ends of
    // i64 instead of wrapping. The generator anchors both operands at zero and at either end of
    // i64, so in-range sums and overflow in both directions are drawn.
    #[hegel::test]
    fn arithmetic_saturates_and_since_inverts_add(tc: hegel::TestCase) {
        let start = Timestamp::from_micros(near_an_end(&tc));
        let delta = near_an_end(&tc);
        let later = start.saturating_add(delta);
        assert_eq!(later.micros(), start.micros().saturating_add(delta));
        assert!(Timestamp::ZERO <= Timestamp::MAX && later <= Timestamp::MAX);
        match start.micros().checked_add(delta) {
            Some(sum) => {
                assert_eq!(later, Timestamp(sum));
                assert_eq!(later.since(start), delta);
            }
            None => assert_eq!(later, if delta > 0 { Timestamp::MAX } else { Timestamp(i64::MIN) }),
        }
        assert_eq!(start.since(later), start.micros().saturating_sub(later.micros()));
    }

    #[test]
    fn constants_and_display() {
        assert_eq!((Timestamp::ZERO.micros(), Timestamp::MAX.micros()), (0, i64::MAX));
        assert_eq!(Timestamp::from_micros(-3).to_string(), "-3µs");
        assert_eq!(Timestamp::MAX.saturating_add(1), Timestamp::MAX);
        assert_eq!(Timestamp(i64::MIN).since(Timestamp(1)), i64::MIN);
    }

    #[test]
    fn timestamps_outside_chrono_range_do_not_convert() {
        assert_eq!(Timestamp(i64::MAX).to_datetime(), None);
    }
}
