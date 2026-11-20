//! The engine's convoys as their readers take them (#3108): each launch, read from the decided
//! store, under its old crews, with its verdicts, links, stalls and end.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chrono::{TimeZone, Utc};
use flotilla_engine_convoys::{
    convoy_records::EngineConvoyRecords,
    convoys::{declare_carried, Convoys, Retirement},
};
use flotilla_engine_messages::awaiting::{AwaitingReplies, OpenAwaiting, STALL};
use flotilla_protocol::{PrincipalRef, Relationship};
use flotilla_resources::{
    address::{stored_change_request_address, subject_for},
    convoy::{admission::AdmissionDocument, engine::seat_address},
    convoy_records::{ConvoyEnd, ConvoyJoins, ConvoyRecord, ConvoyRecords, Ending, SeatRecord, SeatStall},
    workflow_template::single_agent_workflow_spec,
    ConvoyPhase, SubjectDiscoverySource, WorkflowSnapshot,
};
use flotilla_schema::{Address, Frozen, Kind, NewChange, Timestamp};
use flotilla_schema_sqlite::{
    stream::{Follower, Received, Service},
    Applied, Stamp,
};
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::{
        awaiting_reply::{Awaited, AwaitingReplySpec, ProposedDisposition, StallReason},
        convoy::{
            Abandon, AbandonCause, AddSeat, AdmissionBytes, AdmittedPromise, Authority, Convoy, ConvoyRequest, FailSeat, Link, LinkSource,
            Priority, PromiseId, PromiseSource, Reassign, Seat, SeatName,
        },
        message::MessageRef,
    },
    Ref, Resource,
};

const HOME: &str = "home-a";

fn service(dir: &tempfile::TempDir) -> Arc<Service> {
    Arc::new(Service::open(&dir.path().join("decided.sqlite"), HOME, flotilla_engine_store::registry().expect("registry")).expect("store"))
}

fn project() -> Address {
    Address::project("fleet").expect("project")
}

fn document() -> AdmissionDocument {
    let workflow = single_agent_workflow_spec();
    AdmissionDocument {
        workflow_snapshot: WorkflowSnapshot {
            cascade: None,
            exit: workflow.exit,
            turn_delivery: workflow.turn_delivery,
            vessels: workflow.vessels,
            stall_nudges: workflow.stall_nudges,
            supervision: workflow.supervision,
        },
        placement_decision: None,
        placement_policy: None,
        role: "fixer".into(),
        ensure_admission: None,
        naming: None,
        dispatching_principal_ref: PrincipalRef { namespace: "flotilla".into(), name: "robert".into() },
        continuation: None,
        inputs: BTreeMap::new(),
        repositories: Vec::new(),
        r#ref: None,
        adopted_checkout_refs: BTreeMap::new(),
        subjects: Vec::new(),
        instruction: None,
        workflow_template: None,
        placement_snapshot: None,
        vessel_placements: BTreeMap::new(),
        brief_artifacts: false,
        adopted_checkout: None,
        provenance: BTreeMap::new(),
    }
}

fn request() -> ConvoyRequest {
    let bytes = document().encode().expect("encode");
    ConvoyRequest {
        launch_of: "fix-it".into(),
        workflow_ref: "single".into(),
        admission: Frozen::new(AdmissionBytes::new(bytes)).expect("freeze"),
        project: project(),
        promises: [(
            PromiseId(0),
            AdmittedPromise {
                label: "template/coder/decision-ledger".into(),
                kind: "decision-ledger".into(),
                target: project(),
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
            supervised_by: project().child(flotilla_address::Kind::Role, "operator").expect("operator"),
        }],
        priority: Priority(1),
        kinds: Catalog::resolve(&stock(), &[]).expect("stock").freeze(["decision-ledger"]).expect("freeze"),
    }
}

fn time(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("time")
}

// Behaviour: a launch reads under its old crews, with its admission, its seat's verdict and stall,
// the subjects linked to it with when and how, and how and when it ended.
#[test]
fn a_launch_reads_its_seats_links_stalls_and_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let service = service(&dir);
    let convoys = Convoys::open(HOME, &service).expect("open");
    let records = EngineConvoyRecords::new(HOME, Arc::clone(&service), Arc::default(), OpenAwaiting::open(&service).expect("open records"));
    convoys.world(|world| world.projects.put(&Ref::new(project().as_str()), ()));
    let reserved = convoys.reserve("fix-it", Timestamp(1_000_000)).expect("reserved");
    convoys.create_reserved(&reserved, request(), Timestamp(1_000_000)).expect("admitted");
    let launch = convoys.get("fix-it").expect("launch").name().to_owned();

    let record = records.get(&launch, None).expect("read").expect("held");
    assert_eq!((record.launch_of.as_str(), record.project.as_str(), record.workflow_ref.as_str()), ("fix-it", "fleet", "single"));
    assert_eq!(record.admission, document());
    assert_eq!(record.admitted_at, time(1));
    assert_eq!(record.ended, None);
    assert_eq!(record.seats["work"]["coder"], SeatRecord::default());
    assert_eq!(record.promises["work"]["coder"].len(), 1);
    assert_eq!(records.newest("fix-it", "fleet", None).expect("newest").map(|newest| newest.name), Some(launch.clone()));

    // A stall about the seat, recorded by its sender.
    let seat = seat_address(&project(), &launch, "coder").expect("seat");
    let awaiting = AwaitingReplies::open(HOME, &service).expect("awaiting");
    let supervisor = project().child(flotilla_address::Kind::Role, "operator").expect("operator");
    let stall = AwaitingReplySpec {
        message: MessageRef::new(format!("{supervisor}/message:m1"), HOME),
        sender: seat.clone(),
        recipient: supervisor,
        subject: Some(seat),
        kind: STALL.into(),
        awaited: Awaited::Reply,
        deadline: None,
        report: Some("rate limit".into()),
        stall_reason: Some(StallReason::Infra),
        proposed_disposition: Some(ProposedDisposition::Resume),
    };
    assert!(awaiting.record(stall, Timestamp(2_000_000)).expect("stall recorded"));

    let change = stored_change_request_address("github.com", "flotilla-org/flotilla", 12, &[]).expect("change request");
    let by = SeatName::new("coder", "");
    convoys.world(|world| world.authority.put(&Ref::new("granted"), true));
    convoys
        .apply(
            "fix-it",
            Link { subject: change.clone(), source: LinkSource::Claim, by: by.clone(), authority: Ref::new("granted") },
            Timestamp(3_000_000),
        )
        .expect("linked");
    convoys
        .apply(
            "fix-it",
            FailSeat { seat: by.clone(), reason: "it looped".into(), by: by.clone(), authority: Ref::new("granted") },
            Timestamp(4_000_000),
        )
        .expect("failed");
    convoys
        .apply(
            "fix-it",
            Abandon { by, cause: AbandonCause::Failed, reason: "coder failed".into(), authority: Ref::new("granted") },
            Timestamp(5_000_000),
        )
        .expect("abandoned");

    let record = records.get(&launch, None).expect("read").expect("held");
    assert_eq!(
        record.seats["work"]["coder"],
        SeatRecord {
            failed: Some("it looped".into()),
            handed_back: false,
            // The stall's evidence, as its record carries it.
            stalled: Some(SeatStall {
                report: Some("rate limit".into()),
                reason: Some(flotilla_resources::StallReason::Infra),
                proposed_disposition: Some(flotilla_resources::StallProposedDisposition::Resume),
            }),
            kept_at: None
        }
    );
    assert_eq!(record.subjects.len(), 1);
    let linked = &record.subjects[0];
    assert_eq!(linked.subject, subject_for(&change).expect("subject"));
    assert_eq!(linked.relationship, Relationship::Produces);
    assert_eq!((linked.sources[0].source, linked.sources[0].at), (SubjectDiscoverySource::Claim, time(3)));
    assert_eq!(record.ended, Some(Ending::Held(ConvoyEnd::Failed { reason: "coder failed".into() }, time(5))));
    assert_eq!(records.at(HOME).expect("at home").records.len(), 1);
    assert_eq!(records.every().expect("every").records.len(), 1);
}

// Behaviour: a launch whose admission document does not decode is listed as unreadable, by name
// and with why, beside the launches that read; asked for by name, it is an error naming it.
#[test]
fn an_undecodable_launch_is_listed_as_unreadable_beside_the_others() {
    let dir = tempfile::tempdir().expect("tempdir");
    let service = service(&dir);
    let convoys = Convoys::open(HOME, &service).expect("open");
    let records = EngineConvoyRecords::new(HOME, Arc::clone(&service), Arc::default(), OpenAwaiting::open(&service).expect("open records"));
    convoys.world(|world| world.projects.put(&Ref::new(project().as_str()), ()));
    let reserved = convoys.reserve("fix-it", Timestamp(1_000_000)).expect("reserved");
    convoys.create_reserved(&reserved, request(), Timestamp(1_000_000)).expect("admitted");
    let readable = convoys.get("fix-it").expect("launch").name().to_owned();
    let broken = ConvoyRequest {
        launch_of: "broken".into(),
        admission: Frozen::new(AdmissionBytes::new(vec![0xff, 0x00])).expect("freeze"),
        ..request()
    };
    let reserved = convoys.reserve("broken", Timestamp(2_000_000)).expect("reserved");
    convoys.create_reserved(&reserved, broken, Timestamp(2_000_000)).expect("admitted");
    let unreadable = convoys.get("broken").expect("launch").name().to_owned();

    for listing in [records.at(HOME).expect("at home"), records.every().expect("every")] {
        assert_eq!(listing.records.iter().map(|record| record.name.clone()).collect::<Vec<_>>(), vec![readable.clone()]);
        let [launch] = listing.unreadable.as_slice() else { panic!("one unreadable launch: {:?}", listing.unreadable) };
        assert_eq!(
            (launch.name.as_str(), launch.home.as_str(), launch.launch_of.as_str(), launch.project.as_str()),
            (unreadable.as_str(), HOME, "broken", "fleet")
        );
        assert!(launch.reason.contains("admission document does not decode"), "{}", launch.reason);
    }
    let error = records.get(&unreadable, None).expect_err("an unreadable launch is no record");
    assert!(error.contains(&unreadable), "{error}");
    assert_eq!(records.get(&readable, None).expect("read").map(|record| record.name), Some(readable.clone()));

    // A read of every copy of a name lists the unreadable one with why, and fails nothing (#3108).
    for home in [None, Some(HOME)] {
        let named = records.named(&unreadable, home).expect("a read by name lists an unreadable launch");
        assert!(named.records.is_empty());
        let [launch] = named.unreadable.as_slice() else { panic!("one unreadable launch: {:?}", named.unreadable) };
        assert!(launch.reason.contains("admission document does not decode"), "{}", launch.reason);
        let named = records.named(&readable, home).expect("read by name");
        assert_eq!((named.records.len(), named.unreadable.len()), (1, 0));
    }
}

/// A decided store at `home` holding every kind a retirement reads for holds, declaring what a
/// Convoy snapshot carries as the daemon's store does when it opens.
fn service_at(dir: &tempfile::TempDir, home: &str) -> Arc<Service> {
    let registry = flotilla_provisioning::stored::decided_registry().expect("registry");
    let service = Arc::new(Service::open(&dir.path().join("decided.sqlite"), home, registry).expect("store"));
    declare_carried(&service);
    service
}

/// One round of `home`'s stream, as `from` serves it after `after`, received into `to`.
fn round(from: &Service, home: &str, after: Option<u64>, to: &Service) -> Received {
    let mut frames = Vec::new();
    from.serve_home(home, after, Timestamp(9_000_000), |frame| {
        frames.push(frame.to_vec());
        Ok(())
    })
    .expect("serve");
    let relayed = from.home() != home;
    to.with_store(|store| {
        let mut follower = if relayed { Follower::relayed(store) } else { Follower::new(store) };
        follower.receive(home, |sink| frames.iter().try_for_each(|frame| sink(frame)))
    })
    .expect("follow")
}

/// A launch of `fix-it` admitted at `convoys`' home.
fn admitted(convoys: &Convoys) -> String {
    convoys.world(|world| world.projects.put(&Ref::new(project().as_str()), ()));
    convoys.world(|world| world.authority.put(&Ref::new("granted"), true));
    let reserved = convoys.reserve("fix-it", Timestamp(1_000_000)).expect("reserved");
    convoys.create_reserved(&reserved, request(), Timestamp(1_000_000)).expect("admitted");
    convoys.get("fix-it").expect("launch").name().to_owned()
}

/// Ends `launch` as failed at two seconds and retires it at three; the version of its end.
fn failed_and_retired(convoys: &Convoys, launch: &str) -> u64 {
    let abandon = Abandon {
        by: SeatName::new("coder", ""),
        cause: AbandonCause::Failed,
        reason: "coder failed".into(),
        authority: Ref::new("granted"),
    };
    let ended = convoys.apply("fix-it", abandon, Timestamp(2_000_000)).expect("abandoned").0;
    assert_eq!(convoys.retire(launch, Timestamp(3_000_000)), Ok(Retirement::Retired));
    ended
}

// Behaviour (#3108): a replica given a retired launch by a snapshot reads how it ended, and when,
// as its home does, from the end entry the snapshot carries; the home's end event, sent to the
// replica afterwards, is a duplicate there and changes nothing.
#[test]
fn a_replica_snapshotted_a_retired_launch_reads_its_end() {
    let (dir_b, dir_a) = (tempfile::tempdir().expect("tempdir"), tempfile::tempdir().expect("tempdir"));
    let home_b = service_at(&dir_b, "home-b");
    let convoys = Convoys::open("home-b", &home_b).expect("open");
    let launch = admitted(&convoys);
    let ended = failed_and_retired(&convoys, &launch);

    let home_a = service_at(&dir_a, HOME);
    assert!(round(&home_b, "home-b", None, &home_a).snapshot);
    let records = EngineConvoyRecords::new(HOME, Arc::clone(&home_a), Arc::default(), OpenAwaiting::open(&home_a).expect("open records"));
    let record = records.get(&launch, Some("home-b")).expect("read").expect("the replica");
    assert!(record.retired);
    assert_eq!(record.ended, Some(Ending::Held(ConvoyEnd::Failed { reason: "coder failed".into() }, time(2))));

    let end = home_b
        .with_store(|store| store.read()?.changes_since("home-b", ended - 1, 1))
        .expect("the home's log")
        .pop()
        .filter(|event| event.home_seq == ended)
        .expect("the end event");
    let applied = home_a
        .with_store(|store| {
            let mut write = store.write()?;
            let applied = write.apply(&end)?;
            write.commit()?;
            Ok(applied)
        })
        .expect("apply");
    assert!(matches!(applied, Applied::Duplicate(_)), "{applied:?}");
    let record = records.get(&launch, Some("home-b")).expect("read").expect("the replica");
    assert_eq!(record.ended, Some(Ending::Held(ConvoyEnd::Failed { reason: "coder failed".into() }, time(2))));
}

// Behaviour (#3108): a replica holding a retired launch by snapshot resumes from its own
// watermark with the home's tail, as any replica does, and the launch's expunge there takes the
// end entry its snapshot carried with the rest of it.
#[test]
fn a_replica_resumes_past_a_carried_end_and_an_expunge_takes_it() {
    let (dir_b, dir_a) = (tempfile::tempdir().expect("tempdir"), tempfile::tempdir().expect("tempdir"));
    let home_b = service_at(&dir_b, "home-b");
    let convoys = Convoys::open("home-b", &home_b).expect("open");
    let launch = admitted(&convoys);
    let ended = failed_and_retired(&convoys, &launch);
    let home_a = service_at(&dir_a, HOME);
    assert!(round(&home_b, "home-b", None, &home_a).snapshot);

    // At its home, the retired launch is deleted; the replica resumes with the delete.
    let id = home_b
        .with_store(|store| {
            let mut write = store.write()?;
            let id = write.resolve(<Resource<Convoy> as Kind>::CODE, &launch, "home-b")?.expect("held");
            let version = write.load_snapshot(id)?.expect("its rows").version;
            let change = write.open_change(&NewChange::new("test", Timestamp(4_000_000)))?;
            write.delete::<Resource<Convoy>>(Stamp { id, version }, change)?;
            write.commit()?;
            Ok(id)
        })
        .expect("deleted at its home");
    let resume = |label: &str| {
        let cursor = home_a.watermark("home-b").expect("cursor").home_seq;
        let resumed = round(&home_b, "home-b", Some(cursor), &home_a);
        assert!(!resumed.snapshot, "a resume from the replica's watermark is a tail: {label}");
        assert_eq!(resumed.applied, 1, "{label}");
    };
    resume("the delete");

    // Then expunged, which the replica applies in turn.
    home_b
        .with_store(|store| {
            let mut write = store.write()?;
            let change = write.open_change(&NewChange::new("test", Timestamp(5_000_000)))?;
            write.expunge::<Resource<Convoy>>(id, change)?;
            write.commit()
        })
        .expect("expunged at its home");
    resume("the expunge");
    let held = home_a.with_store(|store| store.read()?.changes_since("home-b", 0, 1_000)).expect("the replica's log");
    assert!(held.iter().all(|event| event.home_seq != ended), "the carried end goes with the launch: {held:?}");
}

// Behaviour (#3108): a follower whose cursor at a home is before the cut of the snapshot a relay
// holds of it is sent a snapshot through the relay, never a tail through the end entries the relay
// holds only as its snapshot carried them, and reads the retired launch's end as its home does.
#[test]
fn a_relay_holding_a_retired_launch_by_snapshot_snapshots_a_follower_behind_its_cut() {
    let dirs = [(); 3].map(|_| tempfile::tempdir().expect("tempdir"));
    let home_b = service_at(&dirs[0], "home-b");
    let convoys = Convoys::open("home-b", &home_b).expect("open");
    let launch = admitted(&convoys);
    let home_c = service_at(&dirs[2], "home-c");
    round(&home_b, "home-b", None, &home_c);
    let cursor = home_c.watermark("home-b").expect("cursor").home_seq;
    failed_and_retired(&convoys, &launch);

    // The relay's store declares what a Convoy snapshot carries as it opens: it relays them
    // without its own convoys opened.
    let relay = service_at(&dirs[1], HOME);
    assert!(round(&home_b, "home-b", None, &relay).snapshot);
    assert!(round(&relay, "home-b", Some(cursor), &home_c).snapshot, "a follower behind the relay's cut is snapshotted");
    let records =
        EngineConvoyRecords::new("home-c", Arc::clone(&home_c), Arc::default(), OpenAwaiting::open(&home_c).expect("open records"));
    let record = records.get(&launch, Some("home-b")).expect("read").expect("the replica");
    assert!(record.retired);
    assert_eq!(record.ended, Some(Ending::Held(ConvoyEnd::Failed { reason: "coder failed".into() }, time(2))));
}

// Behaviour (#3108): a replica restored from a snapshot that carries no end entry (one an earlier
// generation served) holds a retired launch without the entry saying how it ended. Retired is
// ended: the launch reads as ended, how not held here, in a read by name and in its old-shape
// view, whose phase is terminal, so nothing counts it as live.
#[test]
fn a_retired_launch_whose_end_is_not_held_reads_as_ended() {
    let (dir_b, dir_a) = (tempfile::tempdir().expect("tempdir"), tempfile::tempdir().expect("tempdir"));
    let registry = flotilla_provisioning::stored::decided_registry().expect("registry");
    // A home whose store declares nothing a Convoy snapshot carries.
    let home_b = Arc::new(Service::open(&dir_b.path().join("decided.sqlite"), "home-b", registry).expect("store"));
    let convoys = Convoys::open("home-b", &home_b).expect("open");
    let launch = admitted(&convoys);
    failed_and_retired(&convoys, &launch);

    let home_a = service_at(&dir_a, HOME);
    assert!(round(&home_b, "home-b", None, &home_a).snapshot);
    let records = EngineConvoyRecords::new(HOME, Arc::clone(&home_a), Arc::default(), OpenAwaiting::open(&home_a).expect("open records"));
    let record = records.get(&launch, Some("home-b")).expect("a retired launch reads").expect("the replica");
    assert!(record.retired);
    assert_eq!(record.ended, Some(Ending::NotHeld));
    let named = records.named(&launch, Some("home-b")).expect("read by name");
    assert_eq!((named.records.len(), named.unreadable.len()), (1, 0), "{named:?}");

    assert!(record.is_terminal(&[]), "a retired launch is not live");
    let view = record.read_view("flotilla", ConvoyJoins { works: &BTreeMap::new(), working: &BTreeSet::new(), notes: None });
    let status = view.status.expect("an ended launch's view has a status");
    assert_eq!(status.phase, ConvoyPhase::Ended);
    assert!(status.phase.is_terminal());
    assert_eq!((status.disposition, status.finished_at), (None, None), "how and when it ended are not held here");
    assert_eq!(status.message.as_deref(), Some(ConvoyRecord::END_NOT_HELD));
}

// Behaviour: a crew that hands its work to a crew it fans out reads as handed back, under its old
// crew; the new crew reads under its vessel and owns the work.
#[test]
fn a_handoff_reads_as_handed_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let service = service(&dir);
    let convoys = Convoys::open(HOME, &service).expect("open");
    let records = EngineConvoyRecords::new(HOME, Arc::clone(&service), Arc::default(), OpenAwaiting::open(&service).expect("open records"));
    convoys.world(|world| world.projects.put(&Ref::new(project().as_str()), ()));
    let reserved = convoys.reserve("fix-it", Timestamp(1_000_000)).expect("reserved");
    convoys.create_reserved(&reserved, request(), Timestamp(1_000_000)).expect("admitted");
    let (coder, reviewer) = (SeatName::new("coder", ""), SeatName::new("reviewer", "work"));
    convoys.world(|world| world.authority.put(&Ref::new("handoff"), true));
    let add = AddSeat {
        seat: reviewer.clone(),
        parent: coder.clone(),
        supervised_by: project().child(flotilla_address::Kind::Role, "operator").expect("operator"),
        by: coder.clone(),
        authority: Ref::new("handoff"),
    };
    convoys.apply("fix-it", add, Timestamp(2_000_000)).expect("seated");
    let reassign = Reassign { promise: PromiseId(0), to: reviewer, by: coder, authority: Authority::Owner };
    convoys.apply("fix-it", reassign, Timestamp(3_000_000)).expect("reassigned");

    let launch = convoys.get("fix-it").expect("launch").name().to_owned();
    let record = records.get(&launch, None).expect("read").expect("held");
    assert!(record.seats["work"]["coder"].handed_back);
    assert!(!record.seats["work"]["reviewer"].handed_back);
    assert_eq!(record.promises["work"].keys().collect::<Vec<_>>(), ["reviewer"]);
}
