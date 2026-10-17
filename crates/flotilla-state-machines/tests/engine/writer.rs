//! The single writer, records, own-record reads and attempt judgements
//! (ADR 0064 "Machines": state is a value, ends are ends, attempts are a history).

use flotilla_schema::{Position, Timestamp};
use flotilla_state_machines::{
    attempt::{retry, window},
    Attempt, AttemptPolicy, Cause, CauseCategory, DefError, EdgeDef, Fault, Home, InputRead, Machine, MachineDef, Receipt, Ref, Retry,
    Schedule, Table, Transport, Version, WriteError, OWN_ATTEMPTS,
};
use hegel::generators as gs;

use super::latch::*;

fn at(micros: i64) -> Timestamp {
    Timestamp::from_micros(micros)
}

fn raised(name: &str) -> Table<Signal> {
    let mut world = Table::new();
    world.put(&Ref::new(name), true);
    world
}

fn fire(name: &str) -> Fire {
    Fire { signal: Ref::new(name) }
}

fn cause(detail: &str) -> Cause {
    Cause::new(CauseCategory::Transient, detail)
}

/// A home with one latch, `l`, loaded with two shots, that fires without
/// spending itself.
fn latch() -> Home<LatchMachine> {
    let mut home = Home::new("kiwi").expect("valid declaration");
    home.create("l", false, &Table::new(), Timestamp(0)).expect("created");
    home.apply("l", Load { shots: 2 }, &Table::new(), at(0)).expect("loaded");
    home
}

fn receipt(micros: i64) -> Receipt {
    Receipt { message: "m".into(), transport: Transport::Turn, at: at(micros) }
}

// Admission creates the resource in the state it returns, at version 1.
#[test]
fn creation_stores_the_admitted_state() {
    let mut home = Home::<LatchMachine>::new("kiwi").expect("valid declaration");
    let created = home.create("l", true, &Table::new(), Timestamp(0)).expect("created").resource;
    assert_eq!((created.state(), created.spec(), created.version()), (&Latch::Armed { shots: 0 }, &true, Version(1)));
    assert!(created.events().is_empty());
}

// An accepted edge stores the next state, raises the version, and appends
// one event holding the typed edge, the labels it moved between, the writer,
// the time and the gate's reads.
#[test]
fn an_applied_edge_stores_the_next_state_and_appends_its_event() {
    let mut home = latch();
    let world = raised("bell");
    let event = home.apply("l", fire("bell"), &world, at(7)).expect("fires").clone();
    assert_eq!((event.edge, &event.payload.edge), (LatchEdgeKind::Fire, &LatchEdge::Fire(fire("bell"))));
    assert_eq!((event.from, event.to, event.at), (LatchLabel::Armed, LatchLabel::Fired, at(7)));
    assert_eq!(event.writer, "kiwi");
    assert_eq!(event.payload.reads, [InputRead { kind: "signal".into(), name: "bell".into(), version: Some(Version(1)) }]);
    let resource = home.get("l").expect("exists");
    assert_eq!(resource.state(), &Latch::Fired { shots: 1, by: "bell".into() });
    assert_eq!((resource.label(), resource.version(), resource.events().len()), (LatchLabel::Fired, Version(3), 2));
    assert_eq!(resource.events().rows()[1].version, 3);
}

// Refusals go back to the caller and are never recorded.
#[test]
fn a_refusal_records_nothing() {
    let mut home = latch();
    let empty = Table::new();
    assert_eq!(home.apply("l", fire("bell"), &empty, at(1)).err(), Some(WriteError::Refused(LatchRefusal::NoSignal)));
    assert_eq!(home.apply("l", ReArm, &empty, at(1)).err(), Some(WriteError::Refused(LatchRefusal::WrongState)));
    assert_eq!(home.apply("l", Load { shots: 0 }, &empty, at(1)).err(), Some(WriteError::Refused(LatchRefusal::NoShots)));
    let resource = home.get("l").expect("exists");
    assert_eq!((resource.state(), resource.version(), resource.events().len()), (&Latch::Armed { shots: 2 }, Version(2), 1));
}

// An end refuses edges and records alike, with the end's reason.
#[test]
fn an_ended_resource_refuses_edges_and_records() {
    let mut home = latch();
    home.apply("l", Withdraw { reason: "no longer wanted".into() }, &Table::new(), at(1)).expect("withdrawn");
    let ended = Some(WriteError::Refused(LatchRefusal::Ended("the latch was withdrawn")));
    assert_eq!(home.apply("l", ReArm, &Table::new(), at(2)).err(), ended);
    assert_eq!(home.record_attempt("l", Attempt::succeeded(at(2))).err(), ended);
    assert_eq!(home.record_receipt("l", receipt(2)).err(), ended);
    let resource = home.get("l").expect("exists");
    assert_eq!(resource.state(), &Latch::Withdrawn { reason: "no longer wanted".into() });
    assert_eq!((resource.version(), resource.attempts().len(), resource.receipts().len()), (Version(3), 0, 0));
}

// Attempts are recorded only under the labels the machine declares.
#[test]
fn attempts_outside_their_labels_are_refused() {
    let mut home = latch();
    home.apply("l", fire("bell"), &raised("bell"), at(1)).expect("fired");
    assert_eq!(home.record_attempt("l", Attempt::succeeded(at(2))).err(), Some(WriteError::Refused(LatchRefusal::WrongState)));
    assert_eq!(home.record_receipt("l", receipt(2)), Ok(Position(0)));
}

#[test]
fn creation_refuses_a_duplicate_name_and_unknown_names_are_reported() {
    let mut home = latch();
    assert_eq!(home.create("l", true, &Table::new(), Timestamp(0)).err(), Some(WriteError::Exists("l".into())));
    assert_eq!(home.apply("x", ReArm, &Table::new(), at(1)).err(), Some(WriteError::Unknown("x".into())));
    assert_eq!(home.record_attempt("x", Attempt::succeeded(at(1))).err(), Some(WriteError::Unknown("x".into())));
    assert_eq!(home.record_receipt("x", receipt(1)).err(), Some(WriteError::Unknown("x".into())));
    assert_eq!(home.judge("x", &Table::new(), at(1), |h| h.state().clone()).err(), Some(WriteError::Unknown("x".into())));
}

// A home refuses a declaration that breaks the rules.
#[test]
fn a_home_refuses_an_invalid_declaration() {
    struct Broken;
    impl Machine for Broken {
        const NAME: &'static str = "Broken";
        type Request = ();
        type Spec = ();
        type State = Latch;
        type Edge = LatchEdge;
        type Refusal = LatchRefusal;
        type World = Table<Signal>;
        fn definition() -> MachineDef<Self> {
            MachineDef::new(LatchLabel::Armed)
        }
        fn admit(_: &mut flotilla_state_machines::Admission<'_, Self>, _: ()) -> Result<((), Latch), LatchRefusal> {
            Ok(((), Latch::Armed { shots: 0 }))
        }
    }
    let errors = Home::<Broken>::new("kiwi").err().expect("refused");
    assert!(errors.contains(&DefError::UndeclaredEdge("fire")));
    assert!(errors.contains(&DefError::UnreachableState("Fired")));
}

// Admission must produce the declared initial label; anything else is a
// declaration fault, and nothing is created.
#[test]
fn admission_outside_the_initial_label_is_a_fault() {
    struct Late;
    impl Machine for Late {
        const NAME: &'static str = "Late";
        type Request = ();
        type Spec = ();
        type State = Latch;
        type Edge = LatchEdge;
        type Refusal = LatchRefusal;
        type World = Table<Signal>;
        fn definition() -> MachineDef<Self> {
            use LatchLabel::*;
            MachineDef::new(Armed)
                .edge(EdgeDef::<Self, Load>::new().from([Armed]).to([Armed]).changes(["shots"]).ungated("fixture").effect(|_, s, _| s))
                .edge(EdgeDef::<Self, Fire>::new().from([Armed]).to([Fired]).ungated("fixture").effect(|_, s, _| s))
                .edge(EdgeDef::<Self, Withdraw>::new().from([Fired]).to([Withdrawn]).ungated("fixture").effect(|_, s, _| s))
                .edge(EdgeDef::<Self, ReArm>::new().from([Fired]).to([Spent]).ungated("fixture").effect(|_, s, _| s))
                .end(Spent, "spent")
                .end(Withdrawn, "withdrawn")
        }
        fn admit(_: &mut flotilla_state_machines::Admission<'_, Self>, _: ()) -> Result<((), Latch), LatchRefusal> {
            Ok(((), Latch::Spent { shots: 0 }))
        }
    }
    let mut home = Home::<Late>::new("kiwi").expect("valid declaration");
    assert_eq!(
        home.create("l", (), &Table::new(), Timestamp(0)).err(),
        Some(WriteError::Fault(Fault::Initial { state: LatchLabel::Spent }))
    );
    assert!(home.get("l").is_none());
}

// A judgement reads through the recording handle and changes nothing.
#[test]
fn a_judgement_records_own_reads_and_writes_nothing() {
    let mut home = latch();
    home.record_attempt("l", Attempt::failed(at(10), cause("down"), true)).expect("recorded");
    let policy = AttemptPolicy { max_failures: 3, base: 100, factor: 2, cap: 1_000 };
    let before = home.get("l").expect("exists").version();
    let judged = home.judge("l", &Table::new(), at(20), |h| retry(h, &policy, 0)).expect("judged");
    assert_eq!(judged.value, Retry { failures: 1, schedule: Schedule::At(at(110)), last_cause: Some(cause("down")) });
    assert_eq!(judged.valid_until, Some(at(110)));
    assert_eq!(judged.reads, [InputRead { kind: OWN_ATTEMPTS.into(), name: "l".into(), version: Some(Version(1)) }]);
    assert_eq!(home.get("l").expect("exists").version(), before);
}

// The retry judgement counts only the window the caller names: failures
// before it stay in the history but no longer exhaust the policy.
#[test]
fn retry_counts_only_the_named_window() {
    let mut home = latch();
    for micros in [10, 20] {
        home.record_attempt("l", Attempt::failed(at(micros), cause("down"), true)).expect("recorded");
    }
    let policy = AttemptPolicy::immediate(2);
    let judge = |before| home.judge("l", &Table::new(), at(30), |h| (retry(h, &policy, before), window(h, before).len())).expect("judged");
    assert_eq!(judge(0).value, (Retry { failures: 2, schedule: Schedule::Exhausted, last_cause: Some(cause("down")) }, 2));
    assert_eq!(judge(1).value, (Retry { failures: 1, schedule: Schedule::At(at(20)), last_cause: Some(cause("down")) }, 1));
    assert_eq!(judge(2).value, (Retry { failures: 0, schedule: Schedule::Now, last_cause: None }, 0));
    assert_eq!(judge(5).value.1, 0, "a window past the history is empty");
}

// The backoff never exceeds its cap and never shrinks as failures grow.
#[hegel::test]
fn backoff_is_monotone_and_capped(tc: hegel::TestCase) {
    // Bases and caps from zero through large values, factors 0..=4 (0 acts as 1),
    // and failure counts across the doubling range where saturation matters.
    let base = tc.draw(gs::integers::<i64>().min_value(-5).max_value(1_000_000));
    let cap = tc.draw(gs::integers::<i64>().min_value(-5).max_value(10_000_000));
    let factor = tc.draw(gs::integers::<u32>().min_value(0).max_value(4));
    let failures = tc.draw(gs::integers::<u32>().min_value(1).max_value(70));
    let policy = AttemptPolicy { max_failures: 100, base, factor, cap };
    let (this, next) = (policy.backoff(failures), policy.backoff(failures + 1));
    assert!(this <= next, "backoff shrank: {this} then {next}");
    assert!((0..=cap.max(0)).contains(&this));
}

/// One generated operation against the latch home.
#[derive(Clone, Debug)]
enum Op {
    Load { shots: u32 },
    Fire { signal: bool },
    Withdraw { reason: bool },
    ReArm,
    Attempt { failed: bool, retryable: bool, cause: u8 },
    Receipt,
    Wait(i64),
}

fn draw_op(tc: &hegel::TestCase) -> Op {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(10)) {
        0 => Op::Load { shots: tc.draw(gs::integers::<u32>().min_value(0).max_value(2)) },
        1 => Op::Fire { signal: tc.draw(gs::booleans()) },
        2 => Op::Withdraw { reason: tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) == 0 },
        3 => Op::ReArm,
        4..=7 => Op::Attempt {
            failed: tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) > 0,
            retryable: tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) > 0,
            cause: tc.draw(gs::integers::<u8>().min_value(0).max_value(1)),
        },
        8 => Op::Receipt,
        _ => Op::Wait(tc.draw(gs::integers::<i64>().min_value(0).max_value(500))),
    }
}

/// An independent model of the writer over the latch: the state and version
/// it should hold, and the attempts and window the test names.
struct Model {
    state: Latch,
    version: u64,
    /// Every attempt recorded: (at, failed, retryable, cause).
    attempts: Vec<(i64, bool, bool, u8)>,
    /// How many attempts came before the window: the count at the last re_arm.
    before: usize,
}

impl Model {
    fn expected_retry(&self, policy: &AttemptPolicy) -> Retry {
        let trailing: Vec<_> = self.attempts[self.before..].iter().rev().take_while(|a| a.1).collect();
        let Some(last) = trailing.first() else {
            return Retry { failures: 0, schedule: Schedule::Now, last_cause: None };
        };
        let failures = trailing.len() as u32;
        let schedule = if !last.2 || failures >= policy.max_failures {
            Schedule::Exhausted
        } else {
            Schedule::At(at(last.0 + policy.backoff(failures)))
        };
        Retry { failures, schedule, last_cause: Some(cause(&last.3.to_string())) }
    }
}

// Behaviour over any operation sequence: the home holds exactly the state
// the latch's rules produce, refusals change nothing, every accepted write
// raises the version by one, rows keep dense positions with rising versions,
// and the retry judgement counts consecutive failures only in the window the
// caller names (here, the attempts since the last re_arm). The generator
// interleaves data-only and label-changing edges (with and without
// evidence), attempts of every outcome, receipts and time, so windows open,
// move and end. Each case is a cheap in-memory sequence, so it runs many
// more cases than the profile default, enough to reach windows that move.
#[hegel::test(test_cases = 300)]
fn the_writer_follows_its_rules_over_any_sequence(tc: hegel::TestCase) {
    let spent = tc.draw(gs::booleans());
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(40));
    let policy = AttemptPolicy { max_failures: tc.draw(gs::integers::<u32>().min_value(1).max_value(3)), base: 50, factor: 2, cap: 300 };
    let mut home = Home::<LatchMachine>::new("kiwi").expect("valid");
    home.create("l", spent, &Table::new(), Timestamp(0)).expect("created");
    let mut model = Model { state: Latch::Armed { shots: 0 }, version: 1, attempts: Vec::new(), before: 0 };
    let mut now = 0i64;
    for _ in 0..steps {
        let op = draw_op(&tc);
        let world = raised("bell");
        let result: Result<(), WriteError<LatchMachine>> = match &op {
            Op::Load { shots } => home.apply("l", Load { shots: *shots }, &world, at(now)).map(|_| ()),
            Op::Fire { signal } => home.apply("l", fire(if *signal { "bell" } else { "quiet" }), &world, at(now)).map(|_| ()),
            Op::Withdraw { reason } => {
                home.apply("l", Withdraw { reason: if *reason { "why".into() } else { " ".into() } }, &world, at(now)).map(|_| ())
            }
            Op::ReArm => home.apply("l", ReArm, &world, at(now)).map(|_| ()),
            Op::Attempt { failed, retryable, cause: c } => {
                let attempt =
                    if *failed { Attempt::failed(at(now), cause(&c.to_string()), *retryable) } else { Attempt::succeeded(at(now)) };
                home.record_attempt("l", attempt).map(|_| ())
            }
            Op::Receipt => home.record_receipt("l", receipt(now)).map(|_| ()),
            Op::Wait(micros) => {
                now += micros;
                Ok(())
            }
        };
        // The oracle: what each operation should do from the model's state.
        let loaded = shots(&model.state);
        let expected: Result<Option<Latch>, LatchRefusal> = match (&op, &model.state) {
            (Op::Wait(_), _) => Ok(None),
            (_, Latch::Spent { .. }) => Err(LatchRefusal::Ended("the latch fired for good")),
            (_, Latch::Withdrawn { .. }) => Err(LatchRefusal::Ended("the latch was withdrawn")),
            (Op::Load { shots: 0 }, Latch::Armed { .. }) => Err(LatchRefusal::NoShots),
            (Op::Load { shots }, Latch::Armed { .. }) => Ok(Some(Latch::Armed { shots: loaded + shots })),
            (Op::Fire { .. }, Latch::Armed { shots: 0 }) => Err(LatchRefusal::NoShots),
            (Op::Fire { signal: false }, Latch::Armed { .. }) => Err(LatchRefusal::NoSignal),
            (Op::Fire { signal: true }, Latch::Armed { .. }) => {
                Ok(Some(if spent { Latch::Spent { shots: loaded - 1 } } else { Latch::Fired { shots: loaded - 1, by: "bell".into() } }))
            }
            (Op::Withdraw { reason: true }, _) => Ok(Some(Latch::Withdrawn { reason: "why".into() })),
            (Op::Withdraw { reason: false }, _) => Err(LatchRefusal::EmptyReason),
            (Op::ReArm, Latch::Fired { .. }) => Ok(Some(Latch::Armed { shots: loaded })),
            (Op::Attempt { .. }, Latch::Armed { .. }) | (Op::Receipt, _) => Ok(None),
            _ => Err(LatchRefusal::WrongState),
        };
        match (&result, &expected) {
            (Ok(()), Ok(next)) => {
                if !matches!(op, Op::Wait(_)) {
                    model.version += 1;
                }
                if let Some(next) = next {
                    model.state = next.clone();
                }
                match op {
                    Op::ReArm => model.before = model.attempts.len(),
                    Op::Attempt { failed, retryable, cause } => model.attempts.push((now, failed, retryable, cause)),
                    _ => {}
                }
            }
            (Err(WriteError::Refused(actual)), Err(wanted)) => assert_eq!(actual, wanted, "{op:?}"),
            (actual, wanted) => panic!("{op:?}: home {actual:?}, model {wanted:?}"),
        }
        let resource = home.get("l").expect("exists");
        assert_eq!(resource.state(), &model.state, "{op:?}");
        assert_eq!(resource.version(), Version(model.version), "{op:?}");
        // Rows across all logs have dense positions and distinct, rising versions.
        let mut versions = Vec::new();
        for (i, row) in resource.events().rows().iter().enumerate() {
            assert_eq!(row.position.0 as usize, i);
            versions.push(row.version);
        }
        for (i, row) in resource.attempts().rows().iter().enumerate() {
            assert_eq!(row.position.0 as usize, i);
            versions.push(row.version);
        }
        for (i, row) in resource.receipts().rows().iter().enumerate() {
            assert_eq!(row.position.0 as usize, i);
            versions.push(row.version);
        }
        versions.sort();
        versions.dedup();
        assert_eq!(versions.len() as u64, model.version - 1, "one row per accepted write");
        assert!(versions.iter().all(|v| *v >= 2 && *v <= model.version));
        let judged = resource.judge(&world, at(now), |h| (retry(h, &policy, model.before), window(h, model.before).len()));
        assert_eq!(judged.value.0, model.expected_retry(&policy), "{op:?}");
        assert_eq!(judged.value.1, model.attempts.len() - model.before);
        assert!(judged.reads.iter().all(|r| r.kind == OWN_ATTEMPTS), "a retry judgement reads only the attempts");
    }
}
