//! Convoy on the engine behind the `[primitives]` switch (#3587 layer 6):
//! this home's Convoy writer over the decided store.

use std::sync::Arc;

use flotilla_core::primitives::convoys::Convoys;
use flotilla_schema::{Address, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::convoy::{
        AdmittedPromise, ConvoyRefusal, ConvoyRequest, Declare, Priority, Progress, PromiseId, PromiseSource, Seat, SeatName, Submit,
        VesselClass,
    },
    Ref, Version,
};

fn address(text: &str) -> Address {
    Address::project(text).expect("address")
}

fn request() -> ConvoyRequest {
    ConvoyRequest {
        project: address("fleet"),
        promises: [(
            PromiseId(0),
            AdmittedPromise {
                label: "template/pr".into(),
                kind: "pr".into(),
                target: address("target"),
                source: PromiseSource::Template,
                owner: SeatName::new("coder", ""),
            },
        )]
        .into(),
        duties: Default::default(),
        seats: vec![Seat { role: "coder".into(), seat: String::new(), parent: None, supervised_by: address("supervisor") }],
        quota: [(VesselClass::new("standard"), 1)].into(),
        priority: Priority(1),
        kinds: Catalog::resolve(&stock(), &[]).expect("stock").freeze(["pr"]).expect("freeze"),
    }
}

fn service(dir: &tempfile::TempDir) -> Arc<Service> {
    Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), "home-a", flotilla_engine_store::registry().expect("registry")).expect("store"),
    )
}

// Behaviour: a Convoy admitted and edged at this home is in the decided store, and a reopened
// home restores it at the version its log gave, so the next edge continues from there.
#[test]
fn a_convoy_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let convoys = Convoys::open("home-a", &service(&dir)).expect("open");
        let refused = convoys.admit("c", request(), Timestamp(1)).expect_err("the project must exist");
        assert_eq!(refused.refusal(), Some(&ConvoyRefusal::UnknownProject));
        convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
        assert!(convoys.admit("c", request(), Timestamp(1)).expect("admitted"));
        assert!(!convoys.admit("c", request(), Timestamp(1)).expect("existing"), "a second admission is the first");
        convoys.world(|world| world.durable.put(&Ref::new(address("change").as_str()), true));
        let submit = Submit {
            promise: PromiseId(0),
            by: SeatName::new("coder", ""),
            address: address("change"),
            revision: "abc".into(),
            durable: Ref::new(address("change").as_str()),
        };
        assert_eq!(convoys.apply("c", submit, Timestamp(2)).expect("submitted"), Version(2));
    }
    let convoys = Convoys::open("home-a", &service(&dir)).expect("reopen");
    let convoy = convoys.get("c").expect("restored");
    assert_eq!(convoy.version(), Version(2));
    let (_, promise) = convoy.spec().promise(PromiseId(0)).expect("promise");
    assert_eq!(promise.progress.value, Progress::Submitted);
}

// Behaviour (#3587 layer 6 labels ruling): a promise's label is unique within its convoy. The
// store refuses a second promise of one convoy under a taken label, and two convoys may each
// have a promise of the same label.
#[test]
fn a_label_is_unique_within_its_convoy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let convoys = Convoys::open("home-a", &service(&dir)).expect("open");
    convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
    let mut twice = request();
    let mut second = twice.promises[&PromiseId(0)].clone();
    second.kind = "decision-ledger".into();
    twice.promises.insert(PromiseId(1), second);
    let error = convoys.admit("c", twice, Timestamp(1)).expect_err("a taken label");
    assert_eq!(error.refusal(), None, "the store refuses it, not the machine: {error:?}");
    assert!(convoys.get("c").is_none(), "nothing was admitted");
    assert!(convoys.admit("c", request(), Timestamp(1)).expect("admitted"));
    assert!(convoys.admit("d", request(), Timestamp(1)).expect("the same label in another convoy"));
}

// Behaviour (#3721 review): a restored home still holds labels unique, and a store refusal of an
// edge (a `Declare` under a taken label) leaves the record as it was, in memory and on reopen.
#[test]
fn a_taken_label_is_refused_after_a_restart_and_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let convoys = Convoys::open("home-a", &service(&dir)).expect("open");
        convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
        assert!(convoys.admit("c", request(), Timestamp(1)).expect("admitted"));
    }
    let convoys = Convoys::open("home-a", &service(&dir)).expect("reopen");
    let declare = Declare {
        promise: PromiseId(1),
        label: "template/pr".into(),
        kind: "pr".into(),
        target: address("target"),
        owner: SeatName::new("coder", ""),
    };
    let error = convoys.apply("c", declare.clone(), Timestamp(2)).expect_err("a taken label");
    assert_eq!(error.refusal(), None, "the store refuses it: {error:?}");
    let convoy = convoys.get("c").expect("still there");
    assert_eq!((convoy.version(), convoy.spec().promises.len()), (Version(1), 1), "nothing changed in memory");
    drop(convoys);
    let convoys = Convoys::open("home-a", &service(&dir)).expect("reopen again");
    assert_eq!(convoys.get("c").expect("restored").spec().promises.len(), 1, "nothing was stored");
    let free = Declare { label: "l2-quint".into(), ..declare };
    assert_eq!(convoys.apply("c", free, Timestamp(3)).expect("a free label"), Version(2));
}
