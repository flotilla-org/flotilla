//! The engine's Vessel (`Resource<Vessel>`, declared by
//! `flotilla_state_machines::machines::vessel::VesselSpec`, kind code `ve`): a placement lease's
//! lifecycle. Generators share the lifecycle state and edge candidates with the terminal session,
//! the other lifecycle kind here; its history, attempts among its edges, is its log entries.

use std::collections::BTreeMap;

use flotilla_schema::{Edge, State};
use flotilla_state_machines::{
    machines::{
        convoy::{Priority, VesselClass},
        vessel::{Vessel, VesselKind, VesselSpec},
    },
    placement::{Dimension, Host},
    Resource,
};
use hegel::{generators as gs, TestCase};

use super::{
    engine::{self, resource},
    name, reference,
    terminal::{candidates, state_value},
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

/// The lease, drawn or covered; its convoy is homed here or elsewhere.
pub fn spec(tc: &TestCase, pick: Pick) -> VesselSpec {
    VesselSpec {
        convoy: reference(tc),
        class: VesselClass::new(name(tc)),
        shape: shape(tc, pick.nth(2)),
        priority: Priority(tc.draw(gs::integers::<u32>())),
        host: Host(tc.draw(gs::integers::<u32>().max_value(3))),
    }
}

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<Vessel> {
    resource(spec(tc, pick), State::new(state_value(tc, pick), timestamp(tc)))
}

impl Fixture for Resource<Vessel> {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Resource<Vessel> {
        generate(tc, pick)
    }

    /// The lease drawn afresh; the state kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Resource<Vessel> {
        self.clone().with_spec(spec(tc, pick))
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<Vessel>>, Resource<Vessel>)> {
        engine::drive(tc, pick, self, candidates::<VesselKind>(tc, self.state()))
    }

    fn stamped(&self, _before: Option<&Resource<Vessel>>, _version: u64) -> Resource<Vessel> {
        self.clone()
    }
}
