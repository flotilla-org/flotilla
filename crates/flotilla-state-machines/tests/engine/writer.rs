//! The single writer, attempt and note edges, and attempt judgements
//! (ADR 0064 "Machines": state is a value, ends are ends, every write is an
//! edge, and attempts are judged from the tally their edges leave).

use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    attempt::retry, AttemptPolicy, Cause, CauseCategory, DefError, EdgeDef, Fault, Home, InputRead, Machine, MachineDef, Outcome, Ref,
    Resource, Retry, Schedule, StateValue, Table, Version, WriteError,
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

fn failed(detail: &str, retryable: bool) -> Try {
    Try { outcome: Outcome::Failed { cause: cause(detail), retryable } }
}

fn succeeded() -> Try {
    Try { outcome: Outcome::Succeeded }
}

fn note() -> Note {
    Note { subject: "m".into() }
}

/// The retry judgement over the latch's tally, as a listing or a gate takes it.
fn judge_retry(latch: &Resource<LatchMachine>, policy: &AttemptPolicy, now: Timestamp) -> flotilla_state_machines::Judged<Retry> {
    latch.judge(&Table::new(), now, |h| {
        let (failures, last) = tally(h.state());
        retry(h, policy, failures, last)
    })
}

// Admission creates the resource in the state it returns, at version 1.
#[test]
fn creation_stores_the_admitted_state() {
    let mut home = Home::<LatchMachine>::new("kiwi").expect("valid declaration");
    let created = home.create("l", true, &Table::new(), Timestamp(0)).expect("created").resource;
    assert_eq!((created.state(), created.spec(), created.version()), (&armed(0), &true, Version(1)));
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
    assert_eq!((resource.state(), resource.version(), resource.events().len()), (&armed(2), Version(2), 1));
}

// An end refuses every edge, attempts and notes too, with the end's reason.
#[test]
fn an_ended_resource_refuses_every_edge() {
    let mut home = latch();
    home.apply("l", Withdraw { reason: "no longer wanted".into() }, &Table::new(), at(1)).expect("withdrawn");
    let ended = Some(WriteError::Refused(LatchRefusal::Ended("the latch was withdrawn")));
    assert_eq!(home.apply("l", ReArm, &Table::new(), at(2)).err(), ended);
    assert_eq!(home.apply("l", succeeded(), &Table::new(), at(2)).err(), ended);
    assert_eq!(home.apply("l", note(), &Table::new(), at(2)).err(), ended);
    let resource = home.get("l").expect("exists");
    assert_eq!(resource.state(), &Latch::Withdrawn { reason: "no longer wanted".into() });
    assert_eq!((resource.version(), resource.events().len()), (Version(3), 2));
}

// An attempt is an edge like any other: it leaves only the labels it is
// declared from, and its event carries its outcome.
#[test]
fn an_attempt_is_an_edge_on_its_labels() {
    let mut home = latch();
    let event = home.apply("l", failed("down", true), &Table::new(), at(1)).expect("attempted").clone();
    assert_eq!((event.edge, event.from, event.to), (LatchEdgeKind::Try, LatchLabel::Armed, LatchLabel::Armed));
    assert_eq!(event.payload.edge, LatchEdge::Try(failed("down", true)));
    let state = home.get("l").expect("exists").state().clone();
    assert_eq!(tally(&state).0, 1);
    home.apply("l", fire("bell"), &raised("bell"), at(2)).expect("fired");
    assert_eq!(home.apply("l", succeeded(), &Table::new(), at(3)).err(), Some(WriteError::Refused(LatchRefusal::WrongState)));
}

// An edge with a master changes nothing but the log: its event is appended
// and the version rises, and the declaration names the master it is about.
#[test]
fn an_edge_about_a_master_records_only_its_event() {
    let mut home = latch();
    let before = home.get("l").expect("exists").state().clone();
    let event = home.apply("l", note(), &Table::new(), at(1)).expect("noted").clone();
    assert_eq!(home.definition().master(&event.payload.edge).as_deref(), Some("m"));
    assert_eq!(home.definition().master(&fire("bell").into()), None);
    assert!(home.definition().find(LatchEdgeKind::Note).expect("declared").has_master());
    let resource = home.get("l").expect("exists");
    assert_eq!((resource.state(), resource.version(), resource.events().len()), (&before, Version(3), 2));
}

#[test]
fn creation_refuses_a_duplicate_name_and_unknown_names_are_reported() {
    let mut home = latch();
    assert_eq!(home.create("l", true, &Table::new(), Timestamp(0)).err(), Some(WriteError::Exists("l".into())));
    assert_eq!(home.apply("x", ReArm, &Table::new(), at(1)).err(), Some(WriteError::Unknown("x".into())));
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
            Ok(((), armed(0)))
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
                .edge(EdgeDef::<Self, Try>::new().from([Armed]).to([Armed]).changes(["failures"]).ungated("fixture").effect(|_, s, _| s))
                .edge(EdgeDef::<Self, Note>::new().from([Armed]).to([Armed]).about(|n| &n.subject).ungated("fixture").effect(|_, s, _| s))
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

// A retry judgement reads the state's tally, records no read, and changes nothing.
#[test]
fn a_retry_judgement_reads_the_tally_and_writes_nothing() {
    let mut home = latch();
    home.apply("l", failed("down", true), &Table::new(), at(10)).expect("attempted");
    let policy = AttemptPolicy { max_failures: 3, base: 100, factor: 2, cap: 1_000 };
    let before = home.get("l").expect("exists").version();
    let judged = judge_retry(home.get("l").expect("exists"), &policy, at(20));
    assert_eq!(judged.value, Retry { failures: 1, schedule: Schedule::At(at(110)), last_cause: Some(cause("down")) });
    assert_eq!(judged.valid_until, Some(at(110)));
    assert!(judged.reads.is_empty(), "the tally is the resource's own state");
    assert_eq!(home.get("l").expect("exists").version(), before);
}

// A success clears the window's failures; failures exhaust it at the policy's
// limit, or at once when one cannot be retried; re-arming opens a new window.
#[test]
fn the_tally_counts_consecutive_failures_in_the_window() {
    let policy = AttemptPolicy::immediate(2);
    let mut home = latch();
    let retry_now = |home: &Home<LatchMachine>| judge_retry(home.get("l").expect("exists"), &policy, at(30)).value;
    home.apply("l", failed("down", true), &Table::new(), at(10)).expect("attempted");
    home.apply("l", succeeded(), &Table::new(), at(11)).expect("attempted");
    assert_eq!(retry_now(&home), Retry { failures: 0, schedule: Schedule::Now, last_cause: None });
    home.apply("l", failed("down", true), &Table::new(), at(20)).expect("attempted");
    assert_eq!(retry_now(&home), Retry { failures: 1, schedule: Schedule::At(at(20)), last_cause: Some(cause("down")) });
    home.apply("l", failed("down", true), &Table::new(), at(21)).expect("attempted");
    assert_eq!(retry_now(&home), Retry { failures: 2, schedule: Schedule::Exhausted, last_cause: Some(cause("down")) });
    home.apply("l", fire("bell"), &raised("bell"), at(22)).expect("fired");
    home.apply("l", ReArm, &Table::new(), at(23)).expect("re-armed");
    assert_eq!(retry_now(&home), Retry { failures: 0, schedule: Schedule::Now, last_cause: None });
    home.apply("l", failed("broken", false), &Table::new(), at(24)).expect("attempted");
    assert_eq!(retry_now(&home), Retry { failures: 1, schedule: Schedule::Exhausted, last_cause: Some(cause("broken")) });
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
    Note,
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
        8 => Op::Note,
        _ => Op::Wait(tc.draw(gs::integers::<i64>().min_value(0).max_value(500))),
    }
}

/// An independent model of the writer over the latch: the state and version
/// it should hold, every attempt, and where the current window starts.
struct Model {
    state: Latch,
    version: u64,
    /// Every attempt recorded: (at, failed, retryable, cause).
    attempts: Vec<(i64, bool, bool, u8)>,
    /// How many attempts came before the window: the count when the latch
    /// last changed label (the tally is Armed's, so leaving Armed ends it).
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
// raises the version by one, events keep dense positions with rising
// versions, and the retry judgement over the state's tally counts
// consecutive failures only in the current window (the attempts since the
// latch last changed label). The generator interleaves data-only and
// label-changing edges (with and without evidence), attempts of every
// outcome, notes about a master and time, so windows open, move and end. Each case is a cheap in-memory sequence, so it runs many
// more cases than the profile default, enough to reach windows that move.
#[hegel::test(test_cases = 300)]
fn the_writer_follows_its_rules_over_any_sequence(tc: hegel::TestCase) {
    let spent = tc.draw(gs::booleans());
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(40));
    let policy = AttemptPolicy { max_failures: tc.draw(gs::integers::<u32>().min_value(1).max_value(3)), base: 50, factor: 2, cap: 300 };
    let mut home = Home::<LatchMachine>::new("kiwi").expect("valid");
    home.create("l", spent, &Table::new(), Timestamp(0)).expect("created");
    let mut model = Model { state: armed(0), version: 1, attempts: Vec::new(), before: 0 };
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
            Op::Attempt { failed: f, retryable, cause: c } => {
                let attempt = if *f { failed(&c.to_string(), *retryable) } else { succeeded() };
                home.apply("l", attempt, &world, at(now)).map(|_| ())
            }
            Op::Note => home.apply("l", note(), &world, at(now)).map(|_| ()),
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
            (Op::Load { shots }, Latch::Armed { .. }) => Ok(Some(armed(loaded + shots))),
            (Op::Fire { .. }, Latch::Armed { shots: 0, .. }) => Err(LatchRefusal::NoShots),
            (Op::Fire { signal: false }, Latch::Armed { .. }) => Err(LatchRefusal::NoSignal),
            (Op::Fire { signal: true }, Latch::Armed { .. }) => {
                Ok(Some(if spent { Latch::Spent { shots: loaded - 1 } } else { Latch::Fired { shots: loaded - 1, by: "bell".into() } }))
            }
            (Op::Withdraw { reason: true }, _) => Ok(Some(Latch::Withdrawn { reason: "why".into() })),
            (Op::Withdraw { reason: false }, _) => Err(LatchRefusal::EmptyReason),
            (Op::ReArm, Latch::Fired { .. }) => Ok(Some(armed(loaded))),
            (Op::Attempt { .. } | Op::Note, Latch::Armed { .. }) => Ok(None),
            _ => Err(LatchRefusal::WrongState),
        };
        match (&result, &expected) {
            (Ok(()), Ok(next)) => {
                if !matches!(op, Op::Wait(_)) {
                    model.version += 1;
                }
                if let Some(next) = next {
                    if next.label() != model.state.label() {
                        model.before = model.attempts.len();
                    }
                    model.state = next.clone();
                }
                if let Op::Attempt { failed, retryable, cause } = op {
                    model.attempts.push((now, failed, retryable, cause));
                }
            }
            (Err(WriteError::Refused(actual)), Err(wanted)) => assert_eq!(actual, wanted, "{op:?}"),
            (actual, wanted) => panic!("{op:?}: home {actual:?}, model {wanted:?}"),
        }
        let resource = home.get("l").expect("exists");
        // An armed latch's tally is checked through the retry judgement below.
        match (resource.state(), &model.state) {
            (Latch::Armed { shots: actual, .. }, Latch::Armed { shots: wanted, .. }) => assert_eq!(actual, wanted, "{op:?}"),
            (actual, wanted) => assert_eq!(actual, wanted, "{op:?}"),
        }
        assert_eq!(resource.version(), Version(model.version), "{op:?}");
        // Events have dense positions and the versions of the writes that appended them.
        let versions: Vec<u64> = resource.events().rows().iter().map(|row| row.version).collect();
        for (i, row) in resource.events().rows().iter().enumerate() {
            assert_eq!(row.position.0 as usize, i);
        }
        assert_eq!(versions, (2..=model.version).collect::<Vec<_>>(), "one event per accepted write");
        let judged = judge_retry(resource, &policy, at(now));
        assert_eq!(judged.value, model.expected_retry(&policy), "{op:?}");
        assert!(judged.reads.is_empty(), "a retry judgement reads only the state");
    }
}
