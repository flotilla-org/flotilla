//! The engine-first Convoy write path behind `[primitives]` (#3587 layer 6):
//! admission, crew verbs, observers, verdicts, handoff and delivery write the
//! engine, its refusals come back to the caller, and every engine write is
//! copied one way into the old status.

use std::{collections::BTreeMap, sync::Arc};

use chrono::Utc;
use flotilla_core::primitives::{convoy_engine::ConvoyEngine, convoy_map, convoys::Convoys};
use flotilla_resources::{
    convoy::engine::{Actor, Admission, EngineConvoys, VerdictScope},
    promises::{self, PromiseKind, PromiseOperation, PromiseSource as OldSource, PromiseState, Submission, SubmissionVerdict},
    Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, InputMeta, ResourceObject,
    SystemClock, VesselRequirement, WorkflowSnapshot,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::machines::convoy::{ConvoyLabel, Progress, SeatName};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const PR: &str = "cr/github.com/org/repo/5";

struct Fixture {
    _dir: tempfile::TempDir,
    backend: ResourceBackend,
    convoys: Arc<Convoys>,
    engine: ConvoyEngine,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), "home-a", flotilla_engine_store::registry().expect("registry")).expect("store"),
    );
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = Arc::new(Convoys::open("home-a", &service).expect("convoys"));
    let engine = ConvoyEngine::new(Arc::clone(&convoys), backend.clone(), Arc::new(SystemClock));
    Fixture { _dir: dir, backend, convoys, engine }
}

fn crew(role: &str, deliverer: bool) -> CrewSpec {
    CrewSpec::builder().role(role.to_string()).source(CrewSource::Tool { command: "test".into() }).deliverer(deliverer).build()
}

/// Old status as Bootstrap leaves it: the snapshot, every crew Working, and
/// the admitted declarations seeded Open.
fn bootstrapped(vessels: Vec<(&str, Vec<CrewSpec>)>) -> ConvoyStatus {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: vessels
                .iter()
                .map(|(name, crew)| VesselRequirement::builder().name(name.to_string()).crew(crew.clone()).build())
                .collect(),
        }),
        crew_work: vessels
            .iter()
            .map(|(name, crew)| {
                (
                    name.to_string(),
                    crew.iter().map(|crew| (crew.role.clone(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())).collect(),
                )
            })
            .collect(),
        ..Default::default()
    };
    for (vessel, role, operation) in promises::admitted_declarations(&[], &status) {
        promises::apply(&mut status, &vessel, &role, &operation);
    }
    status
}

async fn old_convoy(backend: &ResourceBackend, name: &str, status: ConvoyStatus) -> ResourceObject<Convoy> {
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let created = convoys
        .create(
            &InputMeta::builder().name(name.to_string()).build(),
            &ConvoySpec::builder().workflow_ref("w".to_string()).project_ref("fleet".to_string()).build(),
        )
        .await
        .expect("convoy");
    convoys.update_status(name, &created.metadata.resource_version, &status).await.expect("status")
}

async fn reread(backend: &ResourceBackend, name: &str) -> ResourceObject<Convoy> {
    backend.clone().using::<Convoy>(NAMESPACE).get(name).await.expect("convoy")
}

fn old_promise<'a>(convoy: &'a ResourceObject<Convoy>, vessel: &str, role: &str, id: &str) -> &'a promises::Promise {
    promises::owned(convoy.status.as_ref().expect("status"), vessel, role).iter().find(|promise| promise.id == id).expect("old promise")
}

fn submit(id: &str, reference: &str) -> PromiseOperation {
    PromiseOperation::Submit {
        id: id.into(),
        kind: PromiseKind::Pr,
        source: OldSource::Crew,
        submission: Submission { reference: reference.into(), metadata: BTreeMap::new(), submitted_at: Utc::now(), verdict: None },
    }
}

fn merged(id: &str) -> PromiseOperation {
    PromiseOperation::Verdict {
        id: id.into(),
        reference: PR.into(),
        submitted_at: Utc::now(),
        verdict: SubmissionVerdict { accepted: true, who: "forge".into(), at: Utc::now(), why: "PR merged".into() },
    }
}

async fn admitted(fixture: &Fixture, name: &str, status: ConvoyStatus) -> ResourceObject<Convoy> {
    let convoy = old_convoy(&fixture.backend, name, status).await;
    assert_eq!(fixture.engine.admit(&convoy).await.expect("admit"), Admission::Held);
    reread(&fixture.backend, name).await
}

// Behaviour: a freshly bootstrapped convoy is admitted on the engine with one seat per crew and
// its seeded promises under their old ids as labels; the copy back reproduces the seeded old
// promises exactly, so it writes nothing, and a second admission is the first.
#[tokio::test]
async fn a_fresh_convoy_is_admitted_with_its_seeded_promises() {
    let fixture = fixture();
    let status = bootstrapped(vec![("work", vec![crew("coder", true), crew("reviewer", false)])]);
    let seeded = status.promises.clone();
    let convoy = admitted(&fixture, "c", status).await;
    let engine = fixture.convoys.get("c").expect("held");
    let promises: Vec<_> = engine.spec().promises.iter().map(|row| (row.record.label.clone(), row.record.owner.clone())).collect();
    assert_eq!(promises, vec![("template/pr".to_owned(), SeatName::new("coder", ""))]);
    assert_eq!(engine.spec().seats.len(), 2);
    assert_eq!(convoy.status.as_ref().expect("status").promises, seeded, "the copy reproduces the seeded promises");
    assert!(!fixture.engine.copy_back(&convoy).await.expect("copy"), "nothing to copy");
    assert_eq!(fixture.engine.admit(&convoy).await.expect("again"), Admission::Held);
}

// Behaviour: the crew's submission and the forge's merge observation become the engine's Submit
// and observed Keep; the old status follows each, and delivery is refused until nothing is open.
#[tokio::test]
async fn a_submitted_pr_is_kept_by_its_merge_and_the_convoy_delivers() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    let refused = fixture.engine.deliver(&convoy).await.expect_err("an open promise holds delivery");
    assert!(refused.contains("PromisesOutstanding"), "{refused}");

    fixture.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("submitted");
    let convoy = reread(&fixture.backend, "c").await;
    let copied = old_promise(&convoy, "work", "coder", "template/pr");
    assert_eq!(copied.state, PromiseState::Submitted);
    assert_eq!(copied.submissions.len(), 1);
    assert!(copied.submissions[0].names(&copied.submissions[0].subject_address(&[]).expect("cr"), &[]));
    // The same submission again writes nothing.
    fixture.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("again");
    let engine = fixture.convoys.get("c").expect("held");
    assert_eq!(engine.spec().promises.iter().next().expect("promise").record.submissions.len(), 1);

    fixture.engine.promise(&convoy, "work", "coder", &merged("template/pr"), Actor::Observer).await.expect("kept");
    let convoy = reread(&fixture.backend, "c").await;
    assert_eq!(old_promise(&convoy, "work", "coder", "template/pr").state, PromiseState::Kept);
    fixture.engine.deliver(&convoy).await.expect("delivered");
    assert_eq!(fixture.convoys.get("c").expect("held").label(), ConvoyLabel::Delivered);
    fixture.engine.deliver(&convoy).await.expect("delivering again is no error");
}

// Behaviour: the engine's refusals reach the caller and write nothing: a crew may not retract a
// template promise, an observed merge needs a current submission, and another seat may not
// submit against a promise it does not own.
#[tokio::test]
async fn refusals_come_back_to_the_caller() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true), crew("reviewer", false)])])).await;
    let retract = PromiseOperation::Retract { id: "template/pr".into(), reason: "no longer needed".into() };
    let refused = fixture.engine.promise(&convoy, "work", "coder", &retract, Actor::Crew).await.expect_err("template retraction");
    assert!(refused.contains("Unauthorised"), "{refused}");
    let refused = fixture.engine.promise(&convoy, "work", "coder", &merged("template/pr"), Actor::Observer).await.expect_err("stale");
    assert!(refused.contains("StaleEvidence"), "{refused}");
    let engine = fixture.convoys.get("c").expect("held");
    assert_eq!(engine.spec().promises.iter().next().expect("promise").record.progress.value, Progress::Pending);
    assert_eq!(old_promise(&reread(&fixture.backend, "c").await, "work", "coder", "template/pr").state, PromiseState::Open);
}

// Behaviour: a crew-declared promise is the owner's to retract before anything is submitted, and
// a submission against an id the crew never declared declares it first, as the old path did.
#[tokio::test]
async fn crew_declared_promises_follow_the_crew() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    let declare = PromiseOperation::Declare { id: "l2-quint".into(), kind: PromiseKind::Pr, source: OldSource::Crew };
    fixture.engine.promise(&convoy, "work", "coder", &declare, Actor::Crew).await.expect("declared");
    let retract = PromiseOperation::Retract { id: "l2-quint".into(), reason: "folded into l3".into() };
    fixture.engine.promise(&convoy, "work", "coder", &retract, Actor::Crew).await.expect("retracted");
    fixture.engine.promise(&convoy, "work", "coder", &submit("extra", "cr/github.com/org/repo/9"), Actor::Crew).await.expect("implicit");
    let convoy = reread(&fixture.backend, "c").await;
    assert_eq!(old_promise(&convoy, "work", "coder", "l2-quint").state, PromiseState::Retracted);
    assert_eq!(old_promise(&convoy, "work", "coder", "extra").state, PromiseState::Submitted);
    // A retracted id is never reused: declaring it again is a no-op on the same promise.
    fixture.engine.promise(&convoy, "work", "coder", &declare, Actor::Crew).await.expect("existing");
    assert_eq!(fixture.convoys.get("c").expect("held").spec().promises.len(), 3);
}

// Behaviour: a role repeated across vessels gets one seat per vessel, and its repeated template
// ids are qualified labels; the old status keeps the ids crews submit against, and each crew's
// submission reaches its own promise.
#[tokio::test]
async fn repeated_roles_get_seats_and_qualified_labels() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("a", vec![crew("coder", true)]), ("b", vec![crew("coder", true)])])).await;
    let engine = fixture.convoys.get("c").expect("held");
    let labels: Vec<_> = engine.spec().promises.iter().map(|row| row.record.label.clone()).collect();
    assert_eq!(labels, vec!["@coder~a/template/pr".to_owned(), "@coder~b/template/pr".to_owned()]);
    assert_eq!(convoy_map::old_id(&labels[1]), "template/pr");
    fixture.engine.promise(&convoy, "b", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("submitted");
    let convoy = reread(&fixture.backend, "c").await;
    assert_eq!(old_promise(&convoy, "a", "coder", "template/pr").state, PromiseState::Open);
    assert_eq!(old_promise(&convoy, "b", "coder", "template/pr").state, PromiseState::Submitted);
}

// Behaviour: handoff reassigns the sender's open promises to the target's seat, adding a seat
// under the sender for a crew the convoy has none for; the old status shows them under the target.
#[tokio::test]
async fn handoff_reassigns_open_promises() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true), crew("reviewer", false)])])).await;
    fixture.engine.reassign(&convoy, "work", "coder", "reviewer").await.expect("reassigned");
    let convoy = reread(&fixture.backend, "c").await;
    assert_eq!(old_promise(&convoy, "work", "reviewer", "template/pr").state, PromiseState::Open);
    fixture.engine.reassign(&convoy, "work", "reviewer", "fixer").await.expect("reassigned to a new seat");
    let engine = fixture.convoys.get("c").expect("held");
    let (_, fixer) = engine.spec().seat(&SeatName::new("fixer", "work")).expect("fan-out seat");
    assert_eq!(fixer.parent, Some(SeatName::new("reviewer", "")));
    assert_eq!(engine.spec().promises.iter().next().expect("promise").record.owner, SeatName::new("fixer", "work"));
}

// Behaviour: an operator keeps a crew's open promises by verdict (the old `--force`), or keeps or
// retracts one promise by its label; each verdict needs a reason.
#[tokio::test]
async fn operator_verdicts_keep_and_retract() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("a", vec![crew("coder", true)]), ("b", vec![crew("coder", true)])])).await;
    let scope = VerdictScope { vessel: "a".into(), role: Some("coder".into()) };
    let kept = fixture.engine.keep_by_verdict(&convoy, &scope, "alice", "merged by hand").await.expect("kept");
    assert_eq!(kept, vec!["@coder~a/template/pr".to_owned()]);
    let refused = fixture.engine.verdict(&convoy, "@coder~b/template/pr", false, "alice", "").await.expect_err("no reason");
    assert!(refused.contains("Unauthorised"), "{refused}");
    fixture.engine.verdict(&convoy, "@coder~b/template/pr", false, "alice", "dropped").await.expect("retracted");
    let convoy = reread(&fixture.backend, "c").await;
    assert_eq!(old_promise(&convoy, "a", "coder", "template/pr").state, PromiseState::Kept);
    assert_eq!(old_promise(&convoy, "b", "coder", "template/pr").state, PromiseState::Retracted);
    fixture.engine.deliver(&convoy).await.expect("nothing open");
}

// Behaviour: only a convoy with nothing decided is admitted; one with history, or a standing one,
// stays on the old path for the import, and a convoy with no obligation is refused by name.
#[tokio::test]
async fn admission_skips_history_and_refuses_no_obligations() {
    let fixture = fixture();
    let mut history = bootstrapped(vec![("work", vec![crew("coder", true)])]);
    promises::apply(&mut history, "work", "coder", &submit("template/pr", PR));
    let convoy = old_convoy(&fixture.backend, "history", history).await;
    assert!(matches!(fixture.engine.admit(&convoy).await.expect("admit"), Admission::Skipped(_)));
    assert!(!fixture.engine.holds(&convoy));

    let convoy = old_convoy(&fixture.backend, "idle", bootstrapped(vec![("work", vec![crew("reviewer", false)])])).await;
    assert_eq!(fixture.engine.admit(&convoy).await.expect("admit"), Admission::Refused("NoObligations".into()));
    assert!(!fixture.engine.holds(&convoy));
}

// Behaviour: abandonment is the engine's Abandon; an ended engine convoy has nothing to abandon.
#[tokio::test]
async fn abandonment_ends_the_engine_convoy() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    fixture.engine.abandon(&convoy, "alice", "superseded").await.expect("abandoned");
    let engine = fixture.convoys.get("c").expect("held");
    assert_eq!(engine.state().reason(), Some("superseded"));
    fixture.engine.abandon(&convoy, "alice", "again").await.expect("already ended");
}

// Behaviour (dummy fleet, #3587 layer 6): two crews may each declare and submit a promise of the
// same id; each reaches its own, the second under a qualified label, even once the first is kept.
#[tokio::test]
async fn two_crews_submit_promises_of_one_id() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true), crew("reviewer", false)])])).await;
    let ledger = |by: &str| PromiseOperation::Submit {
        id: "script-ledger".into(),
        kind: PromiseKind::DecisionLedger,
        source: OldSource::Crew,
        submission: Submission { reference: format!("{by}-ledger"), metadata: BTreeMap::new(), submitted_at: Utc::now(), verdict: None },
    };
    fixture.engine.promise(&convoy, "work", "coder", &ledger("coder"), Actor::Crew).await.expect("coder's ledger");
    let kept = PromiseOperation::Verdict {
        id: "script-ledger".into(),
        reference: String::new(),
        submitted_at: Utc::now(),
        verdict: SubmissionVerdict { accepted: true, who: "artifact".into(), at: Utc::now(), why: "exists".into() },
    };
    fixture.engine.promise(&convoy, "work", "coder", &kept, Actor::Observer).await.expect("kept");
    fixture.engine.promise(&convoy, "work", "reviewer", &ledger("reviewer"), Actor::Crew).await.expect("reviewer's ledger");
    let convoy = reread(&fixture.backend, "c").await;
    assert_eq!(old_promise(&convoy, "work", "coder", "script-ledger").state, PromiseState::Kept);
    assert_eq!(old_promise(&convoy, "work", "reviewer", "script-ledger").state, PromiseState::Submitted);
    let labels: Vec<_> = fixture.convoys.get("c").expect("held").spec().promises.iter().map(|row| row.record.label.clone()).collect();
    assert!(labels.contains(&"@reviewer/script-ledger".to_owned()), "{labels:?}");
    // The same reference again is the submission already made.
    fixture.engine.promise(&convoy, "work", "coder", &ledger("coder"), Actor::Crew).await.expect("kept: a no-op");
}

// Behaviour (#3721 review): an operator's promise operation carries operator authority, so it
// may retract a template promise the crew may not, and its accepted verdict is a `Keep` verdict.
#[tokio::test]
async fn an_operator_decides_with_its_own_authority() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    let retract = PromiseOperation::Retract { id: "template/pr".into(), reason: "superseded".into() };
    let refused = fixture.engine.promise(&convoy, "work", "coder", &retract, Actor::Crew).await.expect_err("the crew may not");
    assert!(refused.contains("Unauthorised"), "{refused}");
    let operator = Actor::Operator { principal: "alice".into() };
    fixture.engine.promise(&convoy, "work", "coder", &retract, operator).await.expect("the operator may");
    assert_eq!(old_promise(&reread(&fixture.backend, "c").await, "work", "coder", "template/pr").state, PromiseState::Retracted);
}

// Behaviour (#3721 review): a verdict names one promise. An exact label always does; a bare old
// id shared by several promises is refused with the labels to choose from, never guessed.
#[tokio::test]
async fn an_ambiguous_verdict_is_refused_with_the_labels() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("a", vec![crew("coder", true)]), ("b", vec![crew("coder", true)])])).await;
    let refused = fixture.engine.verdict(&convoy, "template/pr", true, "alice", "merged").await.expect_err("ambiguous");
    assert!(refused.contains("`@coder~a/template/pr`") && refused.contains("`@coder~b/template/pr`"), "{refused}");
    fixture.engine.verdict(&convoy, "@coder~b/template/pr", true, "alice", "merged").await.expect("exact");
    let convoy = reread(&fixture.backend, "c").await;
    assert_eq!(old_promise(&convoy, "a", "coder", "template/pr").state, PromiseState::Open);
    assert_eq!(old_promise(&convoy, "b", "coder", "template/pr").state, PromiseState::Kept);
}

// Behaviour (#3721 review): seats are fixed by the frozen snapshot. A crew of a role that joins
// another vessel later is seated under its vessel, and the admitted crew keeps its seat.
#[test]
fn a_later_crew_of_a_seated_role_never_renames_the_seat() {
    let mut status = bootstrapped(vec![("a", vec![crew("coder", true)])]);
    let crews = convoy_map::Crews::of(&status);
    assert_eq!(crews.seat("a", "coder"), SeatName::new("coder", ""));
    status.crew_work.entry("b".into()).or_default().insert("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build());
    let crews = convoy_map::Crews::of(&status);
    assert_eq!(crews.seat("a", "coder"), SeatName::new("coder", ""), "unchanged");
    assert_eq!(crews.seat("b", "coder"), SeatName::new("coder", "b"));
    assert_eq!(crews.crew(&SeatName::new("coder", "")), Some(("a", "coder")));
    assert_eq!(crews.crew(&SeatName::new("coder", "b")), Some(("b", "coder")));
}

// Behaviour (#3721 review): an edit refused part-way says how many of its edges applied, and
// those stay applied; its recorded inputs are cleared, so the world does not grow.
#[test]
fn a_partial_edit_reports_what_applied() {
    use flotilla_core::primitives::convoys::EditError;
    use flotilla_state_machines::{
        machines::convoy::{ConvoyRefusal, Declare, PromiseId, Submit},
        Ref, Source,
    };
    let fixture = fixture();
    let request =
        convoy_map::request(&tokio_block(old_convoy(&fixture.backend, "c", bootstrapped(vec![("work", vec![crew("coder", true)])]))))
            .expect("request");
    fixture.convoys.world(|world| world.projects.put(&Ref::new(request.project.as_str()), ()));
    assert!(fixture.convoys.admit("c", request, flotilla_schema::Timestamp(1)).expect("admitted"));
    let owner = SeatName::new("coder", "");
    let target: flotilla_schema::Address = "project:fleet/convoy:c".parse().expect("target");
    let error = fixture
        .convoys
        .edit("c", flotilla_schema::Timestamp(2), |_, world| {
            world.durable.put(&Ref::new("other"), true);
            let declare =
                Declare { promise: PromiseId(9), label: "l2".into(), kind: "pr".into(), target: target.clone(), owner: owner.clone() };
            // Not recorded as durable: refused after the Declare applied.
            let submit = Submit {
                promise: PromiseId(9),
                by: owner.clone(),
                address: target.clone(),
                revision: "r".into(),
                durable: Ref::new("unrecorded"),
            };
            Ok((vec![declare.into(), submit.into()], ()))
        })
        .expect_err("not durable");
    assert!(matches!(&error, EditError::Refused { refusal: ConvoyRefusal::NotDurable, applied: 1, .. }), "{error:?}");
    assert!(fixture.convoys.get("c").expect("held").spec().promise(PromiseId(9)).is_some(), "the declare stays");
    assert!(fixture.convoys.world(|world| world.durable.lookup(&Ref::new("other")).is_none()), "inputs cleared");
}

fn tokio_block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(future)
}
