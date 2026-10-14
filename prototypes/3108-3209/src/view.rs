//! The home's view: what a gate may read, and the handle that records reads.
//!
//! A gate never touches the view directly. It receives a [`Gate`] whose read
//! methods append an [`InputRead`] each time, so "the event records what the
//! gate relied on" holds by construction rather than by each gate remembering
//! to report.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use crate::attempt::{Attempt, Cause};
use crate::base::{ObjRef, Time};
use crate::record::RecordLog;

/// How a delivered turn reached (or did not reach) the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Turn,
    Notify,
    Muted,
}

/// Typed local inputs. Replica rows and versioned query projections share this
/// prototype port; aggregate variants are not proposed stored resource shapes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact {
    DeliveryFailure {
        cause: Cause,
    },
    Consumer {
        host: String,
    },
    UnboundSince(Time),
    AppliedThrough(Time),
    Receipts {
        sessions: Vec<String>,
    },
    ReplyObserved(bool),
    FulfillerRecorded {
        fulfiller: ObjRef,
    },
    Fulfiller {
        realised: bool,
        can_receive: bool,
        gone: bool,
    },
    ChannelPresent(bool),
    ConversationEnded {
        reason: String,
    },
    Promise {
        kept: bool,
        accepted_retraction: bool,
    },
    Authorised {
        party: String,
    },
    Present {
        identity: String,
    },
    Absent,
    TeardownComplete,
    /// A receipt row on an AgentSession: it received a message.
    Receipt {
        session: String,
        transport: Transport,
    },
    Reply,
    OutcomeMet(bool),
    /// A message that names an older one as superseded.
    Supersedes {
        older: ObjRef,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub subject: ObjRef,
    /// Bumped on every change of value (observations are written only on change).
    pub version: u64,
    pub observer: String,
    pub fact: Fact,
}

/// What the home can see. Replicas of other hosts' observations arrive here.
pub trait HomeView {
    fn now(&self) -> Time;
    fn observation(&self, subject: &ObjRef) -> Option<Observation>;
    /// Time since this host was last heard from over the replication link; `None` if never.
    fn link_age(&self, host: &str) -> Option<Duration>;
}

/// A reference and version a gate read. Observation version zero records absence.
/// Synthetic clock/attempt inputs use their timestamp/length, including zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputRead {
    pub subject: ObjRef,
    pub version: u64,
}

pub struct Gate<'a> {
    view: &'a dyn HomeView,
    reads: RefCell<Vec<InputRead>>,
}

impl<'a> Gate<'a> {
    pub fn new(view: &'a dyn HomeView) -> Self {
        Gate { view, reads: RefCell::new(Vec::new()) }
    }

    pub fn now(&self) -> Time {
        let now = self.view.now();
        self.reads.borrow_mut().push(InputRead { subject: ObjRef::new("clock", "home"), version: now.0 });
        now
    }

    pub fn observe(&self, subject: &ObjRef) -> Option<Observation> {
        let obs = self.view.observation(subject);
        self.reads.borrow_mut().push(InputRead { subject: subject.clone(), version: obs.as_ref().map_or(0, |o| o.version) });
        obs
    }

    /// The resource's own attempt history, versioned by its length.
    pub fn own_attempts<'r>(&self, log: &'r RecordLog<Attempt>) -> &'r [Attempt] {
        self.reads.borrow_mut().push(InputRead { subject: ObjRef::new("attempts", log.owner().to_string()), version: log.len() as u64 });
        log.rows()
    }

    pub fn into_reads(self) -> Vec<InputRead> {
        self.reads.into_inner()
    }
}

/// In-memory view for tests and the walkthrough.
#[derive(Default)]
pub struct MemoryView {
    now: Time,
    observations: HashMap<ObjRef, Observation>,
    links: HashMap<String, Duration>,
}

impl MemoryView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_now(&mut self, now: Time) {
        self.now = now;
    }

    pub fn advance(&mut self, d: Duration) {
        self.now = self.now.plus(d);
    }

    /// Writes only when the value changes, as observers do; returns the version.
    pub fn observe(&mut self, subject: &ObjRef, observer: &str, fact: Fact) -> u64 {
        let entry = self.observations.entry(subject.clone());
        let obs =
            entry.or_insert_with(|| Observation { subject: subject.clone(), version: 0, observer: observer.into(), fact: fact.clone() });
        if obs.version == 0 || obs.fact != fact || obs.observer != observer {
            obs.version += 1;
            obs.fact = fact;
            obs.observer = observer.into();
        }
        obs.version
    }

    pub fn set_link_age(&mut self, host: &str, age: Duration) {
        self.links.insert(host.into(), age);
    }
}

impl HomeView for MemoryView {
    fn now(&self) -> Time {
        self.now
    }

    fn observation(&self, subject: &ObjRef) -> Option<Observation> {
        self.observations.get(subject).cloned()
    }

    fn link_age(&self, host: &str) -> Option<Duration> {
        self.links.get(host).copied()
    }
}

/// Liveness is judged from the observer's link, never from a heartbeat on the observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Staleness {
    Current,
    Stale {
        silent_for: Duration,
    },
    /// The observer has never been heard from.
    Unknown,
}

pub fn staleness(view: &dyn HomeView, observer: &str, threshold: Duration) -> Staleness {
    match view.link_age(observer) {
        None => Staleness::Unknown,
        Some(age) if age > threshold => Staleness::Stale { silent_for: age },
        Some(_) => Staleness::Current,
    }
}
