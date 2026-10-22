//! The production vessel composition ([`Placements`]) against `conformance/vessel.qnt`, over the
//! engine's memory log. The adaptor (event and state mapping) is shared with the decided store's
//! run: `flotilla_quint_testkit::vessel`.
//!
//! [`Placements`]: flotilla_state_machines::placement::Placements

use flotilla_quint_testkit::vessel::{VesselConformance, VesselLogs};
use flotilla_state_machines::{machines::vessel::Vessel, MemoryLog};

use crate::graph::{self, Conformance, Report};

struct MemoryLogs;

impl VesselLogs for MemoryLogs {
    type Log = MemoryLog<Vessel>;
    fn log() -> MemoryLog<Vessel> {
        MemoryLog::new()
    }
}

// Every reachable vessel state is reached in Rust by the spec's path, and every probe agrees:
// placement refuses in the spec's order (admission, the cap, the fit, a higher-priority waiting
// request that fits), a lease holds the request on one host as the stored vessel's spec, the
// lifecycle half moves or refuses as composed, and a Gone vessel's lease is freed at the ledger.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn vessel_follows_the_full_graph() {
    let mut report = Report::new("vessel");
    graph::check::<VesselConformance<MemoryLogs>>(&graph::load(VesselConformance::<MemoryLogs>::MACHINE), &mut report);
    report.finish();
}
