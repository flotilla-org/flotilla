//! The stored time representation: INTEGER microseconds since the Unix epoch.

use chrono::{DateTime, Utc};

/// Microseconds since the Unix epoch, the unit every stored time uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// Truncates towards the past to whole microseconds, so a stored time never lies in the future
    /// of the time it records.
    pub fn from_datetime(time: &DateTime<Utc>) -> Timestamp {
        Timestamp(time.timestamp_micros())
    }

    /// `None` when the value lies outside chrono's representable range.
    pub fn to_datetime(self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp_micros(self.0)
    }

    pub fn micros(self) -> i64 {
        self.0
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

    #[test]
    fn timestamps_outside_chrono_range_do_not_convert() {
        assert_eq!(Timestamp(i64::MAX).to_datetime(), None);
    }
}
