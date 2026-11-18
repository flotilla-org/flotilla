//! The Convoy import and its pre-roll rehearsal (#3587 layer 9, #3574
//! decision 11): an old convoy mapped onto the engine's record, checked
//! against the machine and written through the creation path.

use std::{collections::BTreeMap, fs, path::Path};

use chrono::Utc;
use flotilla_engine_convoys::convoy_import::{
    check, import_convoy, rehearse, rehearse_all, ConvoyImportRefusal, ConvoyImportWorld, ImportOutcome, ImportedConvoy, StaysOldReason,
};
use flotilla_resources::{
    promises::{self, PromiseKind, PromiseOperation, PromiseSource, Submission, SubmissionVerdict},
    AdmissionDocument, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, LeafMaker,
    ObjectMeta, ResourceObject, Selector, StallEvidenceSource, StallProposedDisposition, StallReason, StallRung, StalledCondition,
    VesselRequirement, WorkCompletionAuthority, WorkPhase, WorkState, WorkflowSnapshot, ENSURED_FROM_ANNOTATION,
};
use flotilla_schema::Address;
use flotilla_state_machines::machines::{
    awaiting_reply,
    convoy::{ConvoyEdge, DutyScope, Progress, SeatName},
    message::MessageRef,
};

const HOME: &str = "home-a";
const PR: &str = "cr/github.com/org/repo/5";

struct World {
    standing_ruled: bool,
}

impl ConvoyImportWorld for World {
    fn message(&self, record: &str) -> Option<(MessageRef, Address)> {
        (record == "supervision-1").then(|| {
            // As the message import maps it: under the launch convoy `c` imports as.
            let inbox: Address = "project:fleet/convoy:c%231/role:bosun".parse().expect("inbox");
            (MessageRef::new(format!("{inbox}/message:supervision-1"), HOME), inbox)
        })
    }
    fn standing_ruled(&self) -> bool {
        self.standing_ruled
    }
    /// Every convoy here is its engine name's first launch.
    fn launch(&self, _: &str) -> Option<u64> {
        Some(1)
    }
}

const WORLD: World = World { standing_ruled: false };

fn agent(role: &str, deliverer: bool) -> CrewSpec {
    CrewSpec::builder()
        .role(role.to_string())
        .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
        .deliverer(deliverer)
        .build()
}

fn convoy(name: &str, phase: ConvoyPhase, crew: Vec<CrewSpec>) -> ResourceObject<Convoy> {
    let mut status = ConvoyStatus {
        phase,
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(crew.clone()).build()],
        }),
        crew_work: BTreeMap::from([(
            "work".to_string(),
            crew.iter().map(|crew| (crew.role.clone(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())).collect(),
        )]),
        ..Default::default()
    };
    for (vessel, role, operation) in promises::admitted_declarations(&[], &status, false) {
        promises::apply(&mut status, &vessel, &role, &operation);
    }
    ResourceObject {
        metadata: ObjectMeta {
            name: name.into(),
            namespace: "flotilla".into(),
            resource_version: "1".into(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
            merge: None,
        },
        spec: ConvoySpec::builder().workflow_ref("w".to_string()).project_ref("fleet".to_string()).build(),
        status: Some(status),
    }
}

fn operate(convoy: &mut ResourceObject<Convoy>, role: &str, operation: PromiseOperation) {
    promises::apply(convoy.status.as_mut().expect("status"), "work", role, &operation);
}

fn submit(id: &str, kind: PromiseKind, reference: &str) -> PromiseOperation {
    PromiseOperation::Submit {
        id: id.into(),
        kind,
        source: PromiseSource::Crew,
        submission: Submission { reference: reference.into(), metadata: BTreeMap::new(), submitted_at: Some(Utc::now()), verdict: None },
    }
}

/// The verdict on the last submission of `role`'s promise `id`, as the old observer made it.
fn judge(convoy: &mut ResourceObject<Convoy>, role: &str, id: &str, accepted: bool) {
    let status = convoy.status.as_ref().expect("status");
    let last = promises::owned(status, "work", role).iter().find(|promise| promise.id == id).and_then(|promise| promise.submissions.last());
    let last = last.expect("a submission").clone();
    let operation = PromiseOperation::Verdict {
        id: id.into(),
        reference: last.reference.clone(),
        submitted_at: last.submitted_at,
        verdict: SubmissionVerdict { accepted, who: "forge".into(), at: Utc::now(), why: "observed".into() },
    };
    operate(convoy, role, operation);
}

fn imported(outcome: Result<ImportOutcome, ConvoyImportRefusal>) -> ImportedConvoy {
    match outcome.expect("imported") {
        ImportOutcome::Imported(convoy) => *convoy,
        ImportOutcome::StaysOld(reason) => panic!("stays old: {reason}"),
    }
}

// Behaviour (#3721 look-over ruling): only the import qualifies a label by its seat. Old ids that
// repeat across crews import as `@<seat>/<id>`, and each still maps back to the id its crew used.
#[test]
fn repeated_old_ids_import_seat_qualified() {
    let mut old = convoy("c", ConvoyPhase::Active, vec![agent("coder", true), agent("reviewer", false)]);
    for role in ["coder", "reviewer"] {
        operate(&mut old, role, PromiseOperation::Declare { id: "notes".into(), kind: PromiseKind::Pr, source: PromiseSource::Crew });
        operate(&mut old, role, submit("notes", PromiseKind::Pr, PR));
    }
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    let promises: Vec<_> = convoy.resource.spec().promises.iter().map(|row| row.record.clone()).collect();
    let labels: Vec<_> = promises.iter().map(|promise| promise.label.as_str()).collect();
    // The reviewer owed nothing at admission, so it holds the default decision ledger.
    assert_eq!(labels, vec!["template/pr", "@coder/notes", "template/decision-ledger", "@reviewer/notes"]);
    let qualified: Vec<_> = promises.iter().filter(|promise| promise.label.starts_with('@')).collect();
    assert_eq!(qualified.len(), 2);
    assert!(qualified.iter().all(|promise| flotilla_engine_convoys::convoy_map::old_id(promise) == "notes"));
}

// Behaviour: a convoy with history imports one to one: each promise keeps its progress and
// submissions under its seat and label, a rejected submission stays, a crew's Done is dropped,
// Landing becomes Open, the claim history rides along, and the machine reaches the same record.
#[test]
fn a_convoy_with_history_imports_one_to_one() {
    let mut old = convoy("c", ConvoyPhase::Landing, vec![agent("coder", true), agent("reviewer", false)]);
    operate(&mut old, "coder", submit("template/pr", PromiseKind::Pr, PR));
    judge(&mut old, "coder", "template/pr", true);
    operate(&mut old, "reviewer", PromiseOperation::Declare { id: "l2".into(), kind: PromiseKind::Pr, source: PromiseSource::Crew });
    operate(&mut old, "reviewer", submit("l2", PromiseKind::Pr, "cr/github.com/org/repo/6"));
    judge(&mut old, "reviewer", "l2", false);
    operate(&mut old, "reviewer", PromiseOperation::Declare { id: "spike".into(), kind: PromiseKind::Pr, source: PromiseSource::Crew });
    operate(&mut old, "reviewer", PromiseOperation::Retract { id: "spike".into(), reason: "folded".into() });
    let status = old.status.as_mut().expect("status");
    let coder = status.crew_work.get_mut("work").and_then(|crew| crew.get_mut("coder")).expect("coder");
    coder.phase = CrewWorkPhase::Done;
    coder.message = Some("merged".into());

    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    let progress: Vec<_> = convoy
        .resource
        .spec()
        .promises
        .iter()
        .map(|row| (row.record.label.clone(), row.record.owner.clone(), row.record.progress.value, row.record.submissions.len()))
        .collect();
    assert_eq!(
        progress,
        vec![
            ("template/pr".to_owned(), SeatName::new("coder", ""), Progress::Kept, 1),
            ("template/decision-ledger".to_owned(), SeatName::new("reviewer", ""), Progress::Pending, 0),
            ("l2".to_owned(), SeatName::new("reviewer", ""), Progress::Pending, 1),
            ("spike".to_owned(), SeatName::new("reviewer", ""), Progress::Retracted, 0),
        ]
    );
    assert_eq!(convoy.resource.label(), flotilla_state_machines::machines::convoy::ConvoyLabel::Open, "Landing imports as Open");
    let coder_claim = convoy.history.crews.iter().find(|claim| claim.role == "coder").expect("coder's claim");
    assert_eq!((coder_claim.vessel.as_str(), coder_claim.phase.as_str()), ("work", "Done"));
    assert!(coder_claim.claim.contains("\"merged\""), "{coder_claim:?}");
    check(&convoy.resource).expect("the machine reaches it");
    assert_eq!(rehearse(std::slice::from_ref(&convoy)), Vec::new());
    // What the spec and the bootstrapped status froze rides in the admission document.
    let spec = convoy.resource.spec();
    assert_eq!(spec.workflow_ref, old.spec.workflow_ref);
    let document = AdmissionDocument::decode("c", spec.admission.value().bytes()).expect("admission document");
    assert_eq!(Some(&document.workflow_snapshot), old.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()));
    assert_eq!(document.dispatching_principal_ref, old.spec.dispatching_principal_ref);
}

// Behaviour: an old convoy admitted with no role imports with its name as its role, as admission
// names a convoy's role now; one with a role keeps it.
#[test]
fn a_convoy_with_no_role_imports_known_by_its_name() {
    for (role, expected) in [("", "c"), ("reviewers", "reviewers")] {
        let mut old = convoy("c", ConvoyPhase::Active, vec![agent("coder", true)]);
        old.spec.role = role.into();
        let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
        let document = AdmissionDocument::decode("c", convoy.resource.spec().admission.value().bytes()).expect("admission document");
        assert_eq!(document.role, expected, "role {role:?}");
    }
}

// Behaviour: every convoy names its project. A stored convoy naming none does not decode, and one
// naming an empty project is refused by name, finite or standing, rather than imported under its
// namespace.
#[test]
fn a_convoy_naming_no_project_is_refused() {
    let mut stored = serde_json::to_value(convoy("c", ConvoyPhase::Active, vec![agent("coder", true)])).expect("encoded");
    stored["spec"].as_object_mut().expect("spec").remove("project_ref");
    assert!(serde_json::from_value::<ResourceObject<Convoy>>(stored).is_err(), "a projectless convoy decodes");
    for standing in [false, true] {
        let mut old = convoy("c", ConvoyPhase::Active, vec![agent("coder", true)]);
        old.spec.project_ref = String::new();
        if standing {
            old.spec.role = "governor".into();
            old.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "ensure-governor".into());
        }
        let world = World { standing_ruled: true };
        assert!(
            matches!(import_convoy(&old, HOME, &[], &world), Err(ConvoyImportRefusal::Unmappable { ref convoy, .. }) if convoy == "c"),
            "standing {standing}"
        );
    }
}

// Behaviour: a finite, unfinished convoy with no promise is the named refusal NoObligations (its
// only agent is done, so admission owes it no default ledger); an ended convoy whose status lacks
// what its end carries is refused by name; an unbootstrapped convoy stays on the old path.
#[test]
fn unfinished_convoys_without_promises_are_refused_by_name() {
    let mut idle = convoy("idle", ConvoyPhase::Active, vec![agent("reviewer", false)]);
    let status = idle.status.as_mut().expect("status");
    status.promises.clear();
    status.crew_work.get_mut("work").and_then(|crew| crew.get_mut("reviewer")).expect("reviewer").phase = CrewWorkPhase::Done;
    assert_eq!(import_convoy(&idle, HOME, &[], &WORLD), Err(ConvoyImportRefusal::NoObligations { convoy: "idle".into() }));
    let landed = convoy("landed", ConvoyPhase::Landed, vec![agent("reviewer", false)]);
    assert_eq!(
        import_convoy(&landed, HOME, &[], &WORLD),
        Err(ConvoyImportRefusal::EndUnrecorded { convoy: "landed".into(), what: "disposition" })
    );
    let mut fresh = convoy("fresh", ConvoyPhase::Pending, vec![agent("coder", true)]);
    fresh.status.as_mut().expect("status").workflow_snapshot = None;
    assert_eq!(import_convoy(&fresh, HOME, &[], &WORLD), Ok(ImportOutcome::StaysOld(StaysOldReason::NotBootstrapped)));
}

// Behaviour (owner ruling, #3108 NoObligations option (a)): a finite convoy that ended before the
// promise ledger, owing nothing, imports ended with no promise history, marked pre-ledger; the
// machine takes its end, Deliver or Abandon. No Kept declaration is made for it.
#[test]
fn an_ended_convoy_owing_nothing_imports_pre_ledger() {
    let owing_nothing = |name: &str| {
        let mut old = convoy(name, ConvoyPhase::Active, vec![agent("reviewer", false)]);
        let status = old.status.as_mut().expect("status");
        status.promises.clear();
        status.crew_work.get_mut("work").and_then(|crew| crew.get_mut("reviewer")).expect("reviewer").phase = CrewWorkPhase::Done;
        old
    };
    let mut abandoned = owing_nothing("abandoned");
    ended(&mut abandoned);
    let mut landed = owing_nothing("landed");
    let status = landed.status.as_mut().expect("status");
    status.phase = ConvoyPhase::Landed;
    status.disposition = Some("merged".into());
    status.finished_at = Some(Utc::now());
    for (old, edge) in [(&abandoned, "Abandon"), (&landed, "Deliver")] {
        let convoy = imported(import_convoy(old, HOME, &[], &WORLD));
        assert!(convoy.history.pre_ledger, "{edge}: marked pre-ledger");
        assert_eq!(convoy.resource.spec().promises.len(), 0, "{edge}: no promise history");
        assert!(convoy.resource.spec().duties.is_empty(), "{edge}: no duty");
        let end = convoy.end.as_ref().expect("ended");
        assert_eq!(matches!(end.edge, ConvoyEdge::Deliver(_)), edge == "Deliver");
    }
    // One still running is refused as before.
    assert_eq!(import_convoy(&owing_nothing("idle"), HOME, &[], &WORLD), Err(ConvoyImportRefusal::NoObligations { convoy: "idle".into() }));
}

// Behaviour (owner ruling, #3108): an agent seat bootstrapped before the default ledger existed, and
// owing nothing, imports with the Pending template decision ledger admission now gives it.
#[test]
fn an_agent_owing_nothing_imports_with_its_default_ledger() {
    let mut old = convoy("c", ConvoyPhase::Active, vec![agent("coder", true), agent("reviewer", false)]);
    old.status.as_mut().expect("status").promises.get_mut("work").expect("work").remove("reviewer");
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    let ledger = convoy.resource.spec().promises.iter().find(|row| row.record.owner == SeatName::new("reviewer", "")).expect("ledger");
    assert_eq!((ledger.record.label.as_str(), ledger.record.progress.value), ("template/decision-ledger", Progress::Pending));
    check(&convoy.resource).expect("the machine reaches it");
}

// Behaviour (owner ruling, #3108): completion is promises only, so a work an operator forced
// complete imports as the operator's verdict keeping its open promises; settled ones keep their
// progress.
#[test]
fn a_forced_work_imports_as_kept_promises() {
    let mut old = convoy("c", ConvoyPhase::Active, vec![agent("coder", true), agent("reviewer", false)]);
    operate(&mut old, "coder", submit("template/pr", PromiseKind::Pr, PR));
    operate(&mut old, "reviewer", PromiseOperation::Declare { id: "spike".into(), kind: PromiseKind::Pr, source: PromiseSource::Crew });
    operate(&mut old, "reviewer", PromiseOperation::Retract { id: "spike".into(), reason: "folded".into() });
    let forced = WorkState::builder()
        .phase(WorkPhase::Complete)
        .completion_authority(WorkCompletionAuthority::HumanOverride)
        .finished_at(Utc::now())
        .build();
    old.status.as_mut().expect("status").work.insert("work".into(), forced);
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    let progress: Vec<_> =
        convoy.resource.spec().promises.iter().map(|row| (row.record.label.clone(), row.record.progress.value)).collect();
    assert_eq!(
        progress,
        vec![
            ("template/pr".to_owned(), Progress::Kept),
            ("template/decision-ledger".to_owned(), Progress::Kept),
            ("spike".to_owned(), Progress::Retracted),
        ]
    );
    check(&convoy.resource).expect("the machine reaches it");
}

// Behaviour: a standing convoy waits for the seat-growth ruling; once it is ruled, it imports with a
// convoy-lifetime duty for each agent seat (and none for a tool seat).
#[test]
fn a_standing_convoy_waits_for_the_ruling_then_holds_lifetime_duties() {
    let tool = CrewSpec::builder().role("shell".to_string()).source(CrewSource::Tool { command: "sh".into() }).build();
    let mut standing = convoy("governor", ConvoyPhase::Active, vec![agent("governor", false), tool]);
    standing.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "governor".into());
    standing.spec.role = "governor".into();
    assert_eq!(import_convoy(&standing, HOME, &[], &WORLD), Err(ConvoyImportRefusal::StandingAwaitsRuling { convoy: "governor".into() }));
    let convoy = imported(import_convoy(&standing, HOME, &[], &World { standing_ruled: true }));
    let duties: Vec<_> = convoy.resource.spec().duties.values().map(|duty| (duty.owner.clone(), duty.scope.clone())).collect();
    assert_eq!(duties, vec![(SeatName::new("governor", ""), DutyScope::Lifetime)]);
    check(&convoy.resource).expect("the machine admits a standing convoy");
}

// Behaviour: a standing convoy with only a tool seat holds no duty. While it is open it is refused
// by name; once it has ended it is dropped with its reason (owner ruling, #3108).
#[test]
fn a_standing_convoy_with_no_agent_seat_is_refused_open_and_dropped_ended() {
    let world = World { standing_ruled: true };
    let tool = || CrewSpec::builder().role("observer".to_string()).source(CrewSource::Tool { command: "sh".into() }).build();
    let standing = |phase| {
        let mut standing = convoy("usage", phase, vec![tool()]);
        standing.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "usage".into());
        standing.spec.role = "usage".into();
        standing
    };
    assert_eq!(
        import_convoy(&standing(ConvoyPhase::Active), HOME, &[], &world),
        Err(ConvoyImportRefusal::Unmappable {
            convoy: "usage".into(),
            reason: "a standing convoy with no agent seat holds no obligation".into()
        })
    );
    for phase in [ConvoyPhase::Abandoned, ConvoyPhase::Failed, ConvoyPhase::Landed] {
        assert_eq!(
            import_convoy(&standing(phase), HOME, &[], &world),
            Ok(ImportOutcome::StaysOld(StaysOldReason::EndedWithNoSeat)),
            "{phase:?}"
        );
    }
}

// Behaviour: an open stall that a supervision message carries becomes the stalled seat's open
// stall record, awaiting that message. One with no message, and one at the operator rung even with
// a message (no record until #3457, ruling 6353447431), records nothing and is reported.
#[test]
fn an_open_stall_becomes_the_seats_stall_record() {
    let mut old = convoy("c", ConvoyPhase::Active, vec![agent("coder", true)]);
    // The coder declared its stall: its seat reads Stalled with the report it made.
    let stall = |message: Option<&str>, rung: StallRung| StalledCondition {
        leaves: vec![flotilla_protocol::Leaf {
            address: flotilla_protocol::LeafAddress::Work { convoy: "c".into(), work: "work".into() },
            field_path: ".crew.coder.phase".into(),
            operator: flotilla_protocol::LeafOperator::Equal,
            literal: "Done".into(),
        }],
        maker: Some(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() }),
        evidence: "blocked; supervisor delivery failed: unreachable".into(),
        source: StallEvidenceSource::Crew,
        cause: None,
        began_at: Utc::now(),
        rung,
        supervisor: None,
        supervision_message: message.map(|name| flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", name)),
        supervision_index: Some(0),
        supervision_exhausted: false,
        reason: Some(StallReason::Scope),
        proposed_disposition: Some(StallProposedDisposition::ReduceScope),
        nudge_history: Vec::new(),
    };
    let status = old.status.as_mut().expect("status");
    let coder = status.crew_work.get_mut("work").and_then(|crew| crew.get_mut("coder")).expect("the coder");
    (coder.phase, coder.message) = (CrewWorkPhase::Stalled, Some("blocked".into()));
    status.stalled = Some(stall(Some("supervision-1"), StallRung::Bosun));
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    let record = convoy.stall.as_ref().expect("a stall record").spec.clone();
    assert_eq!(record.kind, "stall");
    assert_eq!(record.subject.as_ref().map(Address::as_str), Some("project:fleet/convoy:c%231/role:coder"));
    assert_eq!(record.recipient.as_str(), "project:fleet/convoy:c%231/role:bosun");
    // It carries the crew's declaration: its report, not the judge's evidence, and why.
    assert_eq!(record.report.as_deref(), Some("blocked"));
    assert_eq!(record.stall_reason, Some(awaiting_reply::StallReason::Scope));
    assert_eq!(record.proposed_disposition, Some(awaiting_reply::ProposedDisposition::ReduceScope));
    // The import writes the record beside the convoy, Open, and the rehearsal reads it back.
    assert_eq!(rehearse(std::slice::from_ref(&convoy)), Vec::new());
    // Numeric engine IDs are internal to the promise table. A carried stall
    // names a seat and a message, so a standing generation's import rewrites neither.
    let mut standing = old.clone();
    standing.spec.role = "coder".into();
    standing.spec.generation = 2;
    standing.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "ensure-coder".into());
    let mut earlier = standing.clone();
    earlier.metadata.name = "c-previous".into();
    earlier.spec.generation = 1;
    earlier.status.as_mut().unwrap().phase = ConvoyPhase::Landed;
    ended(&mut earlier);
    earlier.status.as_mut().unwrap().stalled = None;
    let folded = rehearse_all([&earlier, &standing], HOME, &[], &World { standing_ruled: true });
    assert!(folded.refused.is_empty(), "{:?}", folded.refused);
    assert!(folded.rehearsal.is_empty(), "{:?}", folded.rehearsal);
    let folded = folded.imported.iter().find(|convoy| convoy.record == "c").expect("the live generation");
    assert_eq!(folded.resource.spec().promises.iter().next().unwrap().record.id.0, 0);
    let carried = &folded.stall.as_ref().unwrap().spec;
    assert_eq!(carried.message, record.message);
    // The second generation is the role's second launch.
    assert_eq!(carried.subject.as_ref().unwrap().as_str(), "project:fleet/convoy:coder%232/role:coder");

    let connection = rusqlite::Connection::open_in_memory().expect("connection");
    let mut store =
        flotilla_schema_sqlite::Store::open(connection, HOME, flotilla_engine_store::registry().expect("registry")).expect("store");
    flotilla_engine_convoys::convoy_import::write(&mut store, &convoy).expect("written");
    let name = flotilla_engine_messages::awaiting::record_name(&record.message);
    let mut read = store.read().expect("read");
    let code = <flotilla_state_machines::Resource<flotilla_state_machines::machines::awaiting_reply::AwaitingReply> as flotilla_schema::Kind>::CODE;
    let id = read.resolve(code, &name, HOME).expect("resolve").expect("the stall record is written");
    let stored = read
        .load::<flotilla_state_machines::Resource<flotilla_state_machines::machines::awaiting_reply::AwaitingReply>>(id)
        .expect("load")
        .expect("present")
        .value;
    assert_eq!(*stored.spec(), record);
    old.status.as_mut().expect("status").stalled = Some(stall(None, StallRung::Bosun));
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    assert!(convoy.stall.is_none());
    assert!(convoy.unrecorded_stall.expect("reported").contains("no supervision message"));
    old.status.as_mut().expect("status").stalled = Some(stall(Some("supervision-1"), StallRung::Operator));
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    assert!(convoy.stall.is_none());
    assert!(convoy.unrecorded_stall.expect("reported").contains("operator-level"));
}

// Behaviour: the importer writes each convoy through the creation path at its home, as one
// `import` change at the convoy's time (#4024); the old claim history is not carried.
#[test]
fn the_import_is_one_change_at_the_convoys_time() {
    use flotilla_schema::{Cause, Kind};
    use flotilla_state_machines::{machines::convoy::Convoy as Engine, Resource};
    let mut old = convoy("c", ConvoyPhase::Active, vec![agent("coder", true)]);
    old.status.as_mut().expect("status").crew_work.get_mut("work").and_then(|crew| crew.get_mut("coder")).expect("coder").message =
        Some("first attempt".into());
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    let connection = rusqlite::Connection::open_in_memory().expect("connection");
    let mut store =
        flotilla_schema_sqlite::Store::open(connection, HOME, flotilla_engine_store::registry().expect("registry")).expect("store");
    let written = flotilla_engine_convoys::convoy_import::write(&mut store, &convoy).expect("written");
    let mut read = store.read().expect("read");
    let id = read.resolve(Resource::<Engine>::CODE, written.name(), HOME).expect("resolve").expect("created");
    let events = read.changes(id, 0, 10).expect("log");
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0].cause, Cause::Create));
    let change = read.change(HOME, events[0].change).expect("read").expect("its change");
    assert_eq!((change.writer.as_str(), change.at), ("import", convoy.resource.since()));
    assert!(convoy.history.crews[0].claim.contains("first attempt"), "the import still reads the claim: {:?}", convoy.history);
}

/// An old convoy stamped, as the old store stamps a record, to the nanosecond: digits a change's
/// microsecond time cannot hold.
fn stamped_to_the_nanosecond() -> ResourceObject<Convoy> {
    use chrono::TimeZone;
    let mut old = convoy("c", ConvoyPhase::Active, vec![agent("coder", true)]);
    old.metadata.creation_timestamp = Utc.timestamp_opt(1_700_000_000, 123_456_789).single().expect("time");
    let started = old.metadata.creation_timestamp + chrono::Duration::hours(1);
    old.status.as_mut().expect("status").started_at = Some(started);
    old
}

// Behaviour (#3108 creation time): an imported convoy is created at the old record's creation
// time to the nanosecond, not when it started, whichever write path imports it; its state keeps
// its start.
#[test]
fn an_imported_convoy_is_created_at_its_old_creation_time() {
    use flotilla_engine_store::ImportTarget;
    use flotilla_schema::{CreatedAt, Kind, Timestamp};
    use flotilla_state_machines::{machines::convoy::Convoy as Engine, Resource};
    let old = stamped_to_the_nanosecond();
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    let started = old.status.as_ref().and_then(|status| status.started_at).expect("started");
    assert_eq!(convoy.resource.since(), Timestamp::from_datetime(&started), "the state keeps its start");
    let created = CreatedAt::from_datetime(&old.metadata.creation_timestamp);

    let connection = rusqlite::Connection::open_in_memory().expect("connection");
    let mut store =
        flotilla_schema_sqlite::Store::open(connection, HOME, flotilla_engine_store::registry().expect("registry")).expect("store");
    let written = flotilla_engine_convoys::convoy_import::write(&mut store, &convoy).expect("written");
    let mut read = store.read().expect("read");
    let id = read.resolve(Resource::<Engine>::CODE, written.name(), HOME).expect("resolve").expect("created");
    assert_eq!(read.creation_time(id).expect("created"), created, "write");

    let mut import = flotilla_engine_store::Import::in_memory(HOME, flotilla_engine_store::registry().expect("registry")).expect("import");
    let written = flotilla_engine_convoys::convoy_import::write_into(&mut import, &convoy).expect("written");
    assert!(import.imports(HOME));
    let mut store = import.store();
    let mut read = store.read().expect("read");
    let id = read.resolve(Resource::<Engine>::CODE, written.name(), HOME).expect("resolve").expect("created");
    assert_eq!(read.creation_time(id).expect("created"), created, "write_into");
}

// Behaviour (#3108 creation time): an imported convoy's readers see the old record's creation
// timestamp exactly, sub-microsecond digits included, at its home and at a replica that installed
// it from the home's snapshot or applied its create: the managed checkout name hashes its RFC 3339
// form, so the name the old store gave its checkouts is the name every home computes.
#[test]
fn an_imported_convoy_reads_its_old_creation_timestamp_at_home_and_on_replicas() {
    use std::sync::Arc;

    use flotilla_engine_convoys::convoy_records::EngineConvoyRecords;
    use flotilla_resources::{
        checkout_path::{checkout_name, checkout_placement_scope},
        convoy_records::{ConvoyJoins, ConvoyRecords},
    };
    use flotilla_schema::Timestamp;
    use flotilla_schema_sqlite::stream::{Follower, Service};
    let old = stamped_to_the_nanosecond();
    let expected = old.metadata.creation_timestamp.to_rfc3339();
    let old_name = checkout_name("work", "flotilla", false, checkout_placement_scope(&old, Some("feta-host")).as_deref());
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    let dir = tempfile::tempdir().expect("tempdir");
    let open = |home: &str| {
        Arc::new(
            Service::open(&dir.path().join(format!("{home}.sqlite")), home, flotilla_engine_store::registry().expect("registry"))
                .expect("store"),
        )
    };
    let home = open(HOME);
    let written =
        home.with_store(|store| Ok(flotilla_engine_convoys::convoy_import::write(store, &convoy))).expect("store").expect("written");
    // `after` none serves the home's snapshot; `Some(0)` serves its entries, the create among them.
    let replica = |at: &str, after: Option<u64>| {
        let mut frames = Vec::new();
        home.serve(after, Timestamp(1), |frame| {
            frames.push(frame.to_vec());
            Ok(())
        })
        .expect("serve");
        let replica = open(at);
        replica
            .with_store(|store| {
                Follower::new(store).receive(HOME, |sink| {
                    for frame in &frames {
                        sink(frame)?;
                    }
                    Ok(())
                })
            })
            .expect("follow");
        replica
    };
    let readers = [
        ("home", HOME, Arc::clone(&home)),
        ("snapshot replica", "home-b", replica("home-b", None)),
        ("event replica", "home-c", replica("home-c", Some(0))),
    ];
    for (which, at, service) in readers {
        let stalls = flotilla_engine_messages::awaiting::OpenAwaiting::open(&service).expect("open records");
        let records = EngineConvoyRecords::new(at, service, Arc::default(), stalls);
        let record = records.get(written.name(), Some(HOME)).expect("read").expect("held");
        assert_eq!(record.admitted_at, old.metadata.creation_timestamp, "{which}: the old stamp exactly");
        let works = BTreeMap::new();
        let working = std::collections::BTreeSet::new();
        let view = record.read_view("flotilla", ConvoyJoins { works: &works, working: &working, notes: None });
        assert_eq!(view.metadata.creation_timestamp.to_rfc3339(), expected, "{which}: the RFC 3339 form the old store served");
        assert_eq!(
            checkout_name("work", "flotilla", false, checkout_placement_scope(&view, Some("feta-host")).as_deref()),
            old_name,
            "{which}: the managed checkout name the old store gave"
        );
    }
}

// Behaviour (the pre-roll rehearsal): every Convoy the stored-record corpus holds is imported,
// stays on the old path, or is refused by name, and every imported one writes and reads back.
// Every unfinished, bootstrapped convoy is imported or refused; ended ones stay old. (The current
// generation predates promises, so today its refusals are all NoObligations; a later corpus may
// name others, which this test leaves to the report.)
#[test]
fn the_stored_record_corpus_rehearses() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../flotilla-store-testkit/tests/fixtures/stored-records");
    let mut records = Vec::new();
    for generation in fs::read_dir(&root).expect("stored-record corpus") {
        let path = generation.expect("generation").path().join("Convoy.json");
        if !path.exists() {
            continue;
        }
        let documents: Vec<serde_json::Value> = serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("documents");
        for (index, document) in documents.into_iter().enumerate() {
            // The corpus is redacted to spec and status; name each record by its position.
            let object = serde_json::json!({
                "metadata": {
                    "name": format!("corpus-{index}"), "namespace": "flotilla", "resource_version": "1",
                    "labels": {}, "annotations": {}, "creation_timestamp": "2026-10-01T00:00:00Z",
                },
                "spec": document["spec"],
                "status": document.get("status").cloned().unwrap_or_default(),
            });
            records.push(serde_json::from_value::<ResourceObject<Convoy>>(object).expect("stored Convoy decodes"));
        }
    }
    assert!(!records.is_empty(), "the corpus holds convoys");
    let report = rehearse_all(&records, HOME, &[], &WORLD);
    assert_eq!(report.imported.len() + report.stays_old.len() + report.refused.len(), records.len());
    assert!(report.rehearsal.is_empty(), "rehearsal refusals: {:?}", report.rehearsal);
    let names: std::collections::BTreeSet<_> = records.iter().map(|record| record.metadata.name.clone()).collect();
    let named = |refusal: &ConvoyImportRefusal| match refusal {
        ConvoyImportRefusal::NoObligations { convoy }
        | ConvoyImportRefusal::StandingAwaitsRuling { convoy }
        | ConvoyImportRefusal::Unmappable { convoy, .. }
        | ConvoyImportRefusal::EndUnrecorded { convoy, .. }
        | ConvoyImportRefusal::EndRefused { convoy, .. } => names.contains(convoy),
    };
    assert!(report.refused.iter().all(named), "every refusal names a corpus convoy: {:?}", report.refused);
    let bootstrapped =
        records.iter().filter(|record| record.status.as_ref().is_some_and(|status| status.workflow_snapshot.is_some())).count();
    assert_eq!(
        report.imported.len() + report.refused.len(),
        bootstrapped,
        "every bootstrapped convoy, ended or not, is imported or listed"
    );
    eprintln!(
        "pre-roll rehearsal: {} imported, {} refused, {} stay old: {:?}",
        report.imported.len(),
        report.refused.len(),
        report.stays_old.len(),
        report.refused.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
}

// Behaviour (found by the ceiling benchmark): several convoys rehearse at one home; the store
// stamps each one's rows with its own write, and the read back compares their content.
#[test]
fn several_convoys_rehearse_at_one_home() {
    let convoys: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|name| {
            let mut old = convoy(name, ConvoyPhase::Active, vec![agent("coder", true)]);
            operate(&mut old, "coder", submit("template/pr", PromiseKind::Pr, PR));
            imported(import_convoy(&old, HOME, &[], &WORLD))
        })
        .collect();
    assert_eq!(rehearse(&convoys), Vec::new());
}

/// `convoy` ended as the old store records an abandonment: its reason and when it finished. (A
/// Landed one with its promises open, as these fixtures hold them, is no delivery the engine
/// accepts.)
fn ended(convoy: &mut ResourceObject<Convoy>) {
    let status = convoy.status.as_mut().expect("status");
    status.phase = ConvoyPhase::Abandoned;
    status.message = Some("superseded by the next generation".into());
    status.finished_at = Some(Utc::now());
}

// Behaviour: each standing generation is its own convoy, a launch of the role's engine name. The
// live generation imports with its own promise ids; an ended one imports with its end, as any
// ended convoy does (#3108). Each is numbered in generation order, so the live one is the newest.
#[test]
fn a_standing_generation_imports_as_a_launch_of_its_role() {
    use flotilla_engine_convoys::convoy_import::{rehearse_all, write};
    let mut old = convoy("governor-1", ConvoyPhase::Landed, vec![agent("governor", true)]);
    let mut live = convoy("governor-2", ConvoyPhase::Active, vec![agent("governor", true)]);
    for (generation, record) in [(1, &mut old), (2, &mut live)] {
        record.spec.role = "governor".into();
        record.spec.generation = generation;
        record.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "ensure-governor".into());
    }
    ended(&mut old);
    let report = rehearse_all([&live, &old], HOME, &[], &World { standing_ruled: true });
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert!(report.rehearsal.is_empty(), "{:?}", report.rehearsal);
    assert!(report.stays_old.is_empty(), "{:?}", report.stays_old);
    // Each generation is a launch, numbered in record order: the ended one first, with its end.
    let [ended, imported] = &report.imported[..] else { panic!("both generations import: {:?}", report.imported) };
    assert_eq!((ended.record.as_str(), ended.launch, ended.end.is_some()), ("governor-1", 1, true));
    assert_eq!(imported.launch, 2);
    assert_eq!(imported.resource.spec().launch_of, "project:fleet/convoy:governor");
    assert_eq!(imported.resource.spec().seats.len(), 1);
    assert_eq!(imported.resource.spec().promises.iter().next().unwrap().record.id.0, 0);
    // The launch names the generation it was imported from: the record whose checkouts it holds.
    assert_eq!(imported.record, "governor-2");
    AdmissionDocument::decode("governor", imported.resource.spec().admission.value().bytes()).expect("admission document");

    let connection = rusqlite::Connection::open_in_memory().expect("connection");
    let mut store =
        flotilla_schema_sqlite::Store::open(connection, HOME, flotilla_engine_store::registry().expect("registry")).expect("store");
    let written = write(&mut store, imported).expect("written");
    // Named by the launch reserved for it, whatever sequence its create takes.
    assert_eq!(written.name(), "project:fleet/convoy:governor#2");
    assert_eq!(written.spec().launch, 2);

    live.spec.generation = 1;
    let duplicate = rehearse_all([&live, &old], HOME, &[], &World { standing_ruled: true });
    assert!(duplicate.imported.is_empty());
    assert!(matches!(&duplicate.refused[..], [ConvoyImportRefusal::Unmappable { reason, .. }] if reason.contains("duplicate")));
}

#[test]
fn a_standing_import_refuses_an_earlier_live_generation() {
    let mut old = convoy("governor-1", ConvoyPhase::Active, vec![agent("governor", true)]);
    let mut live = convoy("governor-2", ConvoyPhase::Active, vec![agent("governor", true)]);
    for (generation, record) in [(1, &mut old), (2, &mut live)] {
        record.spec.role = "governor".into();
        record.spec.generation = generation;
        record.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "ensure-governor".into());
    }
    ended(&mut old);
    let report = rehearse_all([&old, &live], HOME, &[], &World { standing_ruled: true });
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.imported.len(), 2, "the ended generation imports with its end");
    old.status.as_mut().unwrap().phase = ConvoyPhase::Active;
    live.status.as_mut().unwrap().phase = ConvoyPhase::Landed;
    let report = rehearse_all([&old, &live], HOME, &[], &World { standing_ruled: true });
    assert!(report.imported.is_empty());
    assert!(matches!(&report.refused[..], [ConvoyImportRefusal::Unmappable { reason, .. }] if reason.contains("still live")));
    // Two live generations: the earlier would be a second open convoy of the role.
    live.status.as_mut().unwrap().phase = ConvoyPhase::Active;
    let report = rehearse_all([&old, &live], HOME, &[], &World { standing_ruled: true });
    assert!(report.imported.is_empty());
    assert!(matches!(&report.refused[..], [ConvoyImportRefusal::Unmappable { reason, .. }] if reason.contains("still live")));
}
