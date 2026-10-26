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
        seats: vec![Seat {
            retired: false,
            replacements: 0,
            role: "coder".into(),
            seat: String::new(),
            parent: None,
            supervised_by: address("supervisor"),
        }],
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

// Owner ruling #3825: replacing an ended standing generation reuses its name,
// retains all log history, and never changes an active or refused generation.
#[hegel::test]
fn ended_standing_generations_are_replaced_atomically(tc: hegel::TestCase) {
    use flotilla_schema::{Cause, Kind};
    use flotilla_state_machines::{
        machines::convoy::{Abandon, Authority, Convoy, ConvoyLabel, Deliver, Keep},
        Resource,
    };
    // Sequences cover both terminal labels, duplicate admission and optional
    // restart before replacement, across one to three generations.
    let steps = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
    let dir = tempfile::tempdir().expect("tempdir");
    let service = service(&dir);
    let mut convoys = Convoys::open("home-a", &service).expect("open");
    convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
    assert!(convoys.admit_standing("standing", request(), Timestamp(1)).expect("initial generation"));
    for _ in 0..steps {
        assert!(!convoys.admit_standing("standing", request(), Timestamp(2)).expect("duplicate active admission"));
        if tc.draw(hegel::generators::booleans()) {
            convoys
                .apply(
                    "standing",
                    Keep::Verdict {
                        promise: PromiseId(0),
                        by: SeatName::new("operator", ""),
                        authority: Authority::Operator,
                        reason: "accepted".into(),
                    },
                    Timestamp(3),
                )
                .expect("keep");
            convoys.apply("standing", Deliver, Timestamp(4)).expect("deliver");
        } else {
            convoys.world(|world| world.authority.put(&Ref::new("operator"), true));
            convoys
                .apply(
                    "standing",
                    Abandon { by: SeatName::new("operator", ""), reason: "replace generation".into(), authority: Ref::new("operator") },
                    Timestamp(4),
                )
                .expect("abandon");
        }
        if tc.draw(hegel::generators::booleans()) {
            convoys = Convoys::open("home-a", &service).expect("reopen ended generation");
            convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
        }
        let ended = convoys.get("standing").expect("ended generation");
        let (ended_version, ended_state) = (ended.version(), ended.state().clone());
        let mut invalid = request();
        invalid.project = address("missing");
        assert!(convoys.admit_standing("standing", invalid, Timestamp(5)).is_err(), "gate refusal");
        let mut conflicting = request();
        conflicting.promises.insert(PromiseId(1), conflicting.promises[&PromiseId(0)].clone());
        assert!(convoys.admit_standing("standing", conflicting, Timestamp(5)).is_err(), "store refusal rolls retirement back");
        let reopened = Convoys::open("home-a", &service).expect("reopen after refusals");
        let retained = reopened.get("standing").expect("retained ended generation");
        assert_eq!((retained.version(), retained.state()), (ended_version, &ended_state));
        assert!(convoys.admit_standing("standing", request(), Timestamp(6)).expect("new generation"));
        let next = convoys.get("standing").expect("fresh generation");
        assert_eq!(next.label(), ConvoyLabel::Open);
        assert!(next.version() > ended_version);
        assert_eq!(next.spec().promise(PromiseId(0)).expect("fresh promise").1.progress.value, Progress::Pending);
        service
            .with_store(|store| {
                let mut read = store.read()?;
                let id = read.resolve(<Resource<Convoy> as Kind>::CODE, "standing", "home-a")?.expect("identity");
                let events = read.changes(id, 0, 64)?;
                assert!(events.iter().any(|event| event.home_seq == ended_version.0), "ended history retained");
                assert!(events.iter().any(|event| event.cause == Cause::Delete), "retirement logged");
                Ok(())
            })
            .expect("read retained history");
        convoys = Convoys::open("home-a", &service).expect("restart fresh generation");
        assert_eq!(convoys.get("standing").expect("restored").label(), ConvoyLabel::Open);
        convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
    }
}

// A missing stored body or identity must return an error without evicting the
// resident ended generation or poisoning its writer locks.
#[test]
fn a_missing_resident_generation_returns_an_error() {
    use flotilla_schema::{Kind, Origin};
    use flotilla_state_machines::{
        machines::convoy::{Abandon, Convoy},
        Resource, WriteError,
    };
    for expunge in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = service(&dir);
        let convoys = Convoys::open("home-a", &service).expect("open");
        convoys.world(|world| {
            world.projects.put(&Ref::new(address("fleet").as_str()), ());
            world.authority.put(&Ref::new("operator"), true);
        });
        convoys.admit_standing("standing", request(), Timestamp(1)).expect("admit");
        convoys
            .apply(
                "standing",
                Abandon { by: SeatName::new("operator", ""), reason: "ended".into(), authority: Ref::new("operator") },
                Timestamp(2),
            )
            .expect("end");
        let version = convoys.get("standing").expect("resident").version();
        service
            .with_store(|store| {
                let mut read = store.read()?;
                let id = read.resolve(<Resource<Convoy> as Kind>::CODE, "standing", "home-a")?.expect("identity");
                let stamp = read.load::<Resource<Convoy>>(id)?.expect("stored").stamp();
                drop(read);
                let mut write = store.write()?;
                let origin = Origin::new("test", Timestamp(3));
                write.delete::<Resource<Convoy>>(stamp, &origin)?;
                if expunge {
                    write.expunge::<Resource<Convoy>>(id, &origin)?;
                }
                write.commit()
            })
            .expect("inject missing backing");
        let error = convoys.admit_standing("standing", request(), Timestamp(4)).expect_err("missing backing");
        assert!(matches!(error, WriteError::Log(_)), "{error:?}");
        assert_eq!(convoys.get("standing").expect("resident retained").version(), version);
    }
}
