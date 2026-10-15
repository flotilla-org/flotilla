//! The declaration core: typed edge dispatch, stable codes, validation,
//! the diagram, and the pure step's fixed order (ADR 0064, #3209).

use std::collections::BTreeSet;

use flotilla_state_machines::{
    edges, states, Coded, DefError, Edge, EdgeDef, EdgeKind, Ended, Handle, InputRead, Machine, MachineDef, Ref, Source, StepError, Table,
    Time, Version, WrongState,
};
use hegel::generators as gs;

use super::latch::*;

fn fire(name: &str) -> LatchEdge {
    Fire { signal: Ref::new(name) }.into()
}

// Codes are stable identities: a rename must never change them. This pins them.
#[test]
fn codes_and_names_are_pinned() {
    let states: Vec<_> = Latch::ALL.iter().map(|s| (s.name(), s.code())).collect();
    assert_eq!(states, [("Armed", 1), ("Fired", 2), ("Spent", 3), ("Withdrawn", 4)]);
    let edges: Vec<_> = LatchEdgeKind::ALL.iter().map(|k| (k.name(), k.code())).collect();
    assert_eq!(edges, [("fire", 10), ("withdraw", 11), ("re_arm", 12)]);
    assert_eq!(fire("s").kind(), LatchEdgeKind::Fire);
}

#[test]
fn the_latch_declaration_validates() {
    assert_eq!(LatchMachine::definition().validate(), Ok(()));
}

// The diagram is drawn from the same declaration the engine enforces.
#[test]
fn mermaid_renders_every_edge_and_end() {
    let diagram = LatchMachine::definition().mermaid();
    assert_eq!(
        diagram,
        "---\ntitle: Latch\n---\nstateDiagram-v2\n  direction LR\n  [*] --> Armed\n\
         \x20 Armed --> Fired: fire (chosen)\n  Armed --> Spent: fire (chosen)\n\
         \x20 Armed --> Withdrawn: withdraw\n  Fired --> Withdrawn: withdraw\n  Fired --> Armed: re_arm\n\
         \x20 Spent --> [*]\n  note right of Spent: the latch fired for good\n\
         \x20 Withdrawn --> [*]\n  note right of Withdrawn: the latch was withdrawn\n"
    );
}

fn step(spec: bool, state: Latch, world: &Table<Signal>, edge: &LatchEdge) -> (Result<Latch, StepError<LatchMachine>>, Vec<InputRead>) {
    let def = LatchMachine::definition();
    let mut handle = Handle::<LatchMachine>::new(&spec, state, world, Time::ZERO);
    let result = def.step(&mut handle, edge);
    (result, handle.reads().to_vec())
}

// Ends are checked first and refuse with the end's reason, before the
// from-state or the gate is consulted.
#[test]
fn an_end_refuses_with_its_reason_before_anything_else() {
    let world = Table::new();
    for (state, reason) in [(Latch::Spent, "the latch fired for good"), (Latch::Withdrawn, "the latch was withdrawn")] {
        for edge in [fire("s"), Withdraw { reason: String::new() }.into(), ReArm.into()] {
            let (result, reads) = step(false, state, &world, &edge);
            assert_eq!(result, Err(StepError::Refused(LatchRefusal::Ended(reason))));
            assert!(reads.is_empty(), "an end refusal reads nothing");
        }
    }
}

// The from-state is checked before the gate, so a gate never runs (or reads)
// for an edge that does not leave the current state.
#[test]
fn a_wrong_state_refuses_before_the_gate_reads() {
    let (result, reads) = step(false, Latch::Fired, &Table::new(), &fire("s"));
    assert_eq!(result, Err(StepError::Refused(LatchRefusal::WrongState)));
    assert!(reads.is_empty());
}

// A typed read records its reference and version; a read that finds nothing
// records an absence, because the gate relied on it.
#[test]
fn gate_reads_are_recorded_with_versions_and_absences() {
    let mut world = Table::<Signal>::new();
    let (result, reads) = step(false, Latch::Armed, &world, &fire("bell"));
    assert_eq!(result, Err(StepError::Refused(LatchRefusal::NoSignal)));
    assert_eq!(reads, [InputRead { kind: "signal", name: "bell".into(), version: None }]);

    world.put(&Ref::new("other"), true);
    let version = world.put(&Ref::new("bell"), true);
    assert_eq!(version, Version(2));
    let (result, reads) = step(false, Latch::Armed, &world, &fire("bell"));
    assert_eq!(result, Ok(Latch::Fired));
    assert_eq!(reads, [InputRead { kind: "signal", name: "bell".into(), version: Some(Version(2)) }]);
}

// A chosen target is picked within the declared set.
#[test]
fn a_chosen_target_follows_the_pick() {
    let mut world = Table::<Signal>::new();
    world.put(&Ref::new("bell"), true);
    assert_eq!(step(true, Latch::Armed, &world, &fire("bell")).0, Ok(Latch::Spent));
    assert_eq!(step(false, Latch::Armed, &world, &fire("bell")).0, Ok(Latch::Fired));
}

// A pick outside `may_reach` is a declaration fault, never a silent move.
#[test]
fn a_pick_outside_may_reach_is_refused() {
    struct Rogue;
    impl Machine for Rogue {
        const NAME: &'static str = "Rogue";
        type Spec = ();
        type State = Latch;
        type Edge = LatchEdge;
        type Refusal = LatchRefusal;
        type World = Table<Signal>;
        fn definition() -> MachineDef<Self> {
            MachineDef::new(Latch::Armed).edge(
                EdgeDef::<Self, Fire>::new()
                    .from([Latch::Armed])
                    .to_chosen([Latch::Fired], |_, _| Latch::Spent)
                    .gate("always", |_, _| Ok(())),
            )
        }
    }
    let world = Table::new();
    let mut handle = Handle::<Rogue>::new(&(), Latch::Armed, &world, Time::ZERO);
    assert_eq!(
        Rogue::definition().step(&mut handle, &fire("s")),
        Err(StepError::Undeclared { edge: LatchEdgeKind::Fire, target: Some(Latch::Spent) })
    );
}

// Judgements share the handle: the earliest declared moment wins, and the
// reads are the dependency set.
#[test]
fn a_judgement_reports_value_validity_and_reads() {
    let mut world = Table::<Signal>::new();
    world.put(&Ref::new("bell"), false);
    let mut handle = Handle::<LatchMachine>::new(&false, Latch::Armed, &world, Time::from_micros(5));
    let raised = handle.read(&Ref::new("bell")).copied();
    handle.until(Time::from_micros(30));
    handle.until(Time::from_micros(20));
    handle.until(Time::from_micros(40));
    let judged = handle.judged(raised);
    assert_eq!(judged.value, Some(false));
    assert_eq!(judged.valid_until, Some(Time::from_micros(20)));
    assert_eq!(judged.reads, [InputRead { kind: "signal", name: "bell".into(), version: Some(Version(1)) }]);
}

#[test]
fn table_lookup_and_remove() {
    let mut table = Table::<Signal>::new();
    let bell = Ref::new("bell");
    table.put(&bell, true);
    assert_eq!(table.lookup(&bell), Some((Version(1), &true)));
    assert_eq!(table.remove(&bell), Some(true));
    assert_eq!(table.lookup(&bell), None);
}

// A word shared by a state and an edge is rejected, ignoring case and underscores.
#[test]
fn validate_rejects_a_state_and_edge_sharing_a_word() {
    states! {
        pub enum Clash { Armed = 1, ReArm = 2 }
    }
    impl From<Ended<Clash>> for LatchRefusal {
        fn from(_: Ended<Clash>) -> Self {
            LatchRefusal::WrongState
        }
    }
    impl From<WrongState<Clash, LatchEdgeKind>> for LatchRefusal {
        fn from(_: WrongState<Clash, LatchEdgeKind>) -> Self {
            LatchRefusal::WrongState
        }
    }
    struct Clashing;
    impl Machine for Clashing {
        const NAME: &'static str = "Clashing";
        type Spec = ();
        type State = Clash;
        type Edge = LatchEdge;
        type Refusal = LatchRefusal;
        type World = Table<Signal>;
        fn definition() -> MachineDef<Self> {
            MachineDef::new(Clash::Armed)
                .edge(EdgeDef::<Self, Fire>::new().from([Clash::Armed]).to(Clash::ReArm).gate("g", |_, _| Ok(())))
                .edge(EdgeDef::<Self, Withdraw>::new().from([Clash::Armed]).to(Clash::ReArm).gate("g", |_, _| Ok(())))
                .edge(EdgeDef::<Self, ReArm>::new().from([Clash::ReArm]).to(Clash::Armed).gate("g", |_, _| Ok(())))
        }
    }
    assert_eq!(Clashing::definition().validate(), Err(vec![DefError::NameClash("re_arm")]));
}

// Duplicate codes are rejected, so two names can never share stored identity.
#[test]
fn validate_rejects_duplicate_codes() {
    states! {
        pub enum Twin { One = 1, Two = 1 }
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Go;
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Back;
    edges! {
        pub enum TwinEdge => TwinEdgeKind { Go(Go) = 7, Back(Back) = 7 }
    }
    #[derive(Clone, Debug, PartialEq)]
    pub struct Never;
    impl From<Ended<Twin>> for Never {
        fn from(_: Ended<Twin>) -> Self {
            Never
        }
    }
    impl From<WrongState<Twin, TwinEdgeKind>> for Never {
        fn from(_: WrongState<Twin, TwinEdgeKind>) -> Self {
            Never
        }
    }
    struct Twins;
    impl Machine for Twins {
        const NAME: &'static str = "Twins";
        type Spec = ();
        type State = Twin;
        type Edge = TwinEdge;
        type Refusal = Never;
        type World = ();
        fn definition() -> MachineDef<Self> {
            MachineDef::new(Twin::One)
                .edge(EdgeDef::<Self, Go>::new().from([Twin::One]).to(Twin::Two).gate("g", |_, _| Ok(())))
                .edge(EdgeDef::<Self, Back>::new().from([Twin::Two]).to(Twin::One).gate("g", |_, _| Ok(())))
        }
    }
    assert_eq!(Twins::definition().validate(), Err(vec![DefError::DuplicateStateCode(1), DefError::DuplicateEdgeCode(7)]));
}

// ---- validate() as a property over generated declarations ----

/// One generated edge declaration: which kind, the from-set as a bit mask
/// over the four states, the target (none, fixed, or a chosen bit mask) and
/// whether it is gated.
#[derive(Clone, Debug)]
struct Drawn {
    kind: usize,
    from: u8,
    target: Option<Result<usize, u8>>,
    gated: bool,
}

fn states_in(mask: u8) -> Vec<Latch> {
    Latch::ALL.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, s)| *s).collect()
}

fn build(initial: usize, edges: &[Drawn], ends: &[(usize, bool)]) -> MachineDef<LatchMachine> {
    fn add<A: flotilla_state_machines::EdgeArg<Edge = LatchEdge>>(def: MachineDef<LatchMachine>, d: &Drawn) -> MachineDef<LatchMachine> {
        let mut e = EdgeDef::<LatchMachine, A>::new().from(states_in(d.from));
        e = match d.target {
            None => e,
            Some(Ok(t)) => e.to(Latch::ALL[t]),
            Some(Err(mask)) => e.to_chosen(states_in(mask), |h, _| h.state()),
        };
        if d.gated {
            e = e.gate("drawn", |_, _| Ok(()));
        }
        def.edge(e)
    }
    let mut def = MachineDef::new(Latch::ALL[initial]);
    for d in edges {
        def = match d.kind {
            0 => add::<Fire>(def, d),
            1 => add::<Withdraw>(def, d),
            _ => add::<ReArm>(def, d),
        };
    }
    for (state, reasoned) in ends {
        def = def.end(Latch::ALL[*state], if *reasoned { "done" } else { " " });
    }
    def
}

/// An independent statement of the rules, by fixed-point iteration rather
/// than the engine's depth-first search.
fn expected(initial: usize, edges: &[Drawn], ends: &[(usize, bool)]) -> BTreeSet<DefError> {
    let kinds = LatchEdgeKind::ALL;
    let mut errors = BTreeSet::new();
    let targets = |d: &Drawn| -> Vec<usize> {
        match d.target {
            None => vec![],
            Some(Ok(t)) => vec![t],
            Some(Err(mask)) => (0..4).filter(|i| mask & (1 << i) != 0).collect(),
        }
    };
    let end_states: BTreeSet<usize> = ends.iter().map(|(s, _)| *s).collect();
    for (i, d) in edges.iter().enumerate() {
        let name = kinds[d.kind].name();
        if edges[..i].iter().any(|e| e.kind == d.kind) {
            errors.insert(DefError::DuplicateEdge(name));
        }
        if !d.gated {
            errors.insert(DefError::UngatedEdge(name));
        }
        if d.target.is_none() {
            errors.insert(DefError::EdgeWithoutTarget(name));
        }
        if d.from == 0 {
            errors.insert(DefError::EdgeWithoutSource(name));
        }
        for s in &end_states {
            if d.from & (1 << s) != 0 {
                errors.insert(DefError::EdgeLeavesEnd { edge: name, end: Latch::ALL[*s].name() });
            }
        }
    }
    for (k, kind) in kinds.iter().enumerate() {
        if !edges.iter().any(|d| d.kind == k) {
            errors.insert(DefError::UndeclaredEdge(kind.name()));
        }
    }
    for (i, (s, reasoned)) in ends.iter().enumerate() {
        if ends[..i].iter().any(|(t, _)| t == s) {
            errors.insert(DefError::DuplicateEnd(Latch::ALL[*s].name()));
        }
        if !reasoned {
            errors.insert(DefError::EndWithoutReason(Latch::ALL[*s].name()));
        }
    }
    let mut reached = BTreeSet::from([initial]);
    loop {
        let before = reached.len();
        for d in edges {
            if reached.iter().any(|s| d.from & (1 << s) != 0) {
                reached.extend(targets(d));
            }
        }
        if reached.len() == before {
            break;
        }
    }
    for s in 0..4 {
        if !reached.contains(&s) {
            errors.insert(DefError::UnreachableState(Latch::ALL[s].name()));
        }
        if !end_states.contains(&s) && !edges.iter().any(|d| d.from & (1 << s) != 0) {
            errors.insert(DefError::DeadState(Latch::ALL[s].name()));
        }
    }
    errors
}

// validate() reports exactly the violated rules for any declaration. The
// generator spans every rule: from-sets and chosen sets over all 16 masks
// (including empty), missing targets and gates, kinds declared zero, one or
// two times, and ends repeated or without a reason.
fn validate_case(tc: hegel::TestCase) {
    let initial = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(5));
    let edges: Vec<Drawn> = (0..count)
        .map(|_| {
            let kind = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let from = tc.draw(gs::integers::<u8>().min_value(0).max_value(15));
            let target = match tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) {
                0 => None,
                1 => Some(Err(tc.draw(gs::integers::<u8>().min_value(0).max_value(15)))),
                _ => Some(Ok(tc.draw(gs::integers::<usize>().min_value(0).max_value(3)))),
            };
            let gated = tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) > 0;
            Drawn { kind, from, target, gated }
        })
        .collect();
    let end_count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let ends: Vec<(usize, bool)> = (0..end_count)
        .map(|_| (tc.draw(gs::integers::<usize>().min_value(0).max_value(3)), tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) > 0))
        .collect();
    let actual: BTreeSet<DefError> = build(initial, &edges, &ends).validate().err().unwrap_or_default().into_iter().collect();
    assert_eq!(actual, expected(initial, &edges, &ends), "edges {edges:?}, ends {ends:?}, initial {initial}");
}

// Pure and fast, so it runs more cases than the profile default.
#[test]
fn validate_reports_exactly_the_broken_rules() {
    hegel::Hegel::new(validate_case).settings(hegel::Settings::new().test_cases(400)).run();
}

// Every kind of the edge enum is reachable through the derived dispatch.
#[test]
fn every_edge_kind_has_one_declaration() {
    let def = LatchMachine::definition();
    for kind in EdgeKind::<LatchMachine>::ALL {
        assert_eq!(def.find(*kind).map(|e| e.kind), Some(*kind));
    }
}
