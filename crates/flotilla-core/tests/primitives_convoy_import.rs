//! The Convoy import and its pre-roll rehearsal (#3587 layer 9, #3574
//! decision 11): an old convoy mapped onto the engine's record, checked
//! against the machine and written through the creation path.

use std::{collections::BTreeMap, fs, path::Path};

use chrono::Utc;
use flotilla_core::primitives::convoy_import::{
    check, import_convoy, rehearse, rehearse_all, ConvoyImportRefusal, ConvoyImportWorld, ImportOutcome, ImportedConvoy,
};
use flotilla_resources::{
    promises::{self, PromiseKind, PromiseOperation, PromiseSource, Submission, SubmissionVerdict},
    Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, LeafMaker, ObjectMeta,
    ResourceObject, Selector, StallEvidenceSource, StallRung, StalledCondition, VesselRequirement, WorkflowSnapshot,
    ENSURED_FROM_ANNOTATION,
};
use flotilla_schema::Address;
use flotilla_state_machines::machines::{
    convoy::{DutyScope, Progress, SeatName},
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
            let inbox: Address = "project:fleet/convoy:c/role:bosun".parse().expect("inbox");
            (MessageRef::new(format!("{inbox}/message:supervision-1"), HOME), inbox)
        })
    }
    fn standing_ruled(&self) -> bool {
        self.standing_ruled
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
    for (vessel, role, operation) in promises::admitted_declarations(&[], &status) {
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
        submission: Submission { reference: reference.into(), metadata: BTreeMap::new(), submitted_at: Utc::now(), verdict: None },
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
    assert_eq!(labels, vec!["template/pr", "@coder/notes", "@reviewer/notes"]);
    assert!(promises.iter().skip(1).all(|promise| flotilla_core::primitives::convoy_map::old_id(promise) == "notes"));
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
}

// Behaviour: a finite, unfinished convoy with no promise is the named refusal NoObligations; an ended
// or unbootstrapped convoy stays on the old path.
#[test]
fn unfinished_convoys_without_promises_are_refused_by_name() {
    let idle = convoy("idle", ConvoyPhase::Active, vec![agent("reviewer", false)]);
    assert_eq!(import_convoy(&idle, HOME, &[], &WORLD), Err(ConvoyImportRefusal::NoObligations { convoy: "idle".into() }));
    let landed = convoy("landed", ConvoyPhase::Landed, vec![agent("reviewer", false)]);
    assert!(matches!(import_convoy(&landed, HOME, &[], &WORLD), Ok(ImportOutcome::StaysOld(_))));
    let mut fresh = convoy("fresh", ConvoyPhase::Pending, vec![agent("coder", true)]);
    fresh.status.as_mut().expect("status").workflow_snapshot = None;
    assert!(matches!(import_convoy(&fresh, HOME, &[], &WORLD), Ok(ImportOutcome::StaysOld(_))));
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

// Behaviour: an open stall that a supervision message carries becomes the stalled seat's open
// stall record, awaiting that message. One with no message, and one at the operator rung even with
// a message (no record until #3457, ruling 6353447431), records nothing and is reported.
#[test]
fn an_open_stall_becomes_the_seats_stall_record() {
    let mut old = convoy("c", ConvoyPhase::Active, vec![agent("coder", true)]);
    let stall = |message: Option<&str>, rung: StallRung| StalledCondition {
        leaves: Vec::new(),
        maker: Some(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() }),
        evidence: "blocked".into(),
        source: StallEvidenceSource::Crew,
        cause: None,
        began_at: Utc::now(),
        rung,
        supervisor: None,
        supervision_message: message.map(|name| flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", name)),
        supervision_index: Some(0),
        supervision_exhausted: false,
        reason: None,
        proposed_disposition: None,
        nudge_history: Vec::new(),
    };
    old.status.as_mut().expect("status").stalled = Some(stall(Some("supervision-1"), StallRung::Bosun));
    let convoy = imported(import_convoy(&old, HOME, &[], &WORLD));
    let record = convoy.stall.as_ref().expect("a stall record").spec.clone();
    assert_eq!(record.kind, "stall");
    assert_eq!(record.subject.as_ref().map(Address::as_str), Some("project:fleet/convoy:c/role:coder"));
    assert_eq!(record.recipient.as_str(), "project:fleet/convoy:c/role:bosun");
    // The import writes the record beside the convoy, Open, and the rehearsal reads it back.
    assert_eq!(rehearse(std::slice::from_ref(&convoy)), Vec::new());
    // Numeric engine IDs are internal to the promise table. A carried stall
    // names a seat and a message, so folding must not rewrite either reference.
    let mut standing = old.clone();
    standing.spec.role = "coder".into();
    standing.spec.generation = 2;
    standing.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "ensure-coder".into());
    let mut earlier = standing.clone();
    earlier.metadata.name = "c-previous".into();
    earlier.spec.generation = 1;
    earlier.status.as_mut().unwrap().phase = ConvoyPhase::Landed;
    let folded = rehearse_all([&earlier, &standing], HOME, &[], &World { standing_ruled: true });
    assert!(folded.refused.is_empty(), "{:?}", folded.refused);
    assert!(folded.rehearsal.is_empty(), "{:?}", folded.rehearsal);
    let folded = &folded.imported[0];
    assert_eq!(folded.resource.spec().promises.iter().next().unwrap().record.id.0, 1);
    let carried = &folded.stall.as_ref().unwrap().spec;
    assert_eq!(carried.message, record.message);
    assert_eq!(carried.subject.as_ref().unwrap().as_str(), "project:fleet/convoy:coder/role:coder");

    let connection = rusqlite::Connection::open_in_memory().expect("connection");
    let mut store =
        flotilla_schema_sqlite::Store::open(connection, HOME, flotilla_engine_store::registry().expect("registry")).expect("store");
    flotilla_core::primitives::convoy_import::write(&mut store, &convoy).expect("written");
    let name = flotilla_core::primitives::awaiting::record_name(&record.message);
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
    flotilla_core::primitives::convoy_import::write(&mut store, &convoy).expect("written");
    let mut read = store.read().expect("read");
    let id = read.resolve(Resource::<Engine>::CODE, "c", HOME).expect("resolve").expect("created");
    let events = read.changes(id, 0, 10).expect("log");
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0].cause, Cause::Create));
    let change = read.change(HOME, events[0].change).expect("read").expect("its change");
    assert_eq!((change.writer.as_str(), change.at), ("import", convoy.resource.since()));
    assert!(convoy.history.crews[0].claim.contains("first attempt"), "the import still reads the claim: {:?}", convoy.history);
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
        | ConvoyImportRefusal::Unmappable { convoy, .. } => names.contains(convoy),
    };
    assert!(report.refused.iter().all(named), "every refusal names a corpus convoy: {:?}", report.refused);
    let unfinished = records
        .iter()
        .filter(|record| {
            record.status.as_ref().is_some_and(|status| {
                status.workflow_snapshot.is_some()
                    && !matches!(status.phase, ConvoyPhase::Landed | ConvoyPhase::Failed | ConvoyPhase::Abandoned)
            })
        })
        .count();
    assert_eq!(report.imported.len() + report.refused.len(), unfinished, "every unfinished convoy is imported or listed");
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

// Seat growth folds old per-generation records into one stable engine identity.
#[test]
fn standing_generations_fold_into_one_stable_convoy() {
    use flotilla_core::primitives::convoy_import::rehearse_all;
    let mut old = convoy("governor-1", ConvoyPhase::Landed, vec![agent("governor", true)]);
    let mut live = convoy("governor-2", ConvoyPhase::Active, vec![agent("governor", true)]);
    for (generation, record) in [(1, &mut old), (2, &mut live)] {
        record.spec.role = "governor".into();
        record.spec.generation = generation;
        record.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "ensure-governor".into());
    }
    let report = rehearse_all([&live, &old], HOME, &[], &World { standing_ruled: true });
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert!(report.rehearsal.is_empty(), "{:?}", report.rehearsal);
    assert_eq!(report.imported.len(), 1);
    assert_eq!(report.folded_generations, 1);
    let imported = &report.imported[0];
    assert_eq!(imported.resource.name(), "project:fleet/convoy:governor");
    assert_eq!(imported.history.previous_generations, vec!["governor-1"]);
    assert_eq!(imported.resource.spec().seats.len(), 1);
    assert_eq!(imported.resource.spec().promise_floor, 1);
    assert_eq!(imported.resource.spec().promises.iter().next().unwrap().record.id.0, 1);
    live.spec.generation = 1;
    let duplicate = rehearse_all([&live, &old], HOME, &[], &World { standing_ruled: true });
    assert!(duplicate.imported.is_empty());
    assert!(matches!(&duplicate.refused[..], [ConvoyImportRefusal::Unmappable { reason, .. }] if reason.contains("duplicate")));
}

#[test]
fn standing_fold_counts_retracted_ids_and_refuses_an_earlier_live_generation() {
    let mut old = convoy("governor-1", ConvoyPhase::Active, vec![agent("governor", true)]);
    let mut live = convoy("governor-2", ConvoyPhase::Active, vec![agent("governor", true)]);
    for (generation, record) in [(1, &mut old), (2, &mut live)] {
        record.spec.role = "governor".into();
        record.spec.generation = generation;
        record.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "ensure-governor".into());
    }
    operate(&mut old, "governor", PromiseOperation::Declare { id: "extra".into(), kind: PromiseKind::Pr, source: PromiseSource::Crew });
    operate(&mut old, "governor", PromiseOperation::Retract { id: "extra".into(), reason: "ended work".into() });
    assert_eq!(promises::owned(old.status.as_ref().unwrap(), "work", "governor").last().unwrap().state, promises::PromiseState::Retracted);
    old.status.as_mut().unwrap().phase = ConvoyPhase::Landed;
    let report = rehearse_all([&old, &live], HOME, &[], &World { standing_ruled: true });
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.imported[0].resource.spec().promise_floor, 2);
    assert_eq!(report.imported[0].resource.spec().promises.iter().next().unwrap().record.id.0, 2);
    old.status.as_mut().unwrap().phase = ConvoyPhase::Active;
    live.status.as_mut().unwrap().phase = ConvoyPhase::Landed;
    let report = rehearse_all([&old, &live], HOME, &[], &World { standing_ruled: true });
    assert!(report.imported.is_empty());
    assert!(matches!(&report.refused[..], [ConvoyImportRefusal::Unmappable { reason, .. }] if reason.contains("earlier live generation")));
}
