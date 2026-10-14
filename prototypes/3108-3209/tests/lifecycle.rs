//! The shared lifecycle end to end: gates, recorded inputs, ends, attempts, judgements.

use std::time::Duration;

use state_machine_prototype::attempt::{retry, Cause, CauseCategory, Outcome, Schedule};
use state_machine_prototype::base::{ObjRef, Time};
use state_machine_prototype::home::{Home, Refusal};
use state_machine_prototype::kinds::lifecycle::*;
use state_machine_prototype::record::{child_tables, ddl};
use state_machine_prototype::view::{Fact, InputRead, MemoryView};

type CloneHome = Home<CloneMachine>;

fn req() -> CloneRequest {
    CloneRequest { url: "https://example.com/r.git".into(), path: "/w/r".into() }
}

fn transient(detail: &str) -> Outcome {
    Outcome::Failure { cause: Cause::new(CauseCategory::Transient, detail), retryable: true }
}

fn obs() -> ObjRef {
    ObjRef::new("observation", "clone-1")
}

fn realise(identity: &str) -> LifecycleEdge {
    LifecycleEdge::Realise { identity: identity.into(), observed: obs() }
}

fn setup() -> (CloneHome, ObjRef, MemoryView) {
    let mut home = CloneHome::new("kiwi").unwrap();
    let id = home.create("clone-1", req()).unwrap();
    let mut view = MemoryView::new();
    view.set_now(Time(1_000));
    (home, id, view)
}

fn gate_reason(r: Result<impl Sized, Refusal>) -> String {
    match r {
        Err(Refusal::Gate { why, .. }) => why.0,
        Err(other) => panic!("expected gate refusal, got {other}"),
        Ok(_) => panic!("expected gate refusal, edge was applied"),
    }
}

#[test]
fn realise_is_gated_on_an_observation_with_matching_identity() {
    let (mut home, id, mut view) = setup();
    assert!(gate_reason(home.apply(&id, realise("/w/r"), &view)).contains("no observation"));
    view.observe(&obs(), "feta", Fact::Present { identity: "/w/other".into() });
    assert!(gate_reason(home.apply(&id, realise("/w/r"), &view)).contains("expected the thing present"));
    assert_eq!(home.get(&id).unwrap().state(), LifecycleState::Requested, "refusals leave no trace");
    assert!(home.get(&id).unwrap().events.is_empty());

    view.observe(&obs(), "feta", Fact::Present { identity: "/w/r".into() });
    let ev = home.apply(&id, realise("/w/r"), &view).unwrap();
    assert_eq!((ev.from, ev.to), (LifecycleState::Requested, LifecycleState::Realised));
    assert_eq!(ev.writer, "kiwi");
}

#[test]
fn the_event_records_the_references_and_versions_the_gate_read() {
    let (mut home, id, mut view) = setup();
    view.observe(&obs(), "feta", Fact::Present { identity: "a".into() });
    view.observe(&obs(), "feta", Fact::Present { identity: "b".into() }); // value changed: version 2
    let ev = home.apply(&id, realise("b"), &view).unwrap();
    assert_eq!(ev.inputs, vec![InputRead { subject: obs(), version: 2 }]);
    assert_eq!(identity(home.get(&id).unwrap()), Some("b"));
}

#[test]
fn unchanged_observations_do_not_bump_versions() {
    let mut view = MemoryView::new();
    assert_eq!(view.observe(&obs(), "feta", Fact::Absent), 1);
    assert_eq!(view.observe(&obs(), "feta", Fact::Absent), 1);
    assert_eq!(view.observe(&obs(), "feta", Fact::Present { identity: "x".into() }), 2);
}

#[test]
fn give_up_is_refused_while_no_attempt_has_failed() {
    let (mut home, id, view) = setup();
    let cause = Cause::new(CauseCategory::Transient, "x");
    let r = home.apply(&id, LifecycleEdge::GiveUp { cause }, &view);
    assert!(gate_reason(r).contains("not exhausted"));
}

#[test]
fn edges_are_checked_against_their_from_states() {
    let (mut home, id, mut view) = setup();
    view.observe(&obs(), "feta", Fact::Absent);
    match home.apply(&id, LifecycleEdge::Vanish { observed: obs() }, &view) {
        Err(Refusal::WrongState { edge: "vanish", state: "Requested", allowed }) => assert_eq!(allowed, vec!["Realised"]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn vanish_and_release_both_reach_gone_with_different_gates() {
    for via_release in [false, true] {
        let (mut home, id, mut view) = setup();
        view.observe(&obs(), "feta", Fact::Present { identity: "i".into() });
        home.apply(&id, realise("i"), &view).unwrap();
        let edge = if via_release {
            let t = ObjRef::new("teardown", "clone-1");
            assert!(gate_reason(home.apply(&id, LifecycleEdge::Release { by: "gov".into(), teardown: t.clone() }, &view))
                .contains("no observation"));
            view.observe(&t, "feta", Fact::TeardownComplete);
            LifecycleEdge::Release { by: "gov".into(), teardown: t }
        } else {
            assert!(gate_reason(home.apply(&id, LifecycleEdge::Vanish { observed: obs() }, &view)).contains("expected the thing absent"));
            view.observe(&obs(), "feta", Fact::Absent);
            LifecycleEdge::Vanish { observed: obs() }
        };
        home.apply(&id, edge, &view).unwrap();
        assert_eq!(home.get(&id).unwrap().state(), LifecycleState::Gone);
    }
}

#[test]
fn end_states_refuse_every_edge_and_record_with_a_visible_reason() {
    let (mut home, id, mut view) = setup();
    view.observe(&obs(), "feta", Fact::Present { identity: "i".into() });
    home.apply(&id, realise("i"), &view).unwrap();
    view.observe(&obs(), "feta", Fact::Absent);
    home.apply(&id, LifecycleEdge::Vanish { observed: obs() }, &view).unwrap();

    let t = ObjRef::new("teardown", "t");
    view.observe(&t, "feta", Fact::TeardownComplete);
    let every_edge = [
        realise("i"),
        LifecycleEdge::Vanish { observed: obs() },
        LifecycleEdge::Release { by: "x".into(), teardown: t },
        LifecycleEdge::GiveUp { cause: Cause::new(CauseCategory::External, "x") },
    ];
    for edge in every_edge {
        let name = state_machine_prototype::machine::Named::name(&edge);
        match home.apply(&id, edge, &view) {
            Err(Refusal::End { state: "Gone", attempted, reason }) => {
                assert_eq!(attempted, name);
                assert!(reason.contains("replaces it"), "{reason}");
            }
            other => panic!("{name}: {other:?}"),
        }
    }
    assert!(matches!(home.record_attempt(&id, "clone", "x", Outcome::Success, &view), Err(Refusal::End { .. })));
    let shown = home.apply(&id, realise("i"), &view).unwrap_err().to_string();
    assert!(shown.contains("end state") && shown.contains("Gone"), "{shown}");
    assert_eq!(home.get(&id).unwrap().events.len(), 2, "late writes append nothing");
}

#[test]
fn gave_up_is_an_end_and_blocks_realise() {
    let (mut home, id, mut view) = setup();
    let cause = Cause::new(CauseCategory::Refused, "auth");
    home.record_attempt(&id, "clone", "origin", Outcome::Failure { cause: cause.clone(), retryable: false }, &view).unwrap();
    home.apply(&id, LifecycleEdge::GiveUp { cause }, &view).unwrap();
    view.observe(&obs(), "feta", Fact::Present { identity: "i".into() });
    assert!(matches!(home.apply(&id, realise("i"), &view), Err(Refusal::End { state: "GaveUp", .. })));
}

#[test]
fn give_up_requires_an_exhausted_policy_and_the_recorded_cause() {
    let (mut home, id, mut view) = setup();
    let give_up = || LifecycleEdge::GiveUp { cause: Cause::new(CauseCategory::Transient, "timeout") };
    for n in 1..=4 {
        assert!(gate_reason(home.apply(&id, give_up(), &view)).contains(&format!("{} of 4", n - 1)));
        home.record_attempt(&id, "clone", "origin", transient("timeout"), &view).unwrap();
        view.advance(Duration::from_secs(400));
    }
    let wrong = LifecycleEdge::GiveUp { cause: Cause::new(CauseCategory::External, "other") };
    assert!(gate_reason(home.apply(&id, wrong, &view)).contains("last recorded failure"));
    let ev = home.apply(&id, give_up(), &view).unwrap();
    assert_eq!(ev.inputs.len(), 1);
    assert_eq!(ev.inputs[0].version, 4, "gate relied on a four-row attempt history");
}

#[test]
fn a_non_retryable_failure_exhausts_immediately() {
    let (mut home, id, view) = setup();
    let cause = Cause::new(CauseCategory::MissingCapability, "no git");
    home.record_attempt(&id, "clone", "origin", Outcome::Failure { cause: cause.clone(), retryable: false }, &view).unwrap();
    home.apply(&id, LifecycleEdge::GiveUp { cause }, &view).unwrap();
}

#[test]
fn attempts_are_records_identified_by_owner_and_position() {
    let (mut home, id, view) = setup();
    let k0 = home.record_attempt(&id, "clone", "a", transient("1"), &view).unwrap();
    let k1 = home.record_attempt(&id, "clone", "b", Outcome::Success, &view).unwrap();
    assert_eq!((k0.owner.clone(), k0.position), (id.clone(), 0));
    assert_eq!((k1.owner, k1.position), (id.clone(), 1));
    let r = home.get(&id).unwrap();
    assert_eq!(r.attempts.get(1).unwrap().target, "b");
    let keys: Vec<u32> = r.attempts.iter().map(|(k, _)| k.position).collect();
    assert_eq!(keys, vec![0, 1]);
}

#[test]
fn retry_schedule_is_computed_from_history_and_policy() {
    let (mut home, id, mut view) = setup();
    let policy = <CloneKind as LifecycleSpec>::policy();
    // No attempts: try now.
    assert_eq!(retry(home.get(&id).unwrap().attempts.rows(), &policy).schedule, Schedule::Now);
    // Failures at t=1000, then t=11_000: backoff 10s then 30s.
    home.record_attempt(&id, "clone", "o", transient("a"), &view).unwrap();
    let j = retry(home.get(&id).unwrap().attempts.rows(), &policy);
    assert_eq!((j.failed_attempts, j.schedule), (1, Schedule::At(Time(11_000))));
    assert!(!j.due(Time(10_999)) && j.due(Time(11_000)));
    view.set_now(Time(11_000));
    home.record_attempt(&id, "clone", "o", transient("b"), &view).unwrap();
    let j = retry(home.get(&id).unwrap().attempts.rows(), &policy);
    assert_eq!((j.failed_attempts, j.schedule), (2, Schedule::At(Time(41_000))));
    // A success resets the count: the history is kept, the judgement moves on.
    view.set_now(Time(50_000));
    home.record_attempt(&id, "clone", "o", Outcome::Success, &view).unwrap();
    let j = retry(home.get(&id).unwrap().attempts.rows(), &policy);
    assert_eq!((j.failed_attempts, j.schedule), (0, Schedule::Now));
    assert_eq!(home.get(&id).unwrap().attempts.len(), 3);
}

#[test]
fn backoff_is_capped() {
    let p = <CloneKind as LifecycleSpec>::policy();
    assert_eq!(p.backoff(1), Duration::from_secs(10));
    assert_eq!(p.backoff(2), Duration::from_secs(30));
    assert_eq!(p.backoff(5), Duration::from_secs(300));
    assert_eq!(p.backoff(50), Duration::from_secs(300));
}

#[test]
fn ready_and_staleness_come_from_the_observer_link_never_from_stored_fields() {
    let (mut home, id, mut view) = setup();
    assert_eq!(ready(home.get(&id).unwrap(), &view), Ready::No("not realised yet"));
    view.observe(&obs(), "feta", Fact::Present { identity: "i".into() });
    home.apply(&id, realise("i"), &view).unwrap();
    let r = home.get(&id).unwrap();
    let (version, events) = (r.version(), r.events.len());

    // Never heard from feta: unknown, so not ready.
    assert_eq!(ready(r, &view), Ready::No("observer never heard from"));
    view.set_link_age("feta", Duration::from_secs(5));
    assert_eq!(ready(r, &view), Ready::Yes);
    assert_eq!(badges(r, &view), vec!["ready"]);
    view.set_link_age("feta", Duration::from_secs(360));
    assert_eq!(ready(r, &view), Ready::No("observer unreachable"));
    assert_eq!(badges(r, &view), vec!["observer unreachable 360s"]);
    view.set_link_age("feta", Duration::from_secs(1));
    assert_eq!(ready(r, &view), Ready::Yes, "recovered with no record change");

    assert_eq!((r.version(), r.events.len()), (version, events), "judging wrote nothing");
}

#[test]
fn the_listing_badge_and_the_give_up_gate_use_the_same_judgement() {
    let (mut home, id, view) = setup();
    for _ in 0..4 {
        home.record_attempt(&id, "clone", "o", transient("t"), &view).unwrap();
    }
    let r = home.get(&id).unwrap();
    assert_eq!(badges(r, &view), vec!["exhausted · 4 failed attempts"]);
    assert!(home.apply(&id, LifecycleEdge::GiveUp { cause: Cause::new(CauseCategory::Transient, "t") }, &view).is_ok());
}

#[test]
fn trying_badge_shows_count_and_next_time() {
    let (mut home, id, view) = setup();
    assert_eq!(badges(home.get(&id).unwrap(), &view), vec!["requested"]);
    home.record_attempt(&id, "clone", "o", transient("t"), &view).unwrap();
    assert_eq!(badges(home.get(&id).unwrap(), &view), vec!["trying · 1 failed attempts · next at 11000ms"]);
}

#[test]
fn imagebuild_is_the_same_machine_with_its_own_policy() {
    let mut home = Home::<ImageBuildMachine>::new("kiwi").unwrap();
    let id = home.create("img-1", ImageBuildRequest { context: ".".into(), tag: "t".into() }).unwrap();
    assert_eq!(id.kind, "ImageBuild");
    let view = MemoryView::new();
    let fail = || Outcome::Failure { cause: Cause::new(CauseCategory::Transient, "e"), retryable: true };
    home.record_attempt(&id, "build", "t", fail(), &view).unwrap();
    home.record_attempt(&id, "build", "t", fail(), &view).unwrap();
    // max_attempts is 2 for image builds (4 for clones).
    assert!(retry(home.get(&id).unwrap().attempts.rows(), &<ImageBuildKind as LifecycleSpec>::policy()).exhausted());
    let a = CloneMachine::definition_edges();
    let b = ImageBuildMachine::definition_edges();
    assert_eq!(a, b);
}

trait EdgesForTest {
    fn definition_edges() -> Vec<(&'static str, Vec<&'static str>)>;
}
impl<K: LifecycleSpec> EdgesForTest for Lifecycle<K> {
    fn definition_edges() -> Vec<(&'static str, Vec<&'static str>)> {
        use state_machine_prototype::machine::{Machine, Named};
        Self::definition().edges.iter().map(|e| (e.name, e.reachable_targets().iter().map(|s| s.name()).collect())).collect()
    }
}

#[test]
fn storage_derive_attach_points_describe_the_record_tables() {
    use state_machine_prototype::attempt::Attempt;
    use state_machine_prototype::home::EdgeEvent;
    let sql = ddl::<Attempt>();
    assert!(sql.starts_with("CREATE TABLE attempts (owner INTEGER NOT NULL, position INTEGER NOT NULL, what INTEGER"), "{sql}");
    assert!(sql.ends_with("PRIMARY KEY (owner, position))"), "{sql}");
    assert_eq!(child_tables::<EdgeEvent<CloneMachine>>(), vec!["edge_event_inputs"]);
    assert!(!ddl::<EdgeEvent<CloneMachine>>().contains("inputs"));
}
