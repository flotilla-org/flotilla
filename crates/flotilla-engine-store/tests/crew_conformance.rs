//! The crew kinds' machines through the decided store (#3629 layer 6): `conformance/vessel.qnt`
//! against the production placement composition and `conformance/ensure.qnt` against the Ensure,
//! each over [`StoreLog`] on an SQLite store holding the stored kind. The adaptors are the
//! engine's own (`flotilla_quint_testkit::{vessel, ensure}`); here every write is one store
//! transaction, the stored row must hold what the home holds, and the Ensure's history is read
//! back from its log entries.
//!
//! ```text
//! QUINT_GRAPH_DIR=DIR cargo test -p flotilla-engine-store --test crew_conformance -- --ignored
//! ```

use std::sync::{Arc, Mutex};

use flotilla_engine_store::StoreLog;
use flotilla_quint_testkit::{
    ensure::{EnsureConformance, EnsureLogs},
    graph::{self, Conformance, Report},
    vessel::{VesselConformance, VesselLogs},
};
use flotilla_schema::{Kind, KindRegistry};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    edge_event,
    machines::{
        ensure::{Ensure, EnsureEdge},
        vessel::Vessel,
    },
    Home, Machine, Resource,
};

const HOME: &str = "home";

fn store() -> StoreLog {
    let mut registry = KindRegistry::new();
    registry.register::<Resource<Vessel>>().expect("vessel");
    registry.register::<Resource<Ensure>>().expect("ensure");
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
        store()
    }
    fn check(home: &Home<Vessel, StoreLog>) {
        stored_as_held(home);
    }
}

impl EnsureLogs for StoreLogs {
    type Log = StoreLog;
    fn log() -> StoreLog {
        store()
    }
    fn history(home: &Home<Ensure, StoreLog>, name: &str) -> Vec<EnsureEdge> {
        stored_as_held(home);
        let mut store = home.log().store().lock().expect("store");
        let mut read = store.read().expect("read");
        let id = read.resolve(Resource::<Ensure>::CODE, name, HOME).expect("resolve").expect("stored");
        let mut edges = Vec::new();
        let mut after = 0;
        loop {
            let entries = read.changes(id, after, 64).expect("changes");
            let Some(last) = entries.last() else { return edges };
            after = last.home_seq;
            for entry in &entries {
                if let Some(event) = edge_event::<Ensure>(entry).expect("entry decodes") {
                    edges.push(event.payload.edge);
                }
            }
        }
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

// Every reachable Ensure state is reached through the store, its history read back from its log.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn ensure_follows_the_full_graph_through_the_store() {
    let mut report = Report::new("ensure (store)");
    graph::check::<EnsureConformance<StoreLogs>>(&graph::load(EnsureConformance::<StoreLogs>::MACHINE), &mut report);
    report.finish();
}

/// One path through each adaptor with the spec's results written out, so the adaptors run in the
/// ordinary test run.
mod walk {
    use flotilla_quint_testkit::{
        graph::{Conformance, Step},
        itf::{int, map_of, set_of, tag, unit, variant},
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

    // Behaviour: through the store, an Ensure records and creates a fulfiller, fails an attempt
    // (read back from its log entries), and is held.
    #[test]
    fn an_ensure_records_attempts_and_holds_through_the_store() {
        type E = EnsureConformance<StoreLogs>;
        let mut ensure = E::create(&Value::Null);
        assert!(matches!(E::apply(&mut ensure, &variant("Record", int(1))), Step::Accepted));
        let create = variant("Create", json!({ "id": int(1), "noLive": true, "absent": true }));
        assert!(matches!(E::apply(&mut ensure, &create), Step::Accepted));
        let failed = variant("Attempted", json!({ "id": int(1), "outcome": variant("Failed", json!({ "retryable": false })) }));
        assert!(matches!(E::apply(&mut ensure, &failed), Step::Accepted));
        let encoded = E::encode(&ensure);
        assert_eq!(tag(&encoded), "Unfulfilled");
        assert_eq!(encoded["value"]["history"].as_array().map(Vec::len), Some(1), "the attempt is read back from the store");
        assert!(matches!(E::apply(&mut ensure, &variant("Hold", unit())), Step::Accepted));
        assert_eq!(tag(&E::encode(&ensure)), "Held");
    }
}
