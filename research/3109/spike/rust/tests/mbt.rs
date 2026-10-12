//! Model-based test: Quint generates traces from the `proposed` module;
//! quint-connect replays each action through the pure Rust `step` and
//! compares the abstracted Rust state with the spec state after every step.

use convoy_spike::{step, CrewPhase, Event, Phase, PromiseState, State as Impl};
use quint_connect::*;
use serde::Deserialize;

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum SPhase {
    Active,
    Landing,
    Landed,
    Abandoned,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum SCrew {
    Working,
    Done,
    Stopped,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum SPromise {
    Open,
    Submitted,
    Kept,
    Retracted,
}

/// The spec's view of the world. Field names are the Quint variables.
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct SpecState {
    #[serde(rename = "proposed::convoy_landing::phase")]
    phase: SPhase,
    #[serde(rename = "proposed::convoy_landing::crew")]
    crew: SCrew,
    #[serde(rename = "proposed::convoy_landing::promise")]
    promise: SPromise,
    #[serde(rename = "proposed::convoy_landing::merged")]
    merged: bool,
    #[serde(rename = "proposed::convoy_landing::entryStale")]
    entry_stale: bool,
    #[serde(rename = "proposed::convoy_landing::vesselLive")]
    vessel_live: bool,
}

/// Abstraction function: richer Rust state -> spec state.
impl State<ConvoyDriver> for SpecState {
    fn from_driver(d: &ConvoyDriver) -> Result<Self> {
        let s = &d.state;
        Ok(SpecState {
            phase: match s.phase {
                Phase::Active => SPhase::Active,
                Phase::Landing => SPhase::Landing,
                Phase::Landed => SPhase::Landed,
                Phase::Abandoned => SPhase::Abandoned,
            },
            crew: match s.crew {
                CrewPhase::Working => SCrew::Working,
                CrewPhase::Done => SCrew::Done,
                CrewPhase::Stopped => SCrew::Stopped,
            },
            promise: match s.promise {
                PromiseState::Open => SPromise::Open,
                PromiseState::Submitted => SPromise::Submitted,
                PromiseState::Kept => SPromise::Kept,
                PromiseState::Retracted => SPromise::Retracted,
            },
            merged: s.pr_merged,
            entry_stale: matches!(s.phase, Phase::Landing | Phase::Landed)
                && s.landing_entry.is_some_and(|e| e.claims_epoch < s.crew_epoch),
            vessel_live: s.vessel_live,
        })
    }
}

#[derive(Default)]
struct ConvoyDriver {
    state: Impl,
}

impl ConvoyDriver {
    /// The spec says this action was enabled, so Rust must accept it.
    fn apply(&mut self, e: Event) {
        let (next, _effects) = step(&self.state, e).unwrap_or_else(|r| panic!("Rust refused an edge the spec allows: {r:?}"));
        self.state = next;
    }
}

impl Driver for ConvoyDriver {
    type State = SpecState;

    fn step(&mut self, step: &Step) -> Result {
        switch!(step {
            init => self.state = Impl::default(),
            submit => self.apply(Event::Submit),
            complete => self.apply(Event::Complete),
            handoff => self.apply(Event::Handoff),
            merge => self.apply(Event::Merge),
            verdictKept => self.apply(Event::VerdictKept),
            verdictRejected => self.apply(Event::VerdictRejected),
            turnDelivery => self.apply(Event::TurnDelivery),
            settle => self.apply(Event::Settle),
            abandon => self.apply(Event::Abandon),
            teardown => self.apply(Event::Teardown)
        })
    }
}

#[quint_run(spec = "../spec/convoy_landing.qnt", main = "proposed", max_samples = 500, max_steps = 20)]
fn proposed_table_matches_spec() -> impl Driver {
    ConvoyDriver::default()
}
