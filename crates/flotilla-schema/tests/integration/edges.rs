//! Edge handles and the backend-neutral `Edge` builder (#3320 ruling 1), without a backend.

use flotilla_schema::{
    Code, Columns, Edge, EdgeAppend, EdgeError, EdgeEvent, EdgeHeader, EdgeSet, Encoder, FieldColumns, Frozen, Identity, Kind, KindCode,
    Position, Records, RowPath, State, Stored, TableRole, Timestamp, Value,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum Phase {
    #[store(code = "c1")]
    Requested,
    #[store(code = "c2")]
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
    #[store(code = "c1")]
    Text { value: String },
    #[store(code = "c2")]
    List {
        #[store(child)]
        items: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum LifecycleEdge {
    #[store(code = "c1")]
    Realise,
    #[store(code = "c2")]
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
    EdgeEvent { edge, from: Phase::Requested, to, at: Timestamp(4), arguments: LifecycleArgs { identity } }
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Note {
    #[store(intern = "wr")]
    by: String,
    #[store(records)]
    replies: Records<String>,
    #[store(state)]
    phase: State<Phase>,
}

/// A generic kind: its handles name its type parameter.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Lifecycle", code = "k1")]
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
    receipts: Records<String>,
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
    let field = |name: &str, columns, state| FieldColumns { name: name.into(), columns, state, datum_of: None, aliases: Vec::new() };
    assert_eq!(
        set.tables[0].fields,
        [field("realised", 0..2, false), field("phase", 2..4, true), field("input", 4..6, false), field("doc", 6..7, false)]
    );
    let columns: Vec<&str> = set.tables[0].columns.iter().map(|column| column.name.as_str()).collect();
    assert_eq!(columns, ["realised_session", "realised_pid", "phase", "phase_since", "input", "input_value", "doc"]);
    let notes = set.tables.iter().find(|table| table.name == "lifecycle_notes").expect("notes");
    assert_eq!(notes.fields, [field("by", 1..2, false), field("phase", 2..4, true)], "after the version stamp");
    let labels = set.tables.iter().find(|table| table.name == "lifecycle_labels").expect("labels");
    assert!(labels.fields.is_empty(), "an element of a primitive has no named fields");
}

// Behaviour (#3392 G8): an edge's event is no row of the kind: it is the header of the write's
// log entry (edge code, from- and to-label, writer, time, master) and one inline CBOR document of
// the arguments, so a kind declares no edge-log table.
#[test]
fn edge_events_are_the_log_entry_header() {
    let set = Session::table_set();
    let tables: Vec<(&str, TableRole)> = set.tables.iter().map(|table| (table.name.as_str(), table.role)).collect();
    assert_eq!(
        tables,
        [
            ("lifecycle", TableRole::Root),
            ("lifecycle_input_items", TableRole::Child),
            ("lifecycle_labels", TableRole::Child),
            ("lifecycle_notes", TableRole::Records),
            ("lifecycle_notes_replies", TableRole::Records),
            ("lifecycle_receipts", TableRole::History),
        ]
    );
    let realise = event(LifecycleEdge::Realise, Phase::Realised, Some(ident()));
    let about = Identity { kind: KindCode::new("k4"), name: "m".into(), home: "h".into() };
    let edge = Edge::new(Session::PHASE, &State::new(Phase::Realised, Timestamp(4)), &realise).expect("edge").about(about.clone());
    let header = edge.into_parts().event;
    assert_eq!(
        header,
        EdgeHeader {
            edge: Code::new("c1"),
            from: Code::new("c1"),
            to: Code::new("c2"),
            at: Timestamp(4),
            arguments: flotilla_cbor::to_vec(&realise.arguments, &flotilla_schema::INLINE_LIMITS).expect("cbor"),
            about: Some(about),
        }
    );
    assert_eq!(header.arguments::<LifecycleArgs<Ident>>(), Ok(realise.arguments));
    assert_eq!(set.validate(), Ok(()));
}

// Behaviour (#3320 ruling 1, and #3319's state value): every edge writes the kind's state value,
// each set field's values encoded by the field's own adapter, appends and sets in
// records rows addressed by position, and every object its values name. Handles exist on generic
// kinds and parts and are named by the field in upper case.
#[test]
fn edges_encode_exactly_their_declared_fields_and_rows() {
    let realised = State::new(Phase::Realised, Timestamp(5));
    let doc = Frozen::new("plan".to_owned()).expect("freeze");
    let realise = event(LifecycleEdge::Realise, Phase::Realised, Some(ident()));
    let edge = Edge::new(Session::PHASE, &realised, &realise)
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
            EdgeSet { row: RowPath::Root, field: "phase", values: vec![Value::Text("c2".into()), int(5)] },
            EdgeSet { row: RowPath::Root, field: "realised", values: vec![Value::Text("s".into()), int(7)] },
            EdgeSet { row: RowPath::Root, field: "doc", values: vec![Value::Object(doc.hash())] },
            EdgeSet {
                row: RowPath::Record { table: "notes", position: Position(2) },
                field: "phase",
                values: vec![Value::Text("c2".into()), int(5)]
            },
        ]
    );
    let appends: Vec<(RowPath, &str, TableRole, &Vec<Value>)> =
        parts.appends.iter().map(|EdgeAppend { under, table, role, rows }| (*under, *table, *role, &rows.tables[0][0].values)).collect();
    assert_eq!(
        appends,
        [(
            RowPath::Record { table: "notes", position: Position(2) },
            "replies",
            TableRole::Records,
            &vec![int(0), Value::Text("ok".into())]
        ),]
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
    let edge = || Edge::new(Session::PHASE, &phase, &retire).expect("event");
    assert_eq!(edge().set(Session::INPUT, &Input::Text { value: "t".into() }), Err(EdgeError::Tables { field: "input".into() }));
    let twice = edge().set(Session::REALISED, &None).and_then(|edge| edge.set(Session::REALISED, &None));
    assert_eq!(twice, Err(EdgeError::Repeated { field: "realised".into() }));
    let other_rows = edge()
        .set_in(Session::NOTES, Position(0), Note::PHASE, &phase)
        .and_then(|edge| edge.set_in(Session::NOTES, Position(1), Note::PHASE, &phase));
    assert!(other_rows.is_ok(), "the same field of different rows is not repeated");
    let realised = State::new(Phase::Realised, Timestamp(2));
    assert_eq!(
        Edge::new(Session::PHASE, &realised, &retire),
        Err(EdgeError::Label { field: "phase".into(), to: Code::new("c1"), state: Code::new("c2") })
    );
}
