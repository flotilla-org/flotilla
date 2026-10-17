//! The engine's Vessel (`flotilla_state_machines::machines::vessel::VesselRow`, kind code 7): a
//! placement lease's lifecycle. Generators share the lifecycle state, history and edge candidates
//! with the terminal session, the other lifecycle kind here.

use std::collections::BTreeMap;

use flotilla_schema::{Edge, Ref, State};
use flotilla_state_machines::{
    machines::{
        convoy::{Priority, VesselClass},
        vessel::{Vessel, VesselKind, VesselRow},
    },
    placement::{Dimension, Host},
};
use hegel::{generators as gs, TestCase};

use super::{
    engine,
    ensure::attempt,
    home, name, records, stamp_flat,
    terminal::{candidates, edges, state_value},
    timestamp, Fixture, Pick,
};

/// Covering indexes: the label is digit 0 (base 4); the shape takes every size in turn.
pub const COVERING: usize = 48;

fn shape(tc: &TestCase, pick: Pick) -> BTreeMap<Dimension, u32> {
    (0..pick.count(tc, 3))
        .map(|n| {
            let dimension = match pick {
                Pick::Draw => tc.draw(gs::integers::<u32>().max_value(3)),
                Pick::Cover(_) => u32::try_from(n).expect("small fixture"),
            };
            (Dimension(dimension), tc.draw(gs::integers::<u32>()))
        })
        .collect()
}

pub fn generate(tc: &TestCase, pick: Pick) -> VesselRow {
    VesselRow {
        convoy: Ref::new(name(tc), home(tc)),
        class: VesselClass(tc.draw(gs::integers::<u32>())),
        shape: shape(tc, pick.nth(2)),
        priority: Priority(tc.draw(gs::integers::<u32>())),
        host: Host(tc.draw(gs::integers::<u32>().max_value(3))),
        state: State::new(state_value(tc, pick), timestamp(tc)),
        edges: records(tc, pick, 3, |pick| engine::event::<Vessel>(tc, pick, &edges(tc, pick))),
        attempts: records(tc, pick.nth(1), 3, |pick| attempt(tc, pick)),
    }
}

impl Fixture for VesselRow {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> VesselRow {
        generate(tc, pick)
    }

    /// The lease drawn afresh; the state, edges and attempts kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> VesselRow {
        VesselRow { state: self.state.clone(), edges: self.edges.clone(), attempts: self.attempts.clone(), ..generate(tc, pick) }
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<VesselRow>, VesselRow)> {
        engine::drive(tc, pick, self, candidates::<VesselKind>(tc, &self.attempts))
    }

    fn stamped(&self, before: Option<&VesselRow>, version: u64) -> VesselRow {
        VesselRow {
            edges: stamp_flat(&self.edges, before.map(|before| &before.edges), version),
            attempts: stamp_flat(&self.attempts, before.map(|before| &before.attempts), version),
            ..self.clone()
        }
    }
}
