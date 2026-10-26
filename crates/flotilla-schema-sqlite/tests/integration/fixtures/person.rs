//! The engine's Person consumer (`Resource<Person>`, declared by `PersonSpec`, kind code `pn`):
//! generators, and its one edge, a receipt.

use flotilla_schema::{Edge, State};
use flotilla_state_machines::{
    machines::person::{Person, PersonSpec, PersonState, PersonWorld, Receive},
    Resource, Transport,
};
use hegel::TestCase;

use super::{
    code_enum,
    engine::{self, resource, Candidate},
    name, timestamp, Fixture, Pick,
};

/// Covering indexes span the receipt's three transports; the state has one label.
pub const COVERING: usize = 3;

pub fn generate(tc: &TestCase, _pick: Pick) -> Resource<Person> {
    resource(PersonSpec::default(), State::new(PersonState::Active, timestamp(tc)))
}

/// A receipt for a drawn message, by each transport in turn; it reads nothing.
pub fn candidates(tc: &TestCase, pick: Pick) -> Vec<Candidate<Person>> {
    let receive = Receive { message: name(tc), transport: code_enum(tc, pick, &[Transport::Turn, Transport::Notify, Transport::Mute]) };
    vec![(receive.into(), Box::new(()) as Box<dyn PersonWorld>, timestamp(tc))]
}

impl Fixture for Resource<Person> {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Resource<Person> {
        generate(tc, pick)
    }

    /// A person has no spec: a spec edit writes the same value.
    fn evolve(&self, _tc: &TestCase, _pick: Pick) -> Resource<Person> {
        self.clone()
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<Person>>, Resource<Person>)> {
        engine::drive(tc, pick, self, candidates(tc, pick))
    }

    fn stamped(&self, _before: Option<&Resource<Person>>, _version: u64) -> Resource<Person> {
        self.clone()
    }
}
