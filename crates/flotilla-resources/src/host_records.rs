//! Hosts as the planners read them (#4413): the port the convoy planner takes, served by the
//! engine's Host records.
//!
//! A placement names its host by the canonical Host name admission resolved it to, so the port
//! reads one host by that name. There is no list: the later placement auction replaces what serves
//! this port without touching the planners.

use crate::{HostConnection, HostStatus};

/// A Host as the planners read it: its cap and what it last observed.
#[derive(Clone, Debug, PartialEq)]
pub struct HostFacts {
    /// Its resource name.
    pub name: String,
    pub connection: HostConnection,
    /// The vessels it declares it can hold, if it is capped.
    pub max_vessels: Option<u32>,
    /// What its home last observed there, judged as of the read: readiness, sleep and fulfilment
    /// facts.
    pub status: Option<HostStatus>,
}

/// What a Host change tells the planners' wakes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostChange {
    /// The host of this name changed.
    Changed { name: String },
    /// Any host may have changed: a home's records were read afresh.
    Rescanned,
}

/// How many changes a slow follower of a [`HostRecords`] may fall behind by; one that lags wakes
/// every waiter, as a rescan does.
pub const HOST_CHANGES: usize = 256;

/// The Host port the planners take. Every read is by name.
pub trait HostRecords: Send + Sync {
    /// The host named `name`, if one is held.
    fn get(&self, name: &str) -> Result<Option<HostFacts>, String>;
    /// The hosts' changes from now on, as their records are written or replicated here.
    fn changes(&self) -> tokio::sync::broadcast::Receiver<HostChange>;
}
