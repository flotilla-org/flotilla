//! Identity and time primitives shared by every module.

use std::time::Duration;

/// Milliseconds since an arbitrary epoch. The engine never reads a wall clock;
/// the home's view supplies `now`, so tests and replays are deterministic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time(pub u64);

impl Time {
    /// Sub-millisecond durations truncate to zero; policies must use whole milliseconds or more.
    pub fn plus(self, d: Duration) -> Time {
        Time(self.0.saturating_add(d.as_millis() as u64))
    }

    pub fn since(self, earlier: Time) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

/// A typed reference to a resource (or to something an observer reports on).
/// In the real store this is an interned `resource_identity` row id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjRef {
    pub kind: String,
    pub name: String,
}

impl ObjRef {
    pub fn new(kind: impl Into<String>, name: impl Into<String>) -> Self {
        ObjRef { kind: kind.into(), name: name.into() }
    }
}

impl std::fmt::Display for ObjRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.kind, self.name)
    }
}
