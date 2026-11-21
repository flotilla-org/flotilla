//! Convoy on the engine (#3587 layer 6):
//! this home's Convoy writer over the decided store.

use std::sync::Arc;

use flotilla_engine_convoys::convoys::Convoys;
use flotilla_schema::{Address, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::convoy::{
        AdmittedPromise, Convoy, ConvoyRefusal, ConvoyRequest, Declare, Priority, Progress, PromiseId, PromiseSource, Seat, SeatName,
        Submit,
    },
    Ref, Version, WriteError,
};

fn address(text: &str) -> Address {
    Address::project(text).expect("address")
}

fn request() -> ConvoyRequest {
    ConvoyRequest {
        launch_of: "c".into(),
        workflow_ref: "w".into(),
        admission: flotilla_schema::Frozen::new(flotilla_state_machines::machines::convoy::AdmissionBytes::new(Vec::new()))
            .expect("freeze"),
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
            failed: None,
            retired: false,
            replacements: 0,
            role: "coder".into(),
            seat: String::new(),
            parent: None,
            supervised_by: address("supervisor"),
        }],
        priority: Priority(1),
        kinds: Catalog::resolve(&stock(), &[]).expect("stock").freeze(["pr"]).expect("freeze"),
    }
}

fn service(dir: &tempfile::TempDir) -> Arc<Service> {
    Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), "home-a", flotilla_engine_store::registry().expect("registry")).expect("store"),
    )
}

/// Admits `request` as admission does: reserves its launch, then creates it.
fn admit(convoys: &Convoys, request: ConvoyRequest, at: Timestamp) -> Result<String, WriteError<Convoy>> {
    let launch = convoys.reserve(&request.launch_of, at)?;
    convoys.create_reserved(&launch, request, at)?;
    Ok(launch)
}

// Behaviour: a Convoy admitted and edged at this home is in the decided store, and a reopened
// home restores it at the version its log gave, so the next edge continues from there.
#[test]
fn a_convoy_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let convoys = Convoys::open("home-a", &service(&dir)).expect("open");
        let refused = admit(&convoys, request(), Timestamp(1)).expect_err("the project must exist");
        assert_eq!(refused.refusal(), Some(&ConvoyRefusal::UnknownProject));
        convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
        admit(&convoys, request(), Timestamp(1)).expect("admitted");
        convoys.world(|world| world.durable.put(&Ref::new(address("change").as_str()), true));
        let submit = Submit {
            promise: PromiseId(0),
            by: SeatName::new("coder", ""),
            address: address("change"),
            revision: "abc".into(),
            durable: Ref::new(address("change").as_str()),
        };
        // The refused admission's reservation took 1, a gap; the admission's reservation 2, its
        // change and entry 3 and 4; the submission's change and entry take 5 and 6 (#4024).
        assert_eq!(convoys.apply("c", submit, Timestamp(2)).expect("submitted"), Version(6));
    }
    let convoys = Convoys::open("home-a", &service(&dir)).expect("reopen");
    let convoy = convoys.get("c").expect("restored");
    assert_eq!(convoy.version(), Version(6));
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
    let error = admit(&convoys, twice, Timestamp(1)).expect_err("a taken label");
    assert_eq!(error.refusal(), None, "the store refuses it, not the machine: {error:?}");
    assert!(convoys.get("c").is_none(), "nothing was admitted");
    admit(&convoys, request(), Timestamp(1)).expect("admitted");
    admit(&convoys, ConvoyRequest { launch_of: "d".into(), ..request() }, Timestamp(1)).expect("the same label in another convoy");
}

// Behaviour (#3721 review): a restored home still holds labels unique, and a store refusal of an
// edge (a `Declare` under a taken label) leaves the record as it was, in memory and on reopen.
#[test]
fn a_taken_label_is_refused_after_a_restart_and_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let convoys = Convoys::open("home-a", &service(&dir)).expect("open");
        convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
        admit(&convoys, request(), Timestamp(1)).expect("admitted");
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
    // The reservation took 1; the admission's change and entry 2 and 3.
    assert_eq!((convoy.version(), convoy.spec().promises.len()), (Version(3), 1), "nothing changed in memory");
    drop(convoys);
    let convoys = Convoys::open("home-a", &service(&dir)).expect("reopen again");
    assert_eq!(convoys.get("c").expect("restored").spec().promises.len(), 1, "nothing was stored");
    let free = Declare { label: "l2-quint".into(), ..declare };
    assert_eq!(convoys.apply("c", free, Timestamp(3)).expect("a free label"), Version(5), "the refusal took no sequence");
}

// Behaviour (owner's ruling, #3108): each standing generation is its own convoy, the next launch
// of the role's name. The ended one stays as it ended, under its own name; a refused admission
// takes no launch; and a reopened home reads the newest launch as the role's convoy. (That only one
// generation is live is admission's check, under its lock.)
#[hegel::test]
fn each_standing_generation_is_its_own_launch(tc: hegel::TestCase) {
    use flotilla_state_machines::machines::convoy::{Abandon, AbandonCause, Authority, ConvoyLabel, Deliver, Keep};
    let standing = |request: ConvoyRequest| ConvoyRequest { launch_of: "standing".into(), ..request };
    // Sequences cover both terminal labels, duplicate admission and optional
    // restart before the next generation, across one to three generations.
    let steps = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
    let dir = tempfile::tempdir().expect("tempdir");
    let service = service(&dir);
    let mut convoys = Convoys::open("home-a", &service).expect("open");
    convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
    admit(&convoys, standing(request()), Timestamp(1)).expect("initial generation");
    for _ in 0..steps {
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
            convoys.apply("standing", Deliver { disposition: "merged".into() }, Timestamp(4)).expect("deliver");
        } else {
            convoys.world(|world| world.authority.put(&Ref::new("operator"), true));
            convoys
                .apply(
                    "standing",
                    Abandon {
                        by: SeatName::new("operator", ""),
                        cause: AbandonCause::Operator,
                        reason: "next generation".into(),
                        authority: Ref::new("operator"),
                    },
                    Timestamp(4),
                )
                .expect("abandon");
        }
        if tc.draw(hegel::generators::booleans()) {
            convoys = Convoys::open("home-a", &service).expect("reopen ended generation");
            convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
        }
        let ended = convoys.get("standing").expect("ended generation");
        let mut invalid = standing(request());
        invalid.project = address("missing");
        assert!(admit(&convoys, invalid, Timestamp(5)).is_err(), "gate refusal");
        let mut conflicting = standing(request());
        conflicting.promises.insert(PromiseId(1), conflicting.promises[&PromiseId(0)].clone());
        assert!(admit(&convoys, conflicting, Timestamp(5)).is_err(), "store refusal");
        assert_eq!(convoys.get("standing").as_ref(), Some(&ended), "a refused admission takes no launch");
        admit(&convoys, standing(request()), Timestamp(6)).expect("new generation");
        let next = convoys.get("standing").expect("fresh generation");
        assert_eq!(next.label(), ConvoyLabel::Open);
        assert!(next.spec().launch > ended.spec().launch);
        assert_eq!(next.name(), format!("standing#{}", next.spec().launch));
        assert_eq!(next.spec().promise(PromiseId(0)).expect("fresh promise").1.progress.value, Progress::Pending);
        convoys = Convoys::open("home-a", &service).expect("restart fresh generation");
        assert_eq!(convoys.get("standing").as_ref(), Some(&next), "the newest launch is the role's convoy");
        let retained = convoys.launch(ended.name()).expect("the ended generation is kept");
        assert_eq!((retained.version(), retained.state()), (ended.version(), ended.state()));
        convoys.world(|world| world.projects.put(&Ref::new(address("fleet").as_str()), ()));
    }
}
