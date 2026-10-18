//! What the driver needs from a fleet under load.

use std::collections::BTreeMap;

use async_trait::async_trait;
use flotilla_dummy_fleet::catalog::CrewMix;
use flotilla_store::load_metrics::{ControllerLoadSnapshot, HistogramSnapshot, ReplicationLoadSnapshot, StoreLoadSnapshot};

/// A convoy's state in the terms of ADR 0064: open until its work is delivered
/// or it is abandoned. Today's control plane can also fail a convoy; that is
/// reported separately rather than folded into either end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConvoyState {
    Open,
    Delivered,
    Abandoned,
    Failed,
}

impl ConvoyState {
    pub fn ended(self) -> bool {
        self != Self::Open
    }
}

/// One convoy the target holds, named as the driver dispatched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvoyObservation {
    pub name: String,
    pub state: ConvoyState,
    /// Stable description of phase and crew states; exclude heartbeats and timestamps.
    pub progress: String,
    /// Evidence for a stall, if present. Empty evidence is a correctness failure.
    pub stall: Option<String>,
    /// Whether this script deliberately causes a stall.
    pub expects_stall: bool,
    pub expected_end: ConvoyState,
}

/// One home's load metrics, cumulative since it started.
#[derive(Debug, Clone, Default)]
pub struct HomeSample {
    /// `None` for a store without a single-writer queue (in-memory stores).
    pub store: Option<StoreLoadSnapshot>,
    pub controllers: BTreeMap<String, ControllerLoadSnapshot>,
    pub replication: ReplicationLoadSnapshot,
    /// Time from a convoy's admission at another home until it arrived here.
    pub replication_lag: HistogramSnapshot,
    /// Time from a Message's creation until its receipt was recorded.
    pub turn_delivery: HistogramSnapshot,
    pub database_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct TargetSample {
    pub homes: Vec<HomeSample>,
    /// Resident memory of the process hosting the target, when it is local.
    pub resident_bytes: Option<u64>,
}

/// A fleet the driver can dispatch convoys to: one or more homes (daemons),
/// each admitting and running the convoys dispatched to it.
#[async_trait]
pub trait FleetTarget: Send + Sync {
    fn homes(&self) -> usize;
    /// Admit a convoy of `mix` named `name` at `home`. Returns once admitted.
    async fn dispatch(&self, home: usize, name: &str, mix: CrewMix) -> Result<(), String>;
    /// Every convoy the home holds that the driver dispatched there. Called
    /// every tick, so it should read maintained state, not the store. A target
    /// may prepend buffered semantic changes in observation order, so a short
    /// lived stall or crew transition between ticks remains visible.
    async fn convoys(&self, home: usize) -> Result<Vec<ConvoyObservation>, String>;
    /// An authorised party's decision to abandon a convoy.
    async fn abandon(&self, home: usize, name: &str, reason: &str) -> Result<(), String>;
    /// Final cleanup evidence for ended convoys, and crew command errors.
    /// Return one diagnostic for each remaining session, vessel or checkout.
    async fn audit(&self) -> Result<Vec<String>, String>;
    async fn sample(&self) -> Result<TargetSample, String>;
    /// Open convoys counted by where they are, in the target's own terms
    /// (for example `Active · coder Working`), to explain a run's tail.
    async fn open_breakdown(&self) -> Result<std::collections::BTreeMap<String, u64>, String> {
        Ok(Default::default())
    }
}
