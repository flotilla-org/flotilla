//! The engine's Service consumer (`Resource<Service>`, declared by `ServiceSpec`, kind code `sv`):
//! generators, and its edges, a receipt and a retirement.

use flotilla_schema::{Edge, State};
use flotilla_state_machines::{
    machines::service::{Receive, Retire, Service, ServiceSpec, ServiceState, ServiceWorld},
    Resource, Transport,
};
use hegel::TestCase;

use super::{
    code_enum,
    engine::{self, resource, Candidate},
    name, timestamp, Fixture, Pick,
};

/// Covering indexes span both labels and the receipt's three transports.
pub const COVERING: usize = 6;

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<Service> {
    let state = if pick.variant(tc, 2, |index| index % 2) == 0 { ServiceState::Running } else { ServiceState::Retired };
    resource(ServiceSpec::default(), State::new(state, timestamp(tc)))
}

/// A receipt for a drawn message, by each transport in turn, and an observed retirement; each
/// reads nothing.
pub fn candidates(tc: &TestCase, pick: Pick) -> Vec<Candidate<Service>> {
    let receive = Receive { message: name(tc), transport: code_enum(tc, pick, &[Transport::Turn, Transport::Notify, Transport::Mute]) };
    vec![
        (receive.into(), Box::new(()) as Box<dyn ServiceWorld>, timestamp(tc)),
        (Retire { observed: true }.into(), Box::new(()) as Box<dyn ServiceWorld>, timestamp(tc)),
    ]
}

impl Fixture for Resource<Service> {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Resource<Service> {
        generate(tc, pick)
    }

    /// A service has no spec: a spec edit writes the same value.
    fn evolve(&self, _tc: &TestCase, _pick: Pick) -> Resource<Service> {
        self.clone()
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<Service>>, Resource<Service>)> {
        engine::drive(tc, pick, self, candidates(tc, pick))
    }

    fn stamped(&self, _before: Option<&Resource<Service>>, _version: u64) -> Resource<Service> {
        self.clone()
    }
}
