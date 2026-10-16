//! Derived tables (`child`, `records`, `history`, maps, sets, tables inside variants), `frozen`
//! and generic types, as layouts and logical rows without a backend.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{
    DecodeError, Decoder, Element, Encoder, Frozen, Hash, Identity, Kind, KindCode, Records, Row, Stored, TableRole, Timestamp, Value,
};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Doc {
    name: String,
    steps: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Stored)]
struct Tag(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Stored)]
enum Phase {
    #[store(code = 1)]
    Early,
    #[store(code = 2)]
    Late,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Gate {
    observed: Identity,
    version: u64,
}

/// A part holding tables: its rows nest them.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Step {
    label: String,
    #[store(child)]
    gates: Vec<Gate>,
    #[store(child)]
    notes: BTreeMap<String, String>,
}

/// A data enum whose `List` variant holds a table and `Text` a column.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Input {
    #[store(code = 1)]
    Text { value: String },
    #[store(code = 2)]
    List {
        #[store(child)]
        items: Vec<String>,
    },
    #[store(code = 3)]
    Unset,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Evidence {
    note: String,
    #[store(child)]
    meta: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Deliverable {
    label: String,
    #[store(records)]
    evidence: Records<Evidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Attempt<A> {
    what: A,
    at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Edge<I> {
    #[store(code = 1)]
    Realise {
        #[store(flatten)]
        identity: I,
    },
    #[store(code = 2)]
    Vanish,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Ident {
    session: String,
    pid: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Plan", code = 1)]
struct Plan {
    #[store(frozen)]
    doc: Frozen<Doc>,
    #[store(frozen)]
    extra: Option<Frozen<Doc>>,
    #[store(child)]
    steps: Vec<Step>,
    #[store(child)]
    inputs: BTreeMap<Tag, Input>,
    #[store(child)]
    phases: BTreeSet<Phase>,
    #[store(records)]
    deliverables: Records<Deliverable>,
    #[store(history)]
    attempts: Records<Attempt<Phase>>,
    #[store(records)]
    edges: Records<Edge<Ident>>,
}

fn doc(name: &str) -> Frozen<Doc> {
    Frozen::new(Doc { name: name.into(), steps: [("a".to_owned(), vec!["x".to_owned()])].into() }).expect("freeze")
}

fn short(tc: &TestCase) -> String {
    tc.draw(gs::text().alphabet("ab").max_size(2))
}

fn up_to<T>(tc: &TestCase, max: usize, mut f: impl FnMut() -> T) -> Vec<T> {
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(max));
    (0..count).map(|_| f()).collect()
}

fn phase(tc: &TestCase) -> Phase {
    if tc.draw(gs::booleans()) {
        Phase::Early
    } else {
        Phase::Late
    }
}

/// Empty and non-empty collections at every depth, colliding keys (two-letter alphabet), every
/// `Input` and `Edge` variant, and equal or distinct frozen documents.
fn plan(tc: &TestCase) -> Plan {
    let docs = ["one", "two"];
    Plan {
        doc: doc(tc.draw(gs::sampled_from(docs.to_vec()))),
        extra: tc.draw(gs::booleans()).then(|| doc(tc.draw(gs::sampled_from(docs.to_vec())))),
        steps: up_to(tc, 3, || Step {
            label: short(tc),
            gates: up_to(tc, 2, || Gate {
                observed: Identity { kind: KindCode(2), name: short(tc), home: "h".into() },
                version: tc.draw(gs::integers::<u64>().max_value(i64::MAX as u64)),
            }),
            notes: up_to(tc, 2, || (short(tc), short(tc))).into_iter().collect(),
        }),
        inputs: up_to(tc, 3, || {
            let input = match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
                0 => Input::Text { value: short(tc) },
                1 => Input::List { items: up_to(tc, 2, || short(tc)) },
                _ => Input::Unset,
            };
            (Tag(short(tc)), input)
        })
        .into_iter()
        .collect(),
        phases: up_to(tc, 2, || phase(tc)).into_iter().collect(),
        deliverables: up_to(tc, 2, || Deliverable {
            label: short(tc),
            evidence: up_to(tc, 2, || Evidence { note: short(tc), meta: up_to(tc, 2, || (short(tc), short(tc))).into_iter().collect() })
                .into(),
        })
        .into(),
        attempts: up_to(tc, 2, || Attempt { what: phase(tc), at: Timestamp(tc.draw(gs::integers::<i64>())) }).into(),
        edges: up_to(tc, 2, || {
            if tc.draw(gs::booleans()) {
                Edge::Realise {
                    identity: Ident { session: short(tc), pid: tc.draw(gs::booleans()).then(|| tc.draw(gs::integers::<i64>())) },
                }
            } else {
                Edge::Vanish
            }
        })
        .into(),
    }
}

// Behaviour (DESIGN naming): tables are numbered in pre-order; a child table is
// `<parent>_<field>`, a table inside a variant `<parent>_<field>_<variant>_<subfield>`; keys are
// the ancestors' own keys renamed `<ancestor field>_<column>` and then the table's own key: `pos`
// for lists and records, `key` for maps, the element `value` for sets. A variant holding only a
// table opens no CHECK.
#[test]
fn tables_follow_the_naming_and_key_rules() {
    let set = Plan::table_set();
    set.validate().expect("valid");
    assert_eq!(Plan::TABLES + 1, set.tables.len(), "the codec cursor counts every declared table");
    let shape: Vec<(&str, TableRole, Vec<&str>, Vec<&str>)> = set
        .tables
        .iter()
        .map(|t| {
            (t.name.as_str(), t.role, t.key.iter().map(|c| c.name.as_str()).collect(), t.columns.iter().map(|c| c.name.as_str()).collect())
        })
        .collect();
    use TableRole::*;
    assert_eq!(
        shape,
        [
            ("plan", Root, vec![], vec!["doc", "extra"]),
            ("plan_steps", Child, vec!["pos"], vec!["label"]),
            ("plan_steps_gates", Child, vec!["steps_pos", "pos"], vec!["observed", "version"]),
            ("plan_steps_notes", Child, vec!["steps_pos", "key"], vec!["value"]),
            ("plan_inputs", Child, vec!["key"], vec!["tag", "text_value"]),
            ("plan_inputs_list_items", Child, vec!["inputs_key", "pos"], vec!["value"]),
            ("plan_phases", Child, vec!["value"], vec![]),
            ("plan_deliverables", Records, vec!["pos"], vec!["label"]),
            ("plan_deliverables_evidence", Records, vec!["deliverables_pos", "pos"], vec!["note"]),
            ("plan_deliverables_evidence_meta", Child, vec!["deliverables_pos", "evidence_pos", "key"], vec!["value"]),
            ("plan_attempts", History, vec!["pos"], vec!["what", "at"]),
            ("plan_edges", Records, vec!["pos"], vec!["tag", "realise_identity_session", "realise_identity_pid"]),
        ]
    );
    let parents: Vec<Option<usize>> = set.tables.iter().map(|t| t.parent).collect();
    assert_eq!(parents, [None, Some(0), Some(1), Some(1), Some(0), Some(4), Some(0), Some(0), Some(7), Some(8), Some(0), Some(0)]);
    let inputs = &set.tables[4];
    assert_eq!(inputs.guards.len(), 1, "only Text has columns");
    assert_eq!((inputs.guards[0].column.as_str(), inputs.guards[0].value), ("tag", 1));
    assert_eq!(set.tables[11].guards[0].required, ["realise_identity_session"]);
}

// Behaviour (DESIGN codec): every shape round-trips through logical rows, at every depth and
// with empty collections anywhere; a nested row's key is its parent's key then its own.
#[hegel::test]
fn tables_round_trip_through_logical_rows(tc: TestCase) {
    let value = plan(&tc);
    let rows = Encoder::encode(&value).expect("encode");
    assert_eq!(rows.tables[2].len(), value.steps.iter().map(|step| step.gates.len()).sum::<usize>());
    for row in &rows.tables[9] {
        assert_eq!(row.key.len(), 3, "deliverable pos, evidence pos, meta key");
    }
    let list_items: usize = value.inputs.values().map(|input| if let Input::List { items } = input { items.len() } else { 0 }).sum();
    assert_eq!(rows.tables[5].len(), list_items, "a variant's table has rows only while it is active");
    assert_eq!(Decoder::decode::<Plan>(rows).expect("decode"), value);
}

// Behaviour (DESIGN Frozen): a frozen field is its content hash; the bytes are recorded once per
// distinct content.
#[test]
fn frozen_fields_record_each_object_once() {
    let empty = Plan {
        doc: doc("one"),
        extra: Some(doc("one")),
        steps: vec![],
        inputs: BTreeMap::new(),
        phases: BTreeSet::new(),
        deliverables: Records::new(),
        attempts: Records::new(),
        edges: Records::new(),
    };
    let rows = Encoder::encode(&empty).expect("encode");
    assert_eq!(rows.objects.len(), 1);
    assert_eq!(rows.tables[0][0].values, [Value::Object(doc("one").hash()), Value::Object(doc("one").hash())]);
    let rows = Encoder::encode(&Plan { extra: Some(doc("two")), ..empty.clone() }).expect("encode");
    assert_eq!(rows.objects.len(), 2);
    assert_eq!(Decoder::decode::<Plan>(rows).expect("decode"), Plan { extra: Some(doc("two")), ..empty });
}

// Behaviour (#3320 decode): stored rows the value cannot have produced are refused with typed
// errors: a gap in list positions, rows of a variant table while another variant is active, a
// missing object, and object bytes that do not hash to their key.
#[test]
fn decode_refuses_rows_the_value_cannot_hold() {
    let value = Plan {
        doc: doc("one"),
        extra: None,
        steps: vec![Step { label: "s".into(), gates: vec![], notes: BTreeMap::new() }],
        inputs: [(Tag("k".into()), Input::Text { value: "v".into() })].into(),
        phases: BTreeSet::new(),
        deliverables: Records::new(),
        attempts: Records::new(),
        edges: Records::new(),
    };
    let rows = Encoder::encode(&value).expect("encode");

    let mut gap = rows.clone();
    gap.tables[1][0].key = vec![Value::Int(1)];
    assert_eq!(Decoder::decode::<Plan>(gap), Err(DecodeError::Position { column: "steps_pos".into(), expected: 0, found: 1 }));

    let mut inactive = rows.clone();
    inactive.tables[5].push(Row { key: vec![Value::Text("k".into()), Value::Int(0)], values: vec![Value::Text("i".into())] });
    assert_eq!(Decoder::decode::<Plan>(inactive), Err(DecodeError::AbsentRows { table: 5 }));

    let mut missing = rows.clone();
    missing.objects.clear();
    assert_eq!(Decoder::decode::<Plan>(missing), Err(DecodeError::MissingObject { hash: doc("one").hash() }));

    let mut tampered = rows;
    let bytes = tampered.objects.values_mut().next().expect("one object");
    let original = Hash::of(bytes);
    bytes.push(0);
    let found = Hash::of(bytes);
    assert_eq!(Decoder::decode::<Plan>(tampered), Err(DecodeError::ObjectHash { expected: original, found }));
}

// Behaviour (DESIGN elements): code enums, newtypes and generic parts are rows of their own.
#[test]
fn generic_parts_are_elements() {
    assert_eq!(<Attempt<Phase> as Element>::TABLES, 0);
    assert_eq!(<Deliverable as Element>::TABLES, 2, "evidence and its meta");
    assert_eq!(<Edge<Ident> as Element>::TABLES, 0);
}
