//! The engine-first Convoy write path (#3587 layer 6):
//! admission, crew verbs, observers, verdicts, handoff and delivery write the
//! engine, its refusals come back to the caller, and every engine write is
//! copied one way into the old status.

use std::{collections::BTreeMap, sync::Arc};

use chrono::Utc;
use flotilla_engine_convoys::{convoy_engine::ConvoyEngine, convoy_map, convoy_records::EngineConvoyRecords, convoys::Convoys};
use flotilla_engine_messages::awaiting::OpenAwaiting;
use flotilla_engine_messages_testkit::{designate_fleet, intent, EngineMessagesHome};
use flotilla_provisioning::stored::checkout::CheckoutIntegration;
use flotilla_resources::{
    convoy::engine::{Actor, Delivery, EngineConvoys, SettlementEvidence, VerdictScope},
    convoy_records::{ConvoyEnd, ConvoyRecords},
    promises::{self, PromiseKind, PromiseOperation, PromiseSource as OldSource, PromiseState, Submission, SubmissionVerdict},
    Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, InputMeta, MessageRelation,
    ResourceObject, SystemClock, VesselRequirement, WorkflowSnapshot,
};
use flotilla_schema::{NewChange, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::machines::convoy::{ConvoyLabel, Progress, SeatName};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const PR: &str = "cr/github.com/org/repo/5";

struct Fixture {
    _dir: tempfile::TempDir,
    backend: ResourceBackend,
    service: Arc<Service>,
    convoys: Arc<Convoys>,
    engine: ConvoyEngine,
}

fn fixture() -> Fixture {
    fixture_at("home-a")
}

fn fixture_at(home: &str) -> Fixture {
    fixture_over(home, flotilla_engine_store::registry().expect("registry"))
}

/// A fixture over the decided store's whole registry: every kind that can hold a convoy, so its
/// holds read.
fn decided_fixture() -> Fixture {
    fixture_over("home-a", flotilla_provisioning::stored::decided_registry().expect("registry"))
}

fn fixture_over(home: &str, registry: flotilla_schema::KindRegistry) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(Service::open(&dir.path().join("decided.sqlite"), home, registry).expect("store"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = Arc::new(Convoys::open(home, &service).expect("convoys"));
    let engine = ConvoyEngine::new(Arc::clone(&convoys), backend.clone(), Arc::new(SystemClock));
    Fixture { _dir: dir, backend, service, convoys, engine }
}

/// What settlement read when it judged a convoy landed: nothing yet, from the log as it is now.
fn landed(fixture: &Fixture) -> SettlementEvidence {
    SettlementEvidence::after(fixture.engine.position().expect("position"))
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
    for (vessel, role, operation) in promises::admitted_declarations(&[], &status, false) {
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

/// The convoy as readers see it: the stored record with the engine's promises
/// in place (#3587 layer 8).
async fn reread(fixture: &Fixture, name: &str) -> ResourceObject<Convoy> {
    let stored = fixture.backend.clone().using::<Convoy>(NAMESPACE).get(name).await.expect("convoy");
    let promises = fixture.engine.promises(&stored, None).expect("read").expect("the engine holds it");
    let mut viewed = stored;
    viewed.status.get_or_insert_with(Default::default).promises = promises;
    viewed
}

fn old_promise<'a>(convoy: &'a ResourceObject<Convoy>, vessel: &str, role: &str, id: &str) -> &'a promises::Promise {
    promises::owned(convoy.status.as_ref().expect("status"), vessel, role).iter().find(|promise| promise.id == id).expect("old promise")
}

fn submit(id: &str, reference: &str) -> PromiseOperation {
    PromiseOperation::Submit {
        id: id.into(),
        kind: PromiseKind::Pr,
        source: OldSource::Crew,
        submission: Submission { reference: reference.into(), metadata: BTreeMap::new(), submitted_at: Some(Utc::now()), verdict: None },
    }
}

fn merged(id: &str) -> PromiseOperation {
    PromiseOperation::Verdict {
        id: id.into(),
        reference: PR.into(),
        submitted_at: Some(Utc::now()),
        verdict: SubmissionVerdict { accepted: true, who: "forge".into(), at: Utc::now(), why: "PR merged".into() },
    }
}

/// Admits `convoy` as admission does: a launch of what it is a launch of is reserved, then
/// created as `convoy` describes it. Returns the launch as admitted.
async fn admit(fixture: &Fixture, convoy: &ResourceObject<Convoy>) -> Result<ResourceObject<Convoy>, String> {
    let mut launch = convoy.clone();
    launch.metadata.name = fixture.engine.reserve(&convoy_map::engine_name(convoy)?).await?;
    fixture.engine.admit_reserved(&launch).await?;
    Ok(launch)
}

async fn admitted(fixture: &Fixture, name: &str, status: ConvoyStatus) -> ResourceObject<Convoy> {
    let convoy = old_convoy(&fixture.backend, name, status).await;
    admit(fixture, &convoy).await.expect("admit");
    reread(fixture, name).await
}

// Behaviour: a freshly bootstrapped convoy is admitted on the engine with one seat per crew and
// its seeded promises under their old ids as labels; the engine's view reproduces the seeded old
// promises exactly, and a launch is admitted once.
#[tokio::test]
async fn a_fresh_convoy_is_admitted_with_its_seeded_promises() {
    let fixture = fixture();
    let status = bootstrapped(vec![("work", vec![crew("coder", true), crew("reviewer", false)])]);
    let seeded = status.promises.clone();
    let convoy = admitted(&fixture, "c", status).await;
    let engine = fixture.convoys.get("c").expect("held");
    let promises: Vec<_> = engine.spec().promises.iter().map(|row| (row.record.label.clone(), row.record.owner.clone())).collect();
    assert_eq!(promises, vec![("template/coder/pr".to_owned(), SeatName::new("coder", ""))]);
    assert_eq!(engine.spec().seats.len(), 2);
    assert_eq!(convoy.status.as_ref().expect("status").promises, seeded, "the engine's view reproduces the seeded promises");
    let launch = engine.name().to_owned();
    let again = ResourceObject { metadata: flotilla_resources::ObjectMeta { name: launch, ..convoy.metadata.clone() }, ..convoy };
    assert!(fixture.engine.admit_reserved(&again).await.is_err(), "its reservation was taken by the first");
}

// Behaviour: delivery is written only on the evidence settlement read. A turn admitted to a crew's
// inbox, or a checkout inspected, after settlement read them refuses it and leaves the convoy open;
// a message to another crew and another checkout's inspection are not its evidence.
#[tokio::test]
async fn delivery_is_refused_when_what_settlement_read_was_written_since() {
    let fixture = decided_fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    fixture.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("submitted");
    fixture.engine.promise(&convoy, "work", "coder", &merged("template/pr"), Actor::Observer).await.expect("kept");
    let convoy = reread(&fixture, "c").await;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let messages = EngineMessagesHome::over(Arc::clone(&fixture.service), NAMESPACE, &backend);
    let (crew, other) = ("fleet/c/work/coder", "fleet/other/work/coder");
    let inbox = messages.primitives().inbox_for(NAMESPACE, crew).await.expect("inbox").to_string();
    let judged =
        || SettlementEvidence { checkouts: ["checkout-c#1".to_owned()].into(), inboxes: [inbox.clone()].into(), ..landed(&fixture) };
    let turn = |record: &'static str, receiver: &'static str| {
        let primitives = messages.primitives().clone();
        async move {
            let intent = intent("fleet/operator", receiver, MessageRelation::Supervisor, "one more thing");
            primitives.admit(record, &intent).await.expect("admitted");
        }
    };

    let evidence = judged();
    turn("m-turn", crew).await;
    let refused = fixture.engine.deliver(&convoy, "merged", &evidence).await.expect("judged");
    assert!(matches!(&refused, Delivery::Moved(why) if why.contains(&inbox)), "{refused:?}");
    assert_eq!(fixture.convoys.get("c").expect("held").label(), ConvoyLabel::Open, "the convoy stays open");

    let evidence = judged();
    inspect(&fixture, "checkout-c#2");
    let refused = fixture.engine.deliver(&convoy, "merged", &evidence).await.expect("judged");
    assert!(matches!(&refused, Delivery::Moved(why) if why.contains("checkout-c#2")), "another launch is held in its place: {refused:?}");
    assert_eq!(fixture.convoys.get("c").expect("held").label(), ConvoyLabel::Open);

    let evidence = judged();
    turn("m-other", other).await;
    inspect(&fixture, "checkout-other#1");
    assert_eq!(fixture.engine.deliver(&convoy, "merged", &evidence).await.expect("judged"), Delivery::Delivered);
    assert_eq!(fixture.convoys.get("c").expect("held").label(), ConvoyLabel::Delivered);
}

/// Writes an inspection of checkout launch `launch`, as its home's inspector does.
fn inspect(fixture: &Fixture, launch: &str) {
    let inspection = CheckoutIntegration {
        failing_since: None,
        head_revision: Some("abc".into()),
        clean: Some(true),
        pushed: Some(true),
        landed: Some(true),
        clean_details: None,
        pushed_details: None,
        landed_details: None,
        change_request: None,
        landed_evidence: None,
        remote_digest: None,
    };
    fixture
        .service
        .with_store(|store| {
            let mut write = store.write()?;
            let change = write.open_change(&NewChange::new("checkout-inspector", Timestamp(2_000_000)))?;
            write.insert(launch, &inspection, change)?;
            write.commit()
        })
        .expect("inspected");
}

// Behaviour: the crew's submission and the forge's merge observation become the engine's Submit
// and observed Keep; the old status follows each, and delivery is refused until nothing is open.
#[tokio::test]
async fn a_submitted_pr_is_kept_by_its_merge_and_the_convoy_delivers() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    let refused = fixture.engine.deliver(&convoy, "merged", &landed(&fixture)).await.expect_err("an open promise holds delivery");
    assert!(refused.contains("PromisesOutstanding"), "{refused}");

    fixture.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("submitted");
    let convoy = reread(&fixture, "c").await;
    let copied = old_promise(&convoy, "work", "coder", "template/pr");
    assert_eq!(copied.state, PromiseState::Submitted);
    assert_eq!(copied.submissions.len(), 1);
    assert!(copied.submissions[0].names(&copied.submissions[0].subject_address(&[]).expect("cr"), &[]));
    // The same submission again from anyone but the crew writes nothing.
    fixture.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Observer).await.expect("again");
    let engine = fixture.convoys.get("c").expect("held");
    assert_eq!(engine.spec().promises.iter().next().expect("promise").record.submissions.len(), 1);
    // The crew's is its answer to a turn, recorded as a submission again (GitHub run GH2-1).
    fixture.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("again");
    let engine = fixture.convoys.get("c").expect("held");
    assert_eq!(engine.spec().promises.iter().next().expect("promise").record.submissions.len(), 2);
    // One with no commit keeps the revision the current submission has.
    let mut pushed = submit("template/pr", PR);
    if let PromiseOperation::Submit { submission, .. } = &mut pushed {
        submission.metadata.insert("commit".into(), "c1".into());
    }
    fixture.engine.promise(&convoy, "work", "coder", &pushed, Actor::Crew).await.expect("a new commit");
    fixture.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("again");
    let engine = fixture.convoys.get("c").expect("held");
    let revisions: Vec<_> =
        engine.spec().promises.iter().next().expect("promise").record.submissions.iter().map(|row| row.record.revision.clone()).collect();
    assert_eq!(revisions, ["", "", "c1", "c1"]);

    fixture.engine.promise(&convoy, "work", "coder", &merged("template/pr"), Actor::Observer).await.expect("kept");
    let convoy = reread(&fixture, "c").await;
    assert_eq!(old_promise(&convoy, "work", "coder", "template/pr").state, PromiseState::Kept);
    fixture.engine.deliver(&convoy, "merged", &landed(&fixture)).await.expect("delivered");
    assert_eq!(fixture.convoys.get("c").expect("held").label(), ConvoyLabel::Delivered);
    // The delivery records the exit entry it was given; delivering again changes nothing.
    fixture.engine.deliver(&convoy, "closed-unmerged", &landed(&fixture)).await.expect("delivering again is no error");
    assert_eq!(fixture.engine.delivered(&convoy).as_deref(), Some("merged"));
}

// Behaviour (run 3 F7): a keyed read of one convoy carries what its log records of a promise:
// when its submission was written, the closed PR's rejection (whose edge records no reason) and
// the operator's own retraction reason. The record-only read that listings take leaves each of
// these absent rather than inventing them.
#[tokio::test]
async fn a_keyed_read_carries_the_logged_submission_rejection_and_retraction() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    let before = Utc::now() - chrono::Duration::milliseconds(1);
    fixture.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("submitted");
    let after = Utc::now() + chrono::Duration::milliseconds(1);
    let closed = PromiseOperation::Verdict {
        id: "template/pr".into(),
        reference: PR.into(),
        submitted_at: None,
        verdict: SubmissionVerdict { accepted: false, who: "forge".into(), at: Utc::now(), why: "PR closed without merging".into() },
    };
    fixture.engine.promise(&convoy, "work", "coder", &closed, Actor::Observer).await.expect("rejected");
    fixture.engine.verdict(&convoy, "template/coder/pr", false, "alice", "closed unmerged; retraction accepted").await.expect("retracted");
    let stored = fixture.backend.clone().using::<Convoy>(NAMESPACE).get("c").await.expect("convoy");

    let recorded = fixture.engine.recorded_promises(&stored, None).expect("read").expect("held");
    let promise = recorded["work"]["coder"].iter().find(|promise| promise.id == "template/pr").expect("promise");
    assert_eq!(promise.state, PromiseState::Retracted);
    assert_eq!(promise.retraction_reason.as_deref(), Some("closed unmerged; retraction accepted"));
    let [submission] = promise.submissions.as_slice() else { panic!("one submission: {:?}", promise.submissions) };
    let at = submission.submitted_at.expect("the Submit entry's time");
    assert!(before <= at && at <= after, "{before} <= {at} <= {after}");
    let verdict = submission.verdict.as_ref().expect("the rejection");
    assert!(!verdict.accepted && verdict.why == "rejected" && verdict.at >= at, "{verdict:?}");

    let listed = fixture.engine.promises(&stored, None).expect("read").expect("held");
    let promise = listed["work"]["coder"].iter().find(|promise| promise.id == "template/pr").expect("promise");
    assert_eq!(promise.retraction_reason, None, "the record holds no reason");
    assert_eq!((promise.submissions[0].submitted_at, &promise.submissions[0].verdict), (None, &None), "nor a time or a rejection");
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
    assert_eq!(old_promise(&reread(&fixture, "c").await, "work", "coder", "template/pr").state, PromiseState::Open);
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
    let convoy = reread(&fixture, "c").await;
    assert_eq!(old_promise(&convoy, "work", "coder", "l2-quint").state, PromiseState::Retracted);
    assert_eq!(old_promise(&convoy, "work", "coder", "extra").state, PromiseState::Submitted);
    // A retracted id is never reused: declaring it again is a no-op on the same promise.
    fixture.engine.promise(&convoy, "work", "coder", &declare, Actor::Crew).await.expect("existing");
    assert_eq!(fixture.convoys.get("c").expect("held").spec().promises.len(), 3);
}

// Behaviour: a role repeated across vessels gets one seat per vessel, and each seat's template
// promise is named by that seat; the old status keeps the ids crews submit against, and each
// crew's submission reaches its own promise.
#[tokio::test]
async fn repeated_roles_get_seats_and_template_labels_by_seat() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("a", vec![crew("coder", true)]), ("b", vec![crew("coder", true)])])).await;
    let engine = fixture.convoys.get("c").expect("held");
    let labels: Vec<_> = engine.spec().promises.iter().map(|row| row.record.label.clone()).collect();
    assert_eq!(labels, vec!["template/coder~a/pr".to_owned(), "template/coder~b/pr".to_owned()]);
    let second = &engine.spec().promises.iter().nth(1).expect("b's promise").record;
    assert_eq!(convoy_map::old_id(second), "template/pr");
    fixture.engine.promise(&convoy, "b", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("submitted");
    let convoy = reread(&fixture, "c").await;
    assert_eq!(old_promise(&convoy, "a", "coder", "template/pr").state, PromiseState::Open);
    assert_eq!(old_promise(&convoy, "b", "coder", "template/pr").state, PromiseState::Submitted);
}

// Behaviour: a crew keeps its own promises through a handoff (#3574 decision 3) except those the
// handoff names: the coder's open deliverable stays the coder's when it hands work to the reviewer,
// a follow-up the reviewer declares and names moves to the coder, and a name that is not the
// sender's, or not open, refuses the handoff by that name and moves nothing. A handoff to a crew
// the convoy has no seat for adds that seat under the sender without moving anything.
#[tokio::test]
async fn handoff_moves_only_the_promises_it_names() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true), crew("reviewer", false)])])).await;
    let (coder, reviewer) = (("work", "coder"), ("work", "reviewer"));
    fixture.engine.reassign(&convoy, coder, reviewer, &[]).await.expect("handed off");
    let convoy = reread(&fixture, "c").await;
    assert_eq!(old_promise(&convoy, "work", "coder", "template/pr").state, PromiseState::Open);

    let declare = PromiseOperation::Declare { id: "follow-up".into(), kind: PromiseKind::Pr, source: OldSource::Crew };
    fixture.engine.promise(&convoy, "work", "reviewer", &declare, Actor::Crew).await.expect("declared");
    let refused = fixture.engine.reassign(&convoy, coder, reviewer, &["template/pr".into(), "follow-up".into()]).await;
    assert!(refused.expect_err("not the coder's").contains("`follow-up`"));
    let declare = PromiseOperation::Declare { id: "dropped".into(), kind: PromiseKind::Pr, source: OldSource::Crew };
    fixture.engine.promise(&convoy, "work", "reviewer", &declare, Actor::Crew).await.expect("declared");
    let retract = PromiseOperation::Retract { id: "dropped".into(), reason: "not needed".into() };
    fixture.engine.promise(&convoy, "work", "reviewer", &retract, Actor::Crew).await.expect("retracted");
    let refused = fixture.engine.reassign(&convoy, reviewer, coder, &["follow-up".into(), "dropped".into()]).await;
    assert!(refused.expect_err("not open").contains("`dropped`"));
    let convoy = reread(&fixture, "c").await;
    assert_eq!(old_promise(&convoy, "work", "coder", "template/pr").state, PromiseState::Open, "a refusal moves nothing");
    assert_eq!(old_promise(&convoy, "work", "reviewer", "follow-up").state, PromiseState::Open, "a refusal moves nothing");

    fixture.engine.reassign(&convoy, reviewer, coder, &["follow-up".into()]).await.expect("reassigned");
    let convoy = reread(&fixture, "c").await;
    assert_eq!(old_promise(&convoy, "work", "coder", "follow-up").state, PromiseState::Open);
    assert!(promises::owned(convoy.status.as_ref().expect("status"), "work", "reviewer").iter().all(|p| p.id != "follow-up"));

    fixture.engine.reassign(&convoy, coder, ("work", "fixer"), &[]).await.expect("handed off to a new seat");
    let engine = fixture.convoys.get("c").expect("held");
    let (_, fixer) = engine.spec().seat(&SeatName::new("fixer", "work")).expect("fan-out seat");
    assert_eq!(fixer.parent, Some(SeatName::new("coder", "")));
    let promise = &engine.spec().promises.iter().next().expect("promise").record;
    assert_eq!((promise.owner.clone(), promise.label.as_str()), (SeatName::new("coder", ""), "template/coder/pr"));
}

// Behaviour: an operator keeps a crew's open promises by verdict (the old `--force`), or keeps or
// retracts one promise by its label; each verdict needs a reason.
#[tokio::test]
async fn operator_verdicts_keep_and_retract() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("a", vec![crew("coder", true)]), ("b", vec![crew("coder", true)])])).await;
    let scope = VerdictScope { vessel: "a".into(), role: Some("coder".into()) };
    let kept = fixture.engine.keep_by_verdict(&convoy, &scope, "alice", "merged by hand").await.expect("kept");
    assert_eq!(kept, vec!["template/coder~a/pr".to_owned()]);
    let refused = fixture.engine.verdict(&convoy, "template/coder~b/pr", false, "alice", "").await.expect_err("no reason");
    assert!(refused.contains("Unauthorised"), "{refused}");
    fixture.engine.verdict(&convoy, "template/coder~b/pr", false, "alice", "dropped").await.expect("retracted");
    let convoy = reread(&fixture, "c").await;
    assert_eq!(old_promise(&convoy, "a", "coder", "template/pr").state, PromiseState::Kept);
    assert_eq!(old_promise(&convoy, "b", "coder", "template/pr").state, PromiseState::Retracted);
    fixture.engine.deliver(&convoy, "claim", &landed(&fixture)).await.expect("nothing open");
}

// Behaviour: a convoy with no obligation is refused by name, and nothing is admitted. (A convoy
// with history is the import's: admission creates only fresh launches.)
#[tokio::test]
async fn admission_refuses_no_obligations() {
    let fixture = fixture();
    let convoy = old_convoy(&fixture.backend, "idle", bootstrapped(vec![("work", vec![crew("reviewer", false)])])).await;
    let refused = admit(&fixture, &convoy).await.expect_err("refused");
    assert!(refused.contains("NoObligations"), "{refused}");
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

// Behaviour: an operator's delete abandons the convoy and, with nothing holding it, retires it in
// the same call, with no retirement follower running: a listing read straight after the delete
// leaves it out, and a read by name still says how it ended. Deleting it again is no error.
#[tokio::test]
async fn a_delete_retires_a_convoy_nothing_holds() {
    let fixture = decided_fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    let records = EngineConvoyRecords::new(
        "home-a",
        Arc::clone(&fixture.service),
        Arc::default(),
        OpenAwaiting::open(&fixture.service).expect("open awaiting"),
    );
    assert_eq!(records.at("home-a").expect("listing").records.len(), 1);
    let launch = fixture.convoys.get("c").expect("held").name().to_owned();

    fixture.engine.delete(&convoy, "alice").await.expect("deleted");
    assert_eq!(fixture.convoys.get("c").expect("held").label(), ConvoyLabel::Retired);
    assert_eq!(records.at("home-a").expect("listing").records.len(), 0, "a deleted convoy is not listed");
    let ended = records.get(&launch, None).expect("read").expect("a retired convoy reads by name");
    assert_eq!(
        ended.ended.and_then(|ending| ending.held().map(|(end, _)| end.clone())),
        Some(ConvoyEnd::Abandoned { reason: "deleted".into() })
    );
    fixture.engine.delete(&convoy, "alice").await.expect("deleting a retired convoy again");
}

// Behaviour (#3108 C3'): a supervisor's or operator's Fail ruling is the engine's FailSeat on the
// crew's seat, with its reason; ruling it again leaves the seat as it failed.
#[tokio::test]
async fn a_fail_ruling_fails_the_seat_once() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    fixture.engine.fail_seat(&convoy, "work", "coder", "alice", "cannot reach the forge").await.expect("failed");
    fixture.engine.fail_seat(&convoy, "work", "coder", "bob", "again").await.expect("already failed");
    let engine = fixture.convoys.get("c").expect("held");
    let (_, seat) = engine.spec().seat(&SeatName::new("coder", "")).expect("seat");
    assert_eq!(seat.failed.as_deref(), Some("cannot reach the forge"));
}

// Behaviour (#3108 C3'): linking and unlinking a subject are the engine's Link and Unlink, each
// once: a subject linked now is left linked, and one not linked now is left unlinked.
#[tokio::test]
async fn subjects_link_and_unlink_on_the_engine() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    let subject = flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "org/repo".into() },
        id: "5".into(),
    };
    let linked = |fixture: &Fixture| fixture.convoys.get("c").expect("held").spec().linked().len();
    fixture.engine.link(&convoy, &subject, flotilla_resources::SubjectDiscoverySource::Operator, "alice").await.expect("linked");
    fixture.engine.link(&convoy, &subject, flotilla_resources::SubjectDiscoverySource::Relay, "relay").await.expect("linked now");
    assert_eq!(linked(&fixture), 1);
    fixture.engine.unlink(&convoy, &subject, "alice").await.expect("unlinked");
    fixture.engine.unlink(&convoy, &subject, "alice").await.expect("not linked now");
    assert_eq!(linked(&fixture), 0);
    // A change request found on the convoy's branch is linked as the branch found it.
    fixture
        .engine
        .link(&convoy, &subject, flotilla_resources::SubjectDiscoverySource::Branch, "convoy-controller")
        .await
        .expect("branch link");
    assert_eq!(linked(&fixture), 1);
}

// Behaviour (#3721 look-over ruling): a label names one deliverable in its convoy. A second
// crew declaring a label already taken is refused with the owner named, never qualified; under its
// own label it is declared, and the first crew's kept promise is untouched.
#[tokio::test]
async fn a_taken_label_is_refused_at_declaration() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true), crew("reviewer", false)])])).await;
    let ledger = |id: &str, by: &str| PromiseOperation::Submit {
        id: id.into(),
        kind: PromiseKind::DecisionLedger,
        source: OldSource::Crew,
        submission: Submission {
            reference: format!("{by}-ledger"),
            metadata: BTreeMap::new(),
            submitted_at: Some(Utc::now()),
            verdict: None,
        },
    };
    fixture.engine.promise(&convoy, "work", "coder", &ledger("script-ledger", "coder"), Actor::Crew).await.expect("coder's ledger");
    let kept = PromiseOperation::Verdict {
        id: "script-ledger".into(),
        reference: String::new(),
        submitted_at: Some(Utc::now()),
        verdict: SubmissionVerdict { accepted: true, who: "artifact".into(), at: Utc::now(), why: "exists".into() },
    };
    fixture.engine.promise(&convoy, "work", "coder", &kept, Actor::Observer).await.expect("kept");
    let refused =
        fixture.engine.promise(&convoy, "work", "reviewer", &ledger("script-ledger", "reviewer"), Actor::Crew).await.expect_err("taken");
    assert!(refused.contains("`script-ledger` is already declared") && refused.contains("owned by `coder`"), "{refused}");
    let own = ledger("script-ledger-reviewer", "reviewer");
    fixture.engine.promise(&convoy, "work", "reviewer", &own, Actor::Crew).await.expect("reviewer's ledger");
    let convoy = reread(&fixture, "c").await;
    assert_eq!(old_promise(&convoy, "work", "coder", "script-ledger").state, PromiseState::Kept);
    assert_eq!(old_promise(&convoy, "work", "reviewer", "script-ledger-reviewer").state, PromiseState::Submitted);
    // The same reference again is the submission already made.
    fixture.engine.promise(&convoy, "work", "coder", &ledger("script-ledger", "coder"), Actor::Crew).await.expect("kept: a no-op");
}

// Behaviour (#3721 look-over ruling): an admission whose promises share a label is refused with
// a reason naming the label and its seats, and nothing is admitted.
#[tokio::test]
async fn an_admission_with_a_label_clash_is_refused() {
    let fixture = fixture();
    let mut status = bootstrapped(vec![("work", vec![crew("coder", true), crew("reviewer", false)])]);
    let api = PromiseOperation::Declare { id: "api-split".into(), kind: PromiseKind::Pr, source: OldSource::Dispatch };
    promises::apply(&mut status, "work", "coder", &api);
    promises::apply(&mut status, "work", "reviewer", &api);
    let convoy = old_convoy(&fixture.backend, "c", status).await;
    let reason = admit(&fixture, &convoy).await.expect_err("a clash is refused");
    assert!(reason.contains("`api-split`") && reason.contains("coder, reviewer"), "{reason}");
    assert!(!fixture.engine.holds(&convoy));
}

// Behaviour (#3587 layer 8): the engine decides and readers read it; no engine edge writes the
// old status, so the stored promises stay as admission found them.
#[tokio::test]
async fn engine_edges_write_no_old_status() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    let before = fixture.backend.clone().using::<Convoy>(NAMESPACE).get("c").await.expect("stored");
    let mut changes = fixture.engine.changes();
    fixture.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("submitted");
    let after = fixture.backend.clone().using::<Convoy>(NAMESPACE).get("c").await.expect("stored");
    assert_eq!(before.metadata.resource_version, after.metadata.resource_version, "the old record is untouched");
    assert_eq!(old_promise(&reread(&fixture, "c").await, "work", "coder", "template/pr").state, PromiseState::Submitted);
    assert_eq!(changes.try_recv().expect("a change"), "c", "followers hear of the engine edge");
}

// Behaviour (#3587 layer 8): a reader at another home sees a replicated convoy's promises from
// its home's engine record in this home's decided store, by one keyed read at that home.
#[tokio::test]
async fn a_replica_is_read_at_its_home() {
    use flotilla_schema::Timestamp;
    use flotilla_schema_sqlite::stream::Follower;
    let home_b = fixture_at("home-b");
    let convoy = admitted(&home_b, "c", bootstrapped(vec![("work", vec![crew("coder", true)])])).await;
    home_b.engine.promise(&convoy, "work", "coder", &submit("template/pr", PR), Actor::Crew).await.expect("submitted");
    let home_a = fixture_at("home-a");
    let mut frames = Vec::new();
    home_b
        .service
        .serve(None, Timestamp(1), |frame| {
            frames.push(frame.to_vec());
            Ok(())
        })
        .expect("serve");
    home_a
        .service
        .with_store(|store| {
            Follower::new(store).receive("home-b", |sink| {
                for frame in &frames {
                    sink(frame)?;
                }
                Ok(())
            })
        })
        .expect("follow");
    assert!(home_a.engine.promises(&convoy, None).expect("read").is_none(), "not this home's");
    let seen = home_a.engine.promises(&convoy, Some("home-b")).expect("read").expect("the replica");
    assert_eq!(seen["work"]["coder"][0].state, PromiseState::Submitted);
    assert!(!home_a.engine.holds(&convoy), "a replica is read, never decided, here");
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
    assert_eq!(old_promise(&reread(&fixture, "c").await, "work", "coder", "template/pr").state, PromiseState::Retracted);
}

// Behaviour (#3721 review): a verdict names one promise. An exact label always does; a bare old
// id shared by several promises is refused with the labels to choose from, never guessed.
#[tokio::test]
async fn an_ambiguous_verdict_is_refused_with_the_labels() {
    let fixture = fixture();
    let convoy = admitted(&fixture, "c", bootstrapped(vec![("a", vec![crew("coder", true)]), ("b", vec![crew("coder", true)])])).await;
    let refused = fixture.engine.verdict(&convoy, "template/pr", true, "alice", "merged").await.expect_err("ambiguous");
    assert!(refused.contains("`template/coder~a/pr`") && refused.contains("`template/coder~b/pr`"), "{refused}");
    fixture.engine.verdict(&convoy, "template/coder~b/pr", true, "alice", "merged").await.expect("exact");
    let convoy = reread(&fixture, "c").await;
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
    use flotilla_engine_convoys::convoys::EditError;
    use flotilla_state_machines::{
        machines::convoy::{ConvoyRefusal, Declare, PromiseId, Submit},
        Ref, Source,
    };
    let fixture = fixture();
    let request =
        convoy_map::request(&tokio_block(old_convoy(&fixture.backend, "c", bootstrapped(vec![("work", vec![crew("coder", true)])]))))
            .expect("request");
    fixture.convoys.world(|world| world.projects.put(&Ref::new(request.project.as_str()), ()));
    let launch = fixture.convoys.reserve("c", flotilla_schema::Timestamp(1)).expect("reserved");
    fixture.convoys.create_reserved(&launch, request, flotilla_schema::Timestamp(1)).expect("admitted");
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

// Behaviour: a standing generation is a launch of its role's address, with the role's duty; an
// edge keeps its duties; the next generation is a launch of its own, and ending one leaves the
// other as it was.
#[tokio::test]
async fn standing_admission_uses_the_ensure_role_identity() {
    use flotilla_resources::{Selector, ENSURED_FROM_ANNOTATION};
    use flotilla_schema::Timestamp;
    use flotilla_state_machines::Ref;
    let fixture = fixture();
    let governor = CrewSpec::builder()
        .role("governor".to_string())
        .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
        .build();
    let mut old = old_convoy(&fixture.backend, "old-generation-id", bootstrapped(vec![("work", vec![governor])])).await;
    old.spec.role = "governor".into();
    old.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "ensure-governor".into());
    let name = "project:fleet/convoy:governor";
    assert_eq!(convoy_map::engine_name(&old).unwrap(), name);
    assert_eq!(convoy_map::address(&old).unwrap().as_str(), name);
    let first = admit(&fixture, &old).await.expect("admitted");
    assert!(fixture.convoys.get("old-generation-id").is_none());
    assert_eq!(fixture.convoys.get(name).unwrap().spec().duties.len(), 1);
    let authority = Ref::new("ensure-governor");
    fixture.convoys.world(|world| world.authority.put(&authority, true));
    let duties = fixture.convoys.get(name).unwrap().spec().duties.clone();
    // Sorts before governor: rebuilding positional duty IDs would renumber them.
    fixture
        .convoys
        .apply(
            &first.metadata.name,
            flotilla_state_machines::machines::convoy::AddSeat {
                seat: SeatName::new("aaa", ""),
                parent: SeatName::new("governor", ""),
                supervised_by: convoy_map::address(&old).unwrap(),
                by: SeatName::new("governor", ""),
                authority: authority.clone(),
            },
            Timestamp(8),
        )
        .unwrap();
    assert_eq!(fixture.convoys.get(name).unwrap().spec().duties, duties);
    assert!(fixture.engine.holds(&first));
    let held = fixture.convoys.get(name).unwrap();
    assert_eq!(held.name(), first.metadata.name);
    assert_eq!(held.name(), format!("{name}#{}", held.spec().launch), "a launch of the role's name");
    assert!(fixture.engine.promises(&first, None).unwrap().is_some());

    fixture.engine.abandon(&first, "operator", "replace generation").await.expect("end engine generation");
    let second = admit(&fixture, &old).await.expect("next generation");
    assert_ne!(second.metadata.name, first.metadata.name, "the next generation is a launch of its own");
    assert!(fixture.engine.holds(&second));
    let ended = fixture.convoys.launch(&first.metadata.name).expect("first generation");
    fixture.engine.abandon(&second, "operator", "end newer generation").await.expect("end newer engine generation");
    assert_eq!(fixture.convoys.launch(&first.metadata.name).expect("first generation").version(), ended.version());
    assert_ne!(fixture.convoys.get(name).expect("newest").label(), ConvoyLabel::Open);
}

#[tokio::test]
async fn invalid_standing_identity_refuses_admission_and_reads_without_legacy_fallback() {
    let fixture = fixture();
    let mut old = old_convoy(&fixture.backend, "invalid-standing", bootstrapped(vec![])).await;
    old.metadata.annotations.insert(flotilla_resources::ENSURED_FROM_ANNOTATION.into(), "ensure".into());
    old.spec.role.clear();
    assert!(fixture.engine.holds(&old), "invalid identity must stay on the error-returning engine path");
    assert!(admit(&fixture, &old).await.is_err());
    assert!(fixture.engine.promises(&old, None).is_err());
    assert!(fixture.convoys.get("invalid-standing").is_none());
    let path: flotilla_resources::convoy::engine::EngineConvoyPath = Arc::new(fixture.engine);
    let read = flotilla_resources::ReadResourceObject { object: old, provenance: flotilla_resources::ResourceProvenance::Local };
    let Err(error) = flotilla_resources::convoy::engine::viewed_read(&path, &read) else { panic!("public read must return the error") };
    assert!(error.contains("no recorded ensure role"), "{error}");
}
