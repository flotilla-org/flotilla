//! Edge handles and the backend-neutral `Edge` builder (#3320 ruling 1), without a backend.

use flotilla_schema::{
    ColumnType, Columns, Edge, EdgeAppend, EdgeError, EdgeEvent, EdgeSet, Encoder, FieldColumns, Frozen, Index, Kind, Position, Records,
    RowPath, State, Stored, TableRole, Timestamp, Value,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum Phase {
    #[store(code = 1)]
    Requested,
    #[store(code = 2)]
    Realised,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
struct Ident {
    session: String,
    pid: Option<i64>,
}

/// A data enum whose `List` variant holds a table: a column-shaped field that spans a table.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Input {
    #[store(code = 1)]
    Text { value: String },
    #[store(code = 2)]
    List {
        #[store(child)]
        items: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum LifecycleEdge {
    #[store(code = 1)]
    Realise,
    #[store(code = 2)]
    Retire,
}

/// The lifecycle's edge arguments, generic over the identity a realise edge carries: the inline
/// CBOR document of each event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LifecycleArgs<I> {
    identity: Option<I>,
}

type LifecycleEvent<I> = EdgeEvent<Phase, LifecycleEdge, LifecycleArgs<I>>;

fn event<I>(edge: LifecycleEdge, to: Phase, identity: Option<I>) -> LifecycleEvent<I> {
    EdgeEvent { edge, from: Phase::Requested, to, writer: "w".into(), at: Timestamp(4), payload: LifecycleArgs { identity } }
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Note {
    #[store(intern)]
    by: String,
    #[store(records)]
    replies: Records<String>,
    #[store(state)]
    phase: State<Phase>,
}

/// A generic kind: its handles name its type parameter.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Lifecycle", code = 1)]
struct Lifecycle<I> {
    #[store(flatten)]
    realised: Option<I>,
    #[store(state)]
    phase: State<Phase>,
    input: Input,
    #[store(frozen)]
    doc: Option<Frozen<String>>,
    #[store(child)]
    labels: Vec<String>,
    #[store(records)]
    notes: Records<Note>,
    #[store(history)]
    events: Records<EdgeEvent<Phase, LifecycleEdge, LifecycleArgs<I>>>,
}

type Session = Lifecycle<Ident>;

fn ident() -> Ident {
    Ident { session: "s".into(), pid: Some(7) }
}

// Behaviour (#3320 ruling 1): the descriptor records each row's column-shaped fields as ranges of
// its table's columns, state fields marked, so an edge sets exactly a field's columns and a spec
// edit knows which columns are state. Table fields and spec collections are not fields of the row.
#[test]
fn rows_record_their_column_shaped_fields() {
    let set = Session::table_set();
    let field = |name: &str, columns, state| FieldColumns { name: name.into(), columns, state };
    assert_eq!(
        set.tables[0].fields,
        [field("realised", 0..3, false), field("phase", 3..5, true), field("input", 5..7, false), field("doc", 7..8, false)]
    );
    let columns: Vec<&str> = set.tables[0].columns.iter().map(|column| column.name.as_str()).collect();
    assert_eq!(
        columns,
        ["realised_present", "realised_session", "realised_pid", "phase", "phase_since", "input", "input_text_value", "doc"]
    );
    let notes = set.tables.iter().find(|table| table.name == "lifecycle_notes").expect("notes");
    assert_eq!(notes.fields, [field("by", 1..2, false), field("phase", 2..4, true)], "after the version stamp");
    let labels = set.tables.iter().find(|table| table.name == "lifecycle_labels").expect("labels");
    assert!(labels.fields.is_empty(), "an element of a primitive has no named fields");
}

// Behaviour (#3320 ruling of 2026-10-15): an edge log is one row per event, a fixed header of
// columns (edge code, from- and to-label, interned writer, time) after the version stamp and one
// inline CBOR document of the arguments, never expanded into columns or tables. Its table is
// indexed for events of one edge in time order and for time ranges; the primary key (owner, pos)
// gives one resource's log in order.
#[test]
fn edge_logs_store_a_header_and_one_document_per_event() {
    let set = Session::table_set();
    let events = set.tables.iter().find(|table| table.name == "lifecycle_events").expect("events");
    let columns: Vec<(&str, &ColumnType, bool)> =
        events.columns.iter().map(|column| (column.name.as_str(), &column.ty, column.nullable)).collect();
    assert_eq!(
        columns,
        [
            ("version", &ColumnType::Integer, false),
            ("edge", &ColumnType::Code(vec![1, 2]), false),
            ("from", &ColumnType::Code(vec![1, 2]), false),
            ("to", &ColumnType::Code(vec![1, 2]), false),
            ("writer", &ColumnType::Interned, false),
            ("at", &ColumnType::Timestamp, false),
            ("payload", &ColumnType::Cbor, false),
        ]
    );
    assert!(!set.tables.iter().any(|table| table.parent == set.tables.iter().position(|table| table.name == "lifecycle_events")));
    let index = |name: &str, eq: &[&str], order: &[&str]| Index {
        name: name.into(),
        table: "lifecycle_events".into(),
        eq: eq.iter().map(|column| column.to_string()).collect(),
        order: order.iter().map(|column| column.to_string()).collect(),
        predicate: Vec::new(),
    };
    assert_eq!(set.indexes, [index("by_edge", &["edge"], &["at"]), index("by_time", &[], &["at"])]);
    assert_eq!(set.validate(), Ok(()));
}

// Behaviour (#3320 ruling 1, and #3319's state value): every edge writes the kind's state value and
// appends its event to the edge log under the root (a placeholder stamp first, then the header and
// the document), each set field's values encoded by the field's own adapter, appends and sets in
// records rows addressed by position, and every object its values name. Handles exist on generic
// kinds and parts and are named by the field in upper case.
#[test]
fn edges_encode_exactly_their_declared_fields_and_rows() {
    let realised = State::new(Phase::Realised, Timestamp(5));
    let doc = Frozen::new("plan".to_owned()).expect("freeze");
    let realise = event(LifecycleEdge::Realise, Phase::Realised, Some(ident()));
    let edge = Edge::new(Session::PHASE, &realised, Session::EVENTS, &realise)
        .and_then(|edge| edge.set(Session::REALISED, &Some(ident())))
        .and_then(|edge| edge.set(Session::DOC, &Some(doc.clone())))
        .and_then(|edge| edge.set_in(Session::NOTES, Position(2), Note::PHASE, &realised))
        .and_then(|edge| edge.append_in(Session::NOTES, Position(2), Note::REPLIES, &"ok".to_owned()))
        .expect("edge");
    let parts = edge.into_parts();
    let int = Value::Int;
    assert_eq!(
        parts.sets,
        [
            EdgeSet { row: RowPath::Root, field: "phase", values: vec![int(2), int(5)] },
            EdgeSet { row: RowPath::Root, field: "realised", values: vec![int(1), Value::Text("s".into()), int(7)] },
            EdgeSet { row: RowPath::Root, field: "doc", values: vec![Value::Object(doc.hash())] },
            EdgeSet { row: RowPath::Record { table: "notes", position: Position(2) }, field: "phase", values: vec![int(2), int(5)] },
        ]
    );
    let appends: Vec<(RowPath, &str, TableRole, &Vec<Value>)> =
        parts.appends.iter().map(|EdgeAppend { under, table, role, rows }| (*under, *table, *role, &rows.tables[0][0].values)).collect();
    assert_eq!(
        appends,
        [
            (
                RowPath::Root,
                "events",
                TableRole::History,
                &vec![
                    int(0),
                    int(1),
                    int(1),
                    int(2),
                    Value::Str("w".into()),
                    int(4),
                    Value::Blob(flotilla_cbor::to_vec(&realise.payload, &flotilla_schema::INLINE_LIMITS).expect("cbor")),
                ]
            ),
            (
                RowPath::Record { table: "notes", position: Position(2) },
                "replies",
                TableRole::Records,
                &vec![int(0), Value::Text("ok".into())]
            ),
        ]
    );
    assert_eq!(parts.objects.keys().copied().collect::<Vec<_>>(), [doc.hash()], "an edge carries the objects it sets");
    let values = Encoder::columns(|e| realised.encode(e)).expect("columns").tables[0][0].values.clone();
    assert_eq!(values, parts.sets[0].values, "a set field encodes as its columns do in the row");
}

// Behaviour (#3320 ruling 1): a column-shaped field whose value spans a nested table (a data enum
// with a `child` variant) changes only by a spec edit, and one edge sets a field of a row once;
// both are refused when the edge is built. An event must enter the label of the state the edge
// writes (#3319: the state value is what the system acts on; the log only records it).
#[test]
fn edges_refuse_table_fields_repeated_fields_and_a_mislabelled_event() {
    let phase = State::new(Phase::Requested, Timestamp(2));
    let retire = event::<Ident>(LifecycleEdge::Retire, Phase::Requested, None);
    let edge = || Edge::new(Session::PHASE, &phase, Session::EVENTS, &retire).expect("event");
    assert_eq!(edge().set(Session::INPUT, &Input::Text { value: "t".into() }), Err(EdgeError::Tables { field: "input".into() }));
    let twice = edge().set(Session::REALISED, &None).and_then(|edge| edge.set(Session::REALISED, &None));
    assert_eq!(twice, Err(EdgeError::Repeated { field: "realised".into() }));
    let other_rows = edge()
        .set_in(Session::NOTES, Position(0), Note::PHASE, &phase)
        .and_then(|edge| edge.set_in(Session::NOTES, Position(1), Note::PHASE, &phase));
    assert!(other_rows.is_ok(), "the same field of different rows is not repeated");
    let realised = State::new(Phase::Realised, Timestamp(2));
    assert_eq!(
        Edge::new(Session::PHASE, &realised, Session::EVENTS, &retire),
        Err(EdgeError::Label { field: "phase".into(), to: 1, state: 2 })
    );
}
