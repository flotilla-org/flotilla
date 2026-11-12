//! The crew kind's machine through the decided store (#3629 layer 6): `conformance/vessel.qnt`
//! against the production placement composition, over [`StoreLog`] on an SQLite store holding the
//! stored kind. The adaptor is the engine's own (`flotilla_quint_testkit::vessel`); here every
//! write is one store transaction, and the stored row must hold what the home holds.
//!
//! ```text
//! QUINT_GRAPH_DIR=DIR cargo test -p flotilla-engine-store --test crew_conformance -- --ignored
//! ```

use std::sync::{Arc, Mutex};

use flotilla_engine_store::StoreLog;
use flotilla_quint_testkit::{
    graph::{self, Conformance, Report},
    vessel::{VesselConformance, VesselLogs},
};
use flotilla_schema::{Kind, KindRegistry};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{machines::vessel::Vessel, Home, Machine, Resource};

const HOME: &str = "home";

/// A fresh store holding only `M`'s stored kind: every probe replays its path into its own store,
/// so the schema each one creates is the one the machine under test needs.
fn store<M: Machine>() -> StoreLog
where
    Resource<M>: Kind,
{
    let mut registry = KindRegistry::new();
    registry.register::<Resource<M>>().expect("stored kind");
    let connection = rusqlite::Connection::open_in_memory().expect("memory");
    StoreLog::new(Arc::new(Mutex::new(Store::open(connection, HOME, registry).expect("store"))))
}

/// Every resource `home` holds is stored as it holds it.
fn stored_as_held<M>(home: &Home<M, StoreLog>)
where
    M: Machine,
    M::Spec: PartialEq + std::fmt::Debug,
    Resource<M>: Kind,
    StoreLog: flotilla_state_machines::Log<M>,
{
    let mut store = home.log().store().lock().expect("store");
    let mut read = store.read().expect("read");
    for held in home.resources() {
        let id = read.resolve(Resource::<M>::CODE, held.name(), HOME).expect("resolve").expect("stored");
        let loaded = read.load::<Resource<M>>(id).expect("load").expect("current row");
        assert_eq!(&loaded.value.with_identity(&loaded.name, loaded.version), held, "the store holds the resource the home holds");
    }
}

struct StoreLogs;

impl VesselLogs for StoreLogs {
    type Log = StoreLog;
    fn log() -> StoreLog {
        store::<Vessel>()
    }
    fn check(home: &Home<Vessel, StoreLog>) {
        stored_as_held(home);
    }
}

// Every reachable vessel state is reached through the store by the spec's path, and every probe
// agrees, writing nothing when refused.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn vessel_follows_the_full_graph_through_the_store() {
    let mut report = Report::new("vessel (store)");
    graph::check::<VesselConformance<StoreLogs>>(&graph::load(VesselConformance::<StoreLogs>::MACHINE), &mut report);
    report.finish();
}

/// One path through each adaptor with the spec's results written out, so the adaptors run in the
/// ordinary test run.
mod walk {
    use flotilla_quint_testkit::{
        graph::{Conformance, Step},
        itf::{int, map_of, set_of, tag, variant},
    };
    use serde_json::{json, Value};

    use super::*;

    fn request() -> Value {
        json!({ "convoy": int(1), "class": int(1), "shape": map_of([(int(1), int(2))]), "priority": int(1) })
    }

    fn evidence(cap: i64, available: i64) -> Value {
        json!({
            "host": int(3),
            "available": map_of([(int(1), int(available))]),
            "waiting": set_of(Vec::new()),
            "live": int(0),
            "cap": int(cap),
            "admitted": cap > 0,
        })
    }

    fn refused(step: Step) -> String {
        match step {
            Step::Refused(refusal) => tag(&refusal).to_owned(),
            Step::Accepted => panic!("expected a refusal, the event was accepted"),
            Step::Unrealisable(reason) => panic!("expected a refusal, the probe was unrealisable: {reason}"),
        }
    }

    // Behaviour (#3629 layer 6): through the store, a vessel waits, is refused for no cap and for
    // no fit without a write, is leased and created Requested, is realised and released, and is
    // then Gone with its lease freed.
    #[test]
    fn a_vessel_waits_is_leased_realised_and_released_through_the_store() {
        type V = VesselConformance<StoreLogs>;
        let mut vessel = V::create(&variant("Waiting", request()));
        assert_eq!(V::encode(&vessel), variant("Waiting", request()));
        assert_eq!(refused(V::apply(&mut vessel, &variant("Place", evidence(0, 4)))), "NotAdmitted");
        assert_eq!(refused(V::apply(&mut vessel, &variant("Place", evidence(1, 1)))), "DoesNotFit");
        assert_eq!(V::version(&vessel), 0, "a refusal writes nothing");
        assert!(matches!(V::apply(&mut vessel, &variant("Place", evidence(1, 4))), Step::Accepted));
        let placement = json!({ "request": request(), "host": int(3) });
        assert_eq!(
            V::encode(&vessel),
            variant("Leased", json!({ "placement": placement, "lifecycle": variant("Requested", Value::Array(vec![])) }))
        );
        let realise = variant("Realise", json!({ "present": true, "matches": true }));
        assert!(matches!(V::apply(&mut vessel, &realise), Step::Accepted));
        assert_eq!(refused(V::apply(&mut vessel, &variant("Place", evidence(1, 4)))), "MissingEvidence");
        assert!(matches!(V::apply(&mut vessel, &variant("Release", json!({ "complete": true }))), Step::Accepted));
        assert_eq!(V::encode(&vessel), variant("Gone", placement));
        assert_eq!(refused(V::apply(&mut vessel, &realise)), "WrongState");
    }
}
