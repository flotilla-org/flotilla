use std::fmt;

/// A moment in microseconds, always supplied by the caller.
///
/// The engine never reads a clock: gates, targets and judgements see the time
/// the caller passed to the write or judgement, so a replay is deterministic.
/// The value is signed so it maps one-to-one onto a 64-bit storage integer;
/// the caller chooses the epoch (Unix time in production, zero in models).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time(i64);

impl Time {
    pub const ZERO: Time = Time(0);
    pub const MAX: Time = Time(i64::MAX);

    pub const fn from_micros(micros: i64) -> Self {
        Time(micros)
    }

    pub const fn as_micros(self) -> i64 {
        self.0
    }

    /// `self` moved later by `micros`, saturating at the representable range.
    pub const fn saturating_add(self, micros: i64) -> Self {
        Time(self.0.saturating_add(micros))
    }

    /// Microseconds from `earlier` to `self`, saturating; negative if `earlier` is later.
    pub const fn since(self, earlier: Time) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}µs", self.0)
    }
}
