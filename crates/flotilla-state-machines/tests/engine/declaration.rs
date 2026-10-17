//! The declaration core: state values with derived labels, typed edge
//! dispatch, stable codes, validation, the diagram, and the pure step's
//! fixed order (ADR 0064, #3209).

use std::collections::BTreeSet;

use flotilla_schema::{CodeEnum, Column, Columns, DecodeError, Decoder, EncodeError, Encoder, Layout, Timestamp};
use flotilla_state_machines::{
    changed, states, Coded, Datum, DefError, Diagram, Edge, EdgeArg, EdgeDef, EdgeKind, Ended, Fault, Handle, InputRead, Machine,
    MachineDef, Ref, Source, StateValue, StepError, Table, Version, WrongState,
};
use hegel::generators as gs;

use super::latch::*;

fn fire(name: &str) -> LatchEdge {
    Fire { signal: Ref::new(name) }.into()
}

fn armed(shots: u32) -> Latch {
    Latch::Armed { shots }
}

// Codes are stable identities: a rename must never change them. This pins them.
#[test]
fn codes_and_names_are_pinned() {
    let labels: Vec<_> = LatchLabel::ALL.iter().map(|s| (s.name(), s.code())).collect();
    assert_eq!(labels, [("Armed", 1), ("Fired", 2), ("Spent", 3), ("Withdrawn", 4)]);
    let edges: Vec<_> = LatchEdgeKind::ALL.iter().map(|k| (k.name(), k.code())).collect();
    assert_eq!(edges, [("fire", 10), ("withdraw", 11), ("re_arm", 12), ("load", 13)]);
    assert_eq!(fire("s").kind(), LatchEdgeKind::Fire);
}

// A state is a label plus its named data: the label is derived from the
// value, and each label names the data it carries in declaration order.
#[test]
fn a_state_value_names_its_label_and_data() {
    let fired = Latch::Fired { shots: 2, by: "bell".into() };
    assert_eq!(fired.label(), LatchLabel::Fired);
    let data: Vec<_> = fired.data().into_iter().map(|(name, value)| (name, format!("{value:?}"))).collect();
    assert_eq!(data, [("shots", "2".to_string()), ("by", "\"bell\"".to_string())]);
    let fields: Vec<_> = LatchLabel::ALL.iter().map(|label| Latch::fields(*label)).collect();
    assert_eq!(fields, [&["shots"][..], &["shots", "by"], &["shots"], &["reason"]]);
}

#[test]
fn the_latch_declaration_validates() {
    assert_eq!(LatchMachine::definition().validate(), Ok(()));
}

// The diagram is drawn from the same declaration the engine enforces: each
// transition names its edge and the data it changes. The label view leaves
// out the edge that changes only data.
#[test]
fn mermaid_renders_every_edge_its_data_and_each_end() {
    let head = "---\ntitle: Latch\n---\nstateDiagram-v2\n  direction LR\n  [*] --> Armed\n";
    let moves = "  Armed --> Fired: fire [shots, by]\n  Armed --> Spent: fire [shots, by]\n\
                 \x20 Armed --> Withdrawn: withdraw [reason]\n  Fired --> Withdrawn: withdraw [reason]\n\
                 \x20 Fired --> Armed: re_arm\n";
    let ends = "  Spent --> [*]\n  note right of Spent: the latch fired for good\n\
                \x20 Withdrawn --> [*]\n  note right of Withdrawn: the latch was withdrawn\n";
    let definition = LatchMachine::definition();
    assert_eq!(definition.mermaid(Diagram::Full), format!("{head}  Armed --> Armed: load [shots]\n{moves}{ends}"));
    assert_eq!(definition.mermaid(Diagram::Labels), format!("{head}{moves}{ends}"));
}

fn step(spec: bool, state: &Latch, world: &Table<Signal>, edge: &LatchEdge) -> (Result<Latch, StepError<LatchMachine>>, Vec<InputRead>) {
    let def = LatchMachine::definition();
    let mut handle = Handle::<LatchMachine>::new(&spec, state, world, Timestamp::ZERO);
    let result = def.step(&mut handle, edge);
    (result, handle.reads().to_vec())
}

// Ends are checked first and refuse with the end's reason, before the
// from-label or the gate is consulted.
#[test]
fn an_end_refuses_with_its_reason_before_anything_else() {
    let world = Table::new();
    let ends =
        [(Latch::Spent { shots: 1 }, "the latch fired for good"), (Latch::Withdrawn { reason: "gone".into() }, "the latch was withdrawn")];
    for (state, reason) in ends {
        for edge in [fire("s"), Withdraw { reason: String::new() }.into(), ReArm.into(), Load { shots: 1 }.into()] {
            let (result, reads) = step(false, &state, &world, &edge);
            assert_eq!(result, Err(StepError::Refused(LatchRefusal::Ended(reason))));
            assert!(reads.is_empty(), "an end refusal reads nothing");
        }
    }
}

// The from-label is checked before the gate, so a gate never runs (or reads)
// for an edge that does not leave the current label.
#[test]
fn a_wrong_state_refuses_before_the_gate_reads() {
    let (result, reads) = step(false, &Latch::Fired { shots: 1, by: "bell".into() }, &Table::new(), &fire("s"));
    assert_eq!(result, Err(StepError::Refused(LatchRefusal::WrongState)));
    assert!(reads.is_empty());
}

// Gates read the current state's data: an armed latch with no shots refuses
// to fire, before it reads the signal.
#[test]
fn a_gate_reads_the_current_state() {
    let (result, reads) = step(false, &armed(0), &Table::new(), &fire("bell"));
    assert_eq!(result, Err(StepError::Refused(LatchRefusal::NoShots)));
    assert!(reads.is_empty());
}

// A typed read records its reference and version; a read that finds nothing
// records an absence, because the gate relied on it.
#[test]
fn gate_reads_are_recorded_with_versions_and_absences() {
    let mut world = Table::<Signal>::new();
    let (result, reads) = step(false, &armed(1), &world, &fire("bell"));
    assert_eq!(result, Err(StepError::Refused(LatchRefusal::NoSignal)));
    assert_eq!(reads, [InputRead { kind: "signal".into(), name: "bell".into(), version: None }]);

    world.put(&Ref::new("other"), true);
    let version = world.put(&Ref::new("bell"), true);
    assert_eq!(version, Version(2));
    let (result, reads) = step(false, &armed(1), &world, &fire("bell"));
    assert_eq!(result, Ok(Latch::Fired { shots: 0, by: "bell".into() }));
    assert_eq!(reads, [InputRead { kind: "signal".into(), name: "bell".into(), version: Some(Version(2)) }]);
}

// The effect produces the next state from the current value: the target
// label is the effect's choice within the declared set, and data carries
// over or changes as the effect writes it.
#[test]
fn the_effect_produces_the_next_state() {
    let mut world = Table::<Signal>::new();
    world.put(&Ref::new("bell"), true);
    assert_eq!(step(true, &armed(3), &world, &fire("bell")).0, Ok(Latch::Spent { shots: 2 }));
    assert_eq!(step(false, &armed(3), &world, &fire("bell")).0, Ok(Latch::Fired { shots: 2, by: "bell".into() }));
    assert_eq!(step(false, &armed(3), &world, &Load { shots: 2 }.into()).0, Ok(armed(5)));
    assert_eq!(step(false, &Latch::Fired { shots: 2, by: "bell".into() }, &world, &ReArm.into()).0, Ok(armed(2)));
}

// An ungated edge runs no gate and reads nothing: its declaration states why.
#[test]
fn an_ungated_edge_states_its_reason_and_reads_nothing() {
    let definition = LatchMachine::definition();
    let re_arm = definition.find(LatchEdgeKind::ReArm).expect("declared");
    assert_eq!((re_arm.gate_doc(), re_arm.ungated_reason()), (None, Some("re-arming a fired latch needs no evidence")));
    let (result, reads) = step(false, &Latch::Fired { shots: 0, by: "bell".into() }, &Table::new(), &ReArm.into());
    assert_eq!(result, Ok(armed(0)));
    assert!(reads.is_empty());
}

// A machine whose effects break their declarations, for the fault tests: its
// `fire` may reach only Fired yet spends the latch, and its `load` declares
// no data yet changes the shots.
struct Rogue;

impl Machine for Rogue {
    const NAME: &'static str = "Rogue";
    type Request = ();
    type Spec = ();
    type State = Latch;
    type Edge = LatchEdge;
    type Refusal = LatchRefusal;
    type World = Table<Signal>;
    fn definition() -> MachineDef<Self> {
        MachineDef::new(LatchLabel::Armed)
            .edge(
                EdgeDef::<Self, Fire>::new()
                    .from([LatchLabel::Armed])
                    .to([LatchLabel::Fired])
                    .changes(["shots"])
                    .ungated("a test fixture")
                    .effect(|_, _, _| Latch::Spent { shots: 0 }),
            )
            .edge(
                EdgeDef::<Self, Load>::new()
                    .from([LatchLabel::Armed])
                    .to([LatchLabel::Armed])
                    .ungated("a test fixture")
                    .effect(|_, state, load| Latch::Armed { shots: shots(&state) + load.shots }),
            )
    }
    fn admit(_: &mut flotilla_state_machines::Admission<'_, Self>, _: ()) -> Result<((), Latch), LatchRefusal> {
        Ok(((), armed(0)))
    }
}

// A next state outside the declared targets is a declaration fault, never a
// silent move.
#[test]
fn a_target_outside_the_declaration_is_a_fault() {
    let world = Table::new();
    let state = armed(1);
    let mut handle = Handle::<Rogue>::new(&(), &state, &world, Timestamp::ZERO);
    assert_eq!(
        Rogue::definition().step(&mut handle, &fire("s")),
        Err(StepError::Fault(Fault::Target { edge: LatchEdgeKind::Fire, target: LatchLabel::Spent }))
    );
}

// Changing data the edge does not declare is a declaration fault, so a
// declaration's list of changed data is exactly what the engine allows. An
// effect that leaves the data as it was changes nothing.
#[test]
fn changing_undeclared_data_is_a_fault() {
    let world = Table::new();
    let state = armed(1);
    let definition = Rogue::definition();
    let mut handle = Handle::<Rogue>::new(&(), &state, &world, Timestamp::ZERO);
    assert_eq!(
        definition.step(&mut handle, &Load { shots: 2 }.into()),
        Err(StepError::Fault(Fault::Changes { edge: LatchEdgeKind::Load, data: vec!["shots"] }))
    );
    let mut handle = Handle::<Rogue>::new(&(), &state, &world, Timestamp::ZERO);
    assert_eq!(definition.step(&mut handle, &Load { shots: 0 }.into()), Ok(armed(1)));
}

// What a move changes: each datum the next state carries that the previous
// one lacks or holds with another value. Data left behind by a label change
// goes with the label.
#[test]
fn changed_names_new_and_altered_data_only() {
    let fired = Latch::Fired { shots: 2, by: "bell".into() };
    assert_eq!(changed(&armed(2), &fired), BTreeSet::from(["by"]));
    assert_eq!(changed(&armed(3), &fired), BTreeSet::from(["shots", "by"]));
    assert_eq!(changed(&fired, &armed(2)), BTreeSet::new());
    assert_eq!(changed(&fired, &Latch::Withdrawn { reason: "r".into() }), BTreeSet::from(["reason"]));
}

fn draw_latch(tc: &hegel::TestCase) -> Latch {
    let shots = tc.draw(gs::integers::<u32>().min_value(0).max_value(2));
    let word = |tc: &hegel::TestCase| ["", "bell", "horn"][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))].to_string();
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) {
        0 => Latch::Armed { shots },
        1 => Latch::Fired { shots, by: word(tc) },
        2 => Latch::Spent { shots },
        _ => Latch::Withdrawn { reason: word(tc) },
    }
}

// For any two state values: nothing changes when a value moves to itself;
// what changes is always data the next label carries; and every datum the
// next value carries either changed or is held equal by the previous value.
// The generator covers every label, with colliding and distinct shots and
// words, so equal data across different labels is drawn.
#[hegel::test(test_cases = 300)]
fn changed_is_exactly_the_data_that_differs(tc: hegel::TestCase) {
    let (previous, next) = (draw_latch(&tc), draw_latch(&tc));
    assert!(changed(&next, &next).is_empty());
    let changes = changed(&previous, &next);
    let fields: BTreeSet<_> = Latch::fields(next.label()).iter().copied().collect();
    assert!(changes.is_subset(&fields), "{previous:?} -> {next:?}: {changes:?}");
    for (name, value) in next.data() {
        let held = previous.data().into_iter().any(|(old, datum)| old == name && datum.same(value));
        assert_eq!(changes.contains(name), !held, "{previous:?} -> {next:?}: {name}");
    }
}

// Judgements share the handle: the earliest declared moment wins, and the
// reads are the dependency set, each distinct read recorded once.
#[test]
fn a_judgement_reports_value_validity_and_reads() {
    let mut world = Table::<Signal>::new();
    world.put(&Ref::new("bell"), false);
    let state = armed(0);
    let mut handle = Handle::<LatchMachine>::new(&false, &state, &world, Timestamp::from_micros(5));
    let raised = handle.read(&Ref::new("bell")).copied();
    handle.read(&Ref::<Signal>::new("bell"));
    handle.until(Timestamp::from_micros(30));
    handle.until(Timestamp::from_micros(20));
    handle.until(Timestamp::from_micros(40));
    let judged = handle.judged(raised);
    assert_eq!(judged.value, Some(false));
    assert_eq!(judged.valid_until, Some(Timestamp::from_micros(20)));
    assert_eq!(judged.reads, [InputRead { kind: "signal".into(), name: "bell".into(), version: Some(Version(1)) }]);
}

// A table's versions only rise: a value removed and written again is newer
// than anything read before, so a recorded read can never be confused with it.
#[test]
fn table_versions_rise_across_removal() {
    let mut table = Table::<Signal>::new();
    let bell = Ref::new("bell");
    table.put(&bell, true);
    assert_eq!(table.lookup(&bell), Some((Version(1), &true)));
    assert_eq!(table.remove(&bell), Some(true));
    assert_eq!(table.lookup(&bell), None);
    table.put(&bell, true);
    assert_eq!(table.lookup(&bell), Some((Version(2), &true)));
}

// A word shared by a label and an edge is rejected, ignoring case and underscores.
#[test]
fn validate_rejects_a_label_and_edge_sharing_a_word() {
    states! {
        pub enum Clash => ClashLabel { Armed = 1, ReArm = 2, Loaded = 3 }
    }
    impl From<Ended<ClashLabel>> for LatchRefusal {
        fn from(_: Ended<ClashLabel>) -> Self {
            LatchRefusal::WrongState
        }
    }
    impl From<WrongState<ClashLabel, LatchEdgeKind>> for LatchRefusal {
        fn from(_: WrongState<ClashLabel, LatchEdgeKind>) -> Self {
            LatchRefusal::WrongState
        }
    }
    struct Clashing;
    impl Machine for Clashing {
        const NAME: &'static str = "Clashing";
        type Request = ();
        type Spec = ();
        type State = Clash;
        type Edge = LatchEdge;
        type Refusal = LatchRefusal;
        type World = Table<Signal>;
        fn definition() -> MachineDef<Self> {
            fn edge<A: EdgeArg<Edge = LatchEdge>>(from: ClashLabel, to: ClashLabel) -> EdgeDef<Clashing, A> {
                EdgeDef::new().from([from]).to([to]).ungated("a test fixture").effect(|_, _, _| Clash::ReArm)
            }
            use ClashLabel::{Armed, Loaded};
            MachineDef::new(Armed)
                .edge(edge::<Fire>(Armed, ClashLabel::ReArm))
                .edge(edge::<Withdraw>(Armed, ClashLabel::ReArm))
                .edge(edge::<ReArm>(ClashLabel::ReArm, Loaded))
                .edge(edge::<Load>(Loaded, Armed))
        }
        fn admit(_: &mut flotilla_state_machines::Admission<'_, Self>, _: ()) -> Result<((), Clash), LatchRefusal> {
            Ok(((), Clash::Armed))
        }
    }
    assert_eq!(Clashing::definition().validate(), Err(vec![DefError::NameClash("re_arm")]));
}

/// A hand-written code enum, outside the macros' `Stored` derive, whose
/// codes may repeat: the only way a declaration can reach a duplicate code.
macro_rules! hand_coded {
    ($name:ident { $($variant:ident = $code:literal),+ }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl Columns for $name {
            const TABLES: usize = 0;

            fn layout(layout: &mut Layout, name: &str, required: bool) {
                i64::layout(layout, name, required);
            }

            fn encode(&self, e: &mut Encoder) -> Result<(), EncodeError> {
                CodeEnum::code(self).encode(e)
            }

            fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
                i64::encode_absent(e)
            }

            fn decode(d: &mut Decoder) -> Result<Option<Self>, DecodeError> {
                Ok(i64::decode(d)?.and_then(|code| Self::ALL.iter().copied().find(|value| CodeEnum::code(value) == code)))
            }

            fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
                i64::decode_absent(d)
            }
        }

        impl Column for $name {}

        impl CodeEnum for $name {
            const CODES: &'static [i64] = &[$($code),+];

            fn code(&self) -> i64 {
                match self {
                    $(Self::$variant => $code),+
                }
            }
        }

        impl Coded for $name {
            const ALL: &'static [Self] = &[$(Self::$variant),+];

            fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($variant)),+
                }
            }
        }
    };
}

// Duplicate codes are rejected, so two names can never share stored identity.
// The macros' derive refuses a repeated code at compile time; a hand-written
// code enum reaches validation.
#[test]
fn validate_rejects_duplicate_codes() {
    hand_coded!(TwinLabel { One = 1, Two = 1 });
    hand_coded!(TwinEdgeKind { Go = 7, Back = 7 });
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Twin {
        One,
        Two,
    }
    impl StateValue for Twin {
        type Label = TwinLabel;
        fn label(&self) -> TwinLabel {
            match self {
                Twin::One => TwinLabel::One,
                Twin::Two => TwinLabel::Two,
            }
        }
        fn data(&self) -> Vec<(&'static str, &dyn Datum)> {
            Vec::new()
        }
        fn fields(_: TwinLabel) -> &'static [&'static str] {
            &[]
        }
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Go;
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Back;
    #[expect(dead_code, reason = "the fixture is validated, never stepped")]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum TwinEdge {
        Go(Go),
        Back(Back),
    }
    impl Edge for TwinEdge {
        type Kind = TwinEdgeKind;
        fn kind(&self) -> TwinEdgeKind {
            match self {
                TwinEdge::Go(_) => TwinEdgeKind::Go,
                TwinEdge::Back(_) => TwinEdgeKind::Back,
            }
        }
    }
    impl EdgeArg for Go {
        type Edge = TwinEdge;
        const KIND: TwinEdgeKind = TwinEdgeKind::Go;
        fn project(edge: &TwinEdge) -> Option<&Go> {
            match edge {
                TwinEdge::Go(arg) => Some(arg),
                TwinEdge::Back(_) => None,
            }
        }
    }
    impl EdgeArg for Back {
        type Edge = TwinEdge;
        const KIND: TwinEdgeKind = TwinEdgeKind::Back;
        fn project(edge: &TwinEdge) -> Option<&Back> {
            match edge {
                TwinEdge::Back(arg) => Some(arg),
                TwinEdge::Go(_) => None,
            }
        }
    }
    #[derive(Clone, Debug, PartialEq)]
    pub struct Never;
    impl From<Ended<TwinLabel>> for Never {
        fn from(_: Ended<TwinLabel>) -> Self {
            Never
        }
    }
    impl From<WrongState<TwinLabel, TwinEdgeKind>> for Never {
        fn from(_: WrongState<TwinLabel, TwinEdgeKind>) -> Self {
            Never
        }
    }
    struct Twins;
    impl Machine for Twins {
        const NAME: &'static str = "Twins";
        type Request = ();
        type Spec = ();
        type State = Twin;
        type Edge = TwinEdge;
        type Refusal = Never;
        type World = ();
        fn definition() -> MachineDef<Self> {
            MachineDef::new(TwinLabel::One)
                .edge(EdgeDef::<Self, Go>::new().from([TwinLabel::One]).to([TwinLabel::Two]).ungated("fixture").effect(|_, _, _| Twin::Two))
                .edge(
                    EdgeDef::<Self, Back>::new().from([TwinLabel::Two]).to([TwinLabel::One]).ungated("fixture").effect(|_, _, _| Twin::One),
                )
        }
        fn admit(_: &mut flotilla_state_machines::Admission<'_, Self>, _: ()) -> Result<((), Twin), Never> {
            Ok(((), Twin::One))
        }
    }
    assert_eq!(Twins::definition().validate(), Err(vec![DefError::DuplicateStateCode(1), DefError::DuplicateEdgeCode(7)]));
}

// ---- validate() as a property over generated declarations ----

/// How a generated edge is checked.
#[derive(Clone, Copy, Debug)]
enum Checked {
    Gated,
    Ungated { reasoned: bool },
    Unchecked,
}

/// One generated edge declaration: which kind, the from- and to-sets as bit
/// masks over the four labels, the data it declares changing, how it is
/// checked and whether it has an effect.
#[derive(Clone, Debug)]
struct Drawn {
    kind: usize,
    from: u8,
    to: u8,
    changes: Vec<&'static str>,
    check: Checked,
    effect: bool,
}

/// Data names to declare: each label's data, and one no label carries.
const DATA: [&str; 4] = ["shots", "by", "reason", "bogus"];

fn labels_in(mask: u8) -> Vec<LatchLabel> {
    LatchLabel::ALL.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, s)| *s).collect()
}

fn build(initial: usize, edges: &[Drawn], ends: &[(usize, bool)]) -> MachineDef<LatchMachine> {
    fn add<A: EdgeArg<Edge = LatchEdge>>(def: MachineDef<LatchMachine>, d: &Drawn) -> MachineDef<LatchMachine> {
        let mut e = EdgeDef::<LatchMachine, A>::new().from(labels_in(d.from)).to(labels_in(d.to)).changes(d.changes.iter().copied());
        e = match d.check {
            Checked::Gated => e.gate("drawn", |_, _| Ok(())),
            Checked::Ungated { reasoned } => e.ungated(if reasoned { "drawn" } else { " " }),
            Checked::Unchecked => e,
        };
        if d.effect {
            e = e.effect(|_, state, _| state);
        }
        def.edge(e)
    }
    let mut def = MachineDef::new(LatchLabel::ALL[initial]);
    for d in edges {
        def = match d.kind {
            0 => add::<Fire>(def, d),
            1 => add::<Withdraw>(def, d),
            2 => add::<ReArm>(def, d),
            _ => add::<Load>(def, d),
        };
    }
    for (state, reasoned) in ends {
        def = def.end(LatchLabel::ALL[*state], if *reasoned { "done" } else { " " });
    }
    def
}

/// An independent statement of the rules, by fixed-point iteration rather
/// than the engine's depth-first search.
fn expected(initial: usize, edges: &[Drawn], ends: &[(usize, bool)]) -> BTreeSet<DefError> {
    let kinds = LatchEdgeKind::ALL;
    let mut errors = BTreeSet::new();
    let end_states: BTreeSet<usize> = ends.iter().map(|(s, _)| *s).collect();
    for (i, d) in edges.iter().enumerate() {
        let name = kinds[d.kind].name();
        if edges[..i].iter().any(|e| e.kind == d.kind) {
            errors.insert(DefError::DuplicateEdge(name));
        }
        match d.check {
            Checked::Unchecked => {
                errors.insert(DefError::UngatedEdge(name));
            }
            Checked::Ungated { reasoned: false } => {
                errors.insert(DefError::UngatedWithoutReason(name));
            }
            _ => {}
        }
        if !d.effect {
            errors.insert(DefError::EdgeWithoutEffect(name));
        }
        if d.to == 0 {
            errors.insert(DefError::EdgeWithoutTarget(name));
        }
        if d.from == 0 {
            errors.insert(DefError::EdgeWithoutSource(name));
        }
        // Inert: no data changes, and every from-label equals every to-label.
        if d.changes.is_empty() && d.from != 0 && d.from == d.to && d.from.count_ones() == 1 {
            errors.insert(DefError::InertEdge(name));
        }
        for data in &d.changes {
            if !labels_in(d.to).iter().any(|label| Latch::fields(*label).contains(data)) {
                errors.insert(DefError::UnknownData { edge: name, data });
            }
        }
        for s in &end_states {
            if d.from & (1 << s) != 0 {
                errors.insert(DefError::EdgeLeavesEnd { edge: name, end: LatchLabel::ALL[*s].name() });
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
            errors.insert(DefError::DuplicateEnd(LatchLabel::ALL[*s].name()));
        }
        if !reasoned {
            errors.insert(DefError::EndWithoutReason(LatchLabel::ALL[*s].name()));
        }
    }
    let mut reached = BTreeSet::from([initial]);
    loop {
        let before = reached.len();
        for d in edges {
            if reached.iter().any(|s| d.from & (1 << s) != 0) {
                reached.extend((0..4).filter(|t| d.to & (1 << t) != 0));
            }
        }
        if reached.len() == before {
            break;
        }
    }
    for s in 0..4 {
        if !reached.contains(&s) {
            errors.insert(DefError::UnreachableState(LatchLabel::ALL[s].name()));
        }
        if !end_states.contains(&s) && !edges.iter().any(|d| d.from & (1 << s) != 0) {
            errors.insert(DefError::DeadState(LatchLabel::ALL[s].name()));
        }
    }
    errors
}

// validate() reports exactly the violated rules for any declaration. The
// generator spans every rule: from- and to-sets over all 16 masks (including
// empty, and inert self-loops), changed data carried by some labels and by none, gated, ungated
// with and without a reason, and unchecked edges, missing effects, kinds
// declared zero, one or two times, and ends repeated or without a reason.
fn validate_case(tc: hegel::TestCase) {
    let initial = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
    let edges: Vec<Drawn> = (0..count)
        .map(|_| {
            let kind = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
            let from = tc.draw(gs::integers::<u8>().min_value(0).max_value(15));
            let to = tc.draw(gs::integers::<u8>().min_value(0).max_value(15));
            let changes = DATA.iter().copied().filter(|_| tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) == 0).collect();
            let check = match tc.draw(gs::integers::<u8>().min_value(0).max_value(7)) {
                0 => Checked::Unchecked,
                1 => Checked::Ungated { reasoned: false },
                2 => Checked::Ungated { reasoned: true },
                _ => Checked::Gated,
            };
            let effect = tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) > 0;
            Drawn { kind, from, to, changes, check, effect }
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
