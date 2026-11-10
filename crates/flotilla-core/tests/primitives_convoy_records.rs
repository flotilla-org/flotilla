//! The engine's convoys as their readers take them (#3108): each launch, read from the decided
//! store, under its old crews, with its verdicts, links, stalls and end.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{TimeZone, Utc};
use flotilla_core::primitives::{
    convoy_records::{seat_address, EngineConvoyRecords},
    convoys::Convoys,
};
use flotilla_engine_messages::awaiting::{AwaitingReplies, STALL};
use flotilla_protocol::{PrincipalRef, Relationship};
use flotilla_resources::{
    address::{stored_change_request_address, subject_for},
    convoy::admission::AdmissionDocument,
    convoy_records::{ConvoyEnd, ConvoyRecords, SeatRecord},
    workflow_template::single_agent_workflow_spec,
    SubjectDiscoverySource, WorkflowSnapshot,
};
use flotilla_schema::{Address, Frozen, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::{
        awaiting_reply::{Awaited, AwaitingReplySpec},
        convoy::{
            Abandon, AbandonCause, AddSeat, AdmissionBytes, AdmittedPromise, Authority, ConvoyRequest, FailSeat, Link, LinkSource,
            Priority, PromiseId, PromiseSource, Reassign, Seat, SeatName, VesselClass,
        },
        message::MessageRef,
    },
    Ref,
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
        admitted_from: None,
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
        quota: [(VesselClass::new("standard"), 1)].into(),
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
    let records = EngineConvoyRecords::new(HOME, Arc::clone(&service));
    convoys.world(|world| world.projects.put(&Ref::new(project().as_str()), ()));
    assert!(convoys.admit(request(), Timestamp(1_000_000)).expect("admitted"));
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
    assert_eq!(record.seats["work"]["coder"], SeatRecord { failed: Some("it looped".into()), handed_back: false, stalled: true });
    assert_eq!(record.subjects.len(), 1);
    let linked = &record.subjects[0];
    assert_eq!(linked.subject, subject_for(&change).expect("subject"));
    assert_eq!(linked.relationship, Relationship::Produces);
    assert_eq!((linked.sources[0].source, linked.sources[0].at), (SubjectDiscoverySource::Claim, time(3)));
    assert_eq!(record.ended, Some((ConvoyEnd::Failed { reason: "coder failed".into() }, time(5))));
    assert_eq!(records.at(HOME).expect("at home").len(), 1);
    assert_eq!(records.every().expect("every").len(), 1);
}

// Behaviour: a crew that hands its work to a crew it fans out reads as handed back, under its old
// crew; the new crew reads under its vessel and owns the work.
#[test]
fn a_handoff_reads_as_handed_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let service = service(&dir);
    let convoys = Convoys::open(HOME, &service).expect("open");
    let records = EngineConvoyRecords::new(HOME, Arc::clone(&service));
    convoys.world(|world| world.projects.put(&Ref::new(project().as_str()), ()));
    assert!(convoys.admit(request(), Timestamp(1_000_000)).expect("admitted"));
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
