//! Label-plus-data states (#3373 rulings 1 to 3), without a backend: a synthetic `Ticket` kind
//! whose state is `#[store(labels = TicketLabel)]`. It is clearly synthetic: no engine kind is a
//! ticket; it exercises every datum form (plain, `child` set and map, `intern`, optional and
//! required) and a label that carries no data.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{
    render_ddl, ColumnType, Condition, DecodeError, Decoder, Edge, EdgeError, EdgeEvent, EdgeParts, EdgeRow, EdgeSet, Encoder,
    FieldColumns, Kind, LabelCheck, Labelled, RowEdit, RowPath, Rows, State, StateData, Stored, TableRole, Timestamp, Value,
};
use hegel::{generators as gs, TestCase};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum TicketLabel {
    #[store(code = 1)]
    Draft,
    #[store(code = 2)]
    Open,
    #[store(code = 3)]
    Closed,
}

/// A hold on an open ticket: an optional flattened datum, behind its `hold_present` flag.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Hold {
    by: String,
    until: Timestamp,
}

/// Who closed a ticket, and when: a required flattened datum.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Signature {
    #[store(intern)]
    who: String,
    at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(labels = TicketLabel)]
enum TicketState {
    #[store(code = 1)]
    Draft,
    #[store(code = 2)]
    Open {
        #[store(child)]
        assignees: BTreeSet<String>,
        #[store(child)]
        notes: BTreeMap<String, String>,
        priority: u8,
        #[store(intern)]
        owner: Option<String>,
        #[store(flatten)]
        hold: Option<Hold>,
    },
    #[store(code = 3)]
    Closed {
        #[store(child)]
        assignees: BTreeSet<String>,
        #[store(child)]
        notes: BTreeMap<String, String>,
        priority: u8,
        reason: String,
        #[store(intern)]
        closed_by: String,
        #[store(flatten)]
        signature: Signature,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum TicketEdge {
    #[store(code = 1)]
    Edit,
    #[store(code = 2)]
    Move,
}

type TicketEvent = EdgeEvent<TicketLabel, TicketEdge, String>;

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Ticket", code = 8, index(name = "live_by_queue", eq(queue), when(state in [Open, Closed])))]
struct Ticket {
    #[store(intern)]
    queue: String,
    title: String,
    #[store(state)]
    state: State<TicketState>,
}

/// Keys and values from tiny alphabets, so two drawn collections share keys and values often: a
/// transition between them inserts, updates, deletes and keeps rows.
fn assignees(tc: &TestCase) -> BTreeSet<String> {
    tc.draw(gs::vecs(gs::text().alphabet("ab").max_size(2)).max_size(4)).into_iter().collect()
}

fn notes(tc: &TestCase) -> BTreeMap<String, String> {
    let keys = tc.draw(gs::vecs(gs::text().alphabet("ab").max_size(2)).max_size(4));
    keys.into_iter().map(|key| (key, tc.draw(gs::text().alphabet("xy").max_size(1)))).collect()
}

fn small(tc: &TestCase, values: &[&str]) -> String {
    tc.draw(gs::sampled_from(values.iter().map(|value| (*value).to_owned()).collect::<Vec<_>>()))
}

/// Every label, with data drawn so that two draws often agree on a datum: priorities and names
/// from two values each, an optional owner present or not.
fn state_value(tc: &TestCase) -> TicketState {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
        0 => TicketState::Draft,
        1 => TicketState::Open {
            assignees: assignees(tc),
            notes: notes(tc),
            priority: tc.draw(gs::integers::<u8>().min_value(0).max_value(1)),
            owner: tc.draw(gs::booleans()).then(|| small(tc, &["ann", "bo"])),
            hold: tc.draw(gs::booleans()).then(|| Hold {
                by: small(tc, &["ann", "bo"]),
                until: Timestamp(tc.draw(gs::integers::<i64>().min_value(0).max_value(1))),
            }),
        },
        _ => TicketState::Closed {
            assignees: assignees(tc),
            notes: notes(tc),
            priority: tc.draw(gs::integers::<u8>().min_value(0).max_value(1)),
            reason: small(tc, &["done", "dup"]),
            closed_by: small(tc, &["ann", "bo"]),
            signature: Signature {
                who: small(tc, &["ann", "bo"]),
                at: Timestamp(tc.draw(gs::integers::<i64>().min_value(0).max_value(1))),
            },
        },
    }
}

fn ticket(tc: &TestCase) -> Ticket {
    Ticket {
        queue: small(tc, &["q", "r"]),
        title: tc.draw(gs::text().max_size(3)),
        state: State::new(state_value(tc), Timestamp(tc.draw(gs::integers::<i64>()))),
    }
}

fn event(from: TicketLabel, to: TicketLabel, at: i64) -> TicketEvent {
    let edge = if from == to { TicketEdge::Edit } else { TicketEdge::Move };
    EdgeEvent { edge, from, to, writer: "w".into(), at: Timestamp(at), payload: String::new() }
}

/// The edge from `previous` to `next` at time `at`, with `next`'s since as the edge requires.
fn transition(previous: &State<TicketState>, next: TicketState, at: i64) -> (Edge<Ticket>, State<TicketState>) {
    let (from, to) = (*previous.value.label(), *next.label());
    let since = if from == to { previous.since } else { Timestamp(at) };
    let next = State::new(next, since);
    let edge = Edge::transition(Ticket::STATE, previous, &next, &event(from, to, at)).expect("edge");
    (edge, next)
}

fn column(name: &str, ty: ColumnType, nullable: bool) -> (String, ColumnType, bool) {
    (name.to_owned(), ty, nullable)
}

// Behaviour (#3373 rulings 1 and 3): a label-plus-data state is one label column `<field>` and
// `<field>_since`, then one column group per column datum and one `state` table per collection
// datum, each declared once and named by the datum whatever labels carry it. Datum columns are
// nullable, and one CHECK per label requires the label's own required data and nulls the data it
// lacks (Draft carries none; only Closed requires a reason). A flattened datum is its struct's
// columns `<datum>_<field>`, behind a `<datum>_present` flag when optional. The label field and each column datum
// are state fields of the row, the datums marked with their label field; a map datum's table is
// keyed by owner and map key, a set datum's by owner and value. An index predicate on the state
// takes its codes from the labels enum.
#[test]
fn a_label_state_is_one_label_column_and_shared_data() {
    let set = Ticket::table_set();
    set.validate().expect("valid descriptor");
    let root = &set.tables[0];
    let columns: Vec<_> = root.columns.iter().map(|column| (column.name.clone(), column.ty.clone(), column.nullable)).collect();
    assert_eq!(
        columns,
        [
            column("queue", ColumnType::Interned, false),
            column("title", ColumnType::Text, false),
            column("state", ColumnType::Code(vec![1, 2, 3]), false),
            column("state_since", ColumnType::Timestamp, false),
            column("priority", ColumnType::Integer, true),
            column("owner", ColumnType::Interned, true),
            column("hold_present", ColumnType::Bool, true),
            column("hold_by", ColumnType::Text, true),
            column("hold_until", ColumnType::Timestamp, true),
            column("reason", ColumnType::Text, true),
            column("closed_by", ColumnType::Interned, true),
            column("signature_who", ColumnType::Interned, true),
            column("signature_at", ColumnType::Timestamp, true),
        ]
    );
    let field = |name: &str, columns, state, datum_of: Option<&str>| FieldColumns {
        name: name.into(),
        columns,
        state,
        datum_of: datum_of.map(str::to_owned),
        aliases: Vec::new(),
    };
    assert_eq!(
        root.fields,
        [
            field("queue", 0..1, false, None),
            field("title", 1..2, false, None),
            field("state", 2..4, true, None),
            field("priority", 4..5, true, Some("state")),
            field("owner", 5..6, true, Some("state")),
            field("hold", 6..9, true, Some("state")),
            field("reason", 9..10, true, Some("state")),
            field("closed_by", 10..11, true, Some("state")),
            field("signature", 11..13, true, Some("state")),
        ]
    );
    let checks: Vec<String> = root.guards.iter().map(|guard| guard.check()).collect();
    assert_eq!(
        checks,
        [
            "CASE WHEN \"hold_present\" = 1 THEN \"hold_by\" IS NOT NULL AND \"hold_until\" IS NOT NULL \
             ELSE \"hold_by\" IS NULL AND \"hold_until\" IS NULL END",
            "CASE WHEN \"state\" = 1 THEN \"priority\" IS NULL AND \"owner\" IS NULL AND \"hold_present\" IS NULL AND \"reason\" IS NULL \
             AND \"closed_by\" IS NULL AND \"signature_who\" IS NULL AND \"signature_at\" IS NULL END",
            "CASE WHEN \"state\" = 2 THEN \"priority\" IS NOT NULL AND \"hold_present\" IS NOT NULL AND \"reason\" IS NULL \
             AND \"closed_by\" IS NULL AND \"signature_who\" IS NULL AND \"signature_at\" IS NULL END",
            "CASE WHEN \"state\" = 3 THEN \"priority\" IS NOT NULL AND \"reason\" IS NOT NULL AND \"closed_by\" IS NOT NULL \
             AND \"signature_who\" IS NOT NULL AND \"signature_at\" IS NOT NULL AND \"owner\" IS NULL AND \"hold_present\" IS NULL END",
        ]
    );
    // A flattened optional datum keeps its presence guard: its columns are null unless it is
    // present, and its flag is null under a label lacking it.
    assert!(!root.guards[0].is_label() && root.guards[1..].iter().all(|guard| guard.is_label()));
    let tables: Vec<(&str, TableRole, Vec<&str>, Vec<&str>)> = set.tables[1..]
        .iter()
        .map(|table| {
            let key = table.key.iter().map(|column| column.name.as_str()).collect();
            let columns = table.columns.iter().map(|column| column.name.as_str()).collect();
            (table.name.as_str(), table.role, key, columns)
        })
        .collect();
    assert_eq!(
        tables,
        [("ticket_assignees", TableRole::State, vec!["value"], vec![]), ("ticket_notes", TableRole::State, vec!["key"], vec!["value"]),]
    );
    let index = set.indexes.iter().find(|index| index.name == "live_by_queue").expect("declared");
    assert_eq!(index.predicate, [Condition::In { column: "state".into(), codes: vec![2, 3] }]);
    let ddl = render_ddl(&set);
    assert!(
        ddl.contains(
            "CREATE TABLE \"ticket_notes\" (\n  \"owner\" INTEGER NOT NULL,\n  \"key\" TEXT NOT NULL,\n  \"value\" TEXT NOT NULL,"
        ),
        "{ddl}"
    );
    assert!(ddl.contains("CHECK (CASE WHEN \"state\" = 2\n      THEN \"priority\" IS NOT NULL\n      AND \"hold_present\" IS NOT NULL\n      AND \"reason\" IS NULL"), "{ddl}");
    assert_eq!(<TicketState as StateData>::DATA, ["assignees", "notes", "priority", "owner", "hold", "reason", "closed_by", "signature"]);
}

// Behaviour (#3373 ruling 1): every value of a label-plus-data state round-trips through its rows:
// every label, collections empty or not, optional data present or not. Under each label the data
// it lacks is null and its tables hold no rows.
#[hegel::test]
fn label_states_round_trip(tc: TestCase) {
    let value = ticket(&tc);
    let rows = Encoder::encode(&value).expect("encode");
    let root = &rows.tables[0][0].values;
    let (assignees, notes) = (&rows.tables[1], &rows.tables[2]);
    match &value.state.value {
        TicketState::Draft => {
            assert!(root[4..].iter().all(|value| *value == Value::Null), "Draft carries no data: {root:?}");
            assert!(assignees.is_empty() && notes.is_empty());
        }
        TicketState::Open { assignees: set, notes: map, .. } => {
            assert!(root[9..].iter().all(|value| *value == Value::Null), "Open lacks a reason and a signature");
            assert_ne!(root[6], Value::Null, "an Open ticket's hold flag is 0 or 1");
            assert_eq!((assignees.len(), notes.len()), (set.len(), map.len()));
        }
        TicketState::Closed { .. } => assert!(root[5..9].iter().all(|value| *value == Value::Null), "Closed lacks an owner and a hold"),
    }
    assert_eq!(Decoder::decode::<Ticket>(rows).expect("decode"), value);
}

fn rows_of(state: TicketState) -> Rows {
    Encoder::encode(&Ticket { queue: "q".into(), title: "t".into(), state: State::new(state, Timestamp(1)) }).expect("encode")
}

// Behaviour (#3373 ruling 1): rows of a datum's table exist only while the label carries the datum,
// and a datum column is null under a label lacking it; decoding refuses either otherwise, and a
// label's required datum stored as null is missing.
#[test]
fn decoding_refuses_data_a_label_lacks() {
    let mut orphan = rows_of(TicketState::Draft);
    orphan.tables[1].push(flotilla_schema::Row { key: vec![Value::Text("a".into())], values: vec![] });
    assert_eq!(Decoder::decode::<Ticket>(orphan), Err(DecodeError::AbsentRows { table: 1 }));

    let mut stray = rows_of(TicketState::Draft);
    stray.tables[0][0].values[9] = Value::Text("why".into());
    assert!(matches!(Decoder::decode::<Ticket>(stray), Err(DecodeError::Type { expected: "null", .. })));

    let closed = TicketState::Closed {
        assignees: BTreeSet::new(),
        notes: BTreeMap::new(),
        priority: 0,
        reason: "done".into(),
        closed_by: "ann".into(),
        signature: Signature { who: "ann".into(), at: Timestamp(2) },
    };
    let mut missing = rows_of(closed.clone());
    missing.tables[0][0].values[9] = Value::Null;
    assert!(matches!(Decoder::decode::<Ticket>(missing), Err(DecodeError::Missing { column }) if column.ends_with("reason")));
    let mut unsigned = rows_of(closed);
    unsigned.tables[0][0].values[11] = Value::Null;
    assert!(Decoder::decode::<Ticket>(unsigned).is_err(), "a required flattened datum's columns are required");
    let mut stray_hold = rows_of(TicketState::Draft);
    stray_hold.tables[0][0].values[8] = Value::Int(3);
    assert!(matches!(Decoder::decode::<Ticket>(stray_hold), Err(DecodeError::Type { expected: "null", .. })), "a hold under Draft");
}

fn open(assignees: &[&str], notes: &[(&str, &str)], priority: u8, owner: Option<&str>) -> TicketState {
    TicketState::Open {
        assignees: assignees.iter().map(|value| (*value).to_owned()).collect(),
        notes: notes.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect(),
        priority,
        owner: owner.map(str::to_owned),
        hold: None,
    }
}

// Behaviour (#3373 rulings 2 and 3, #3392 A3): a transition writes the state as one unit, its
// label, `since` (the event's `at` on a label change, the previous `since` otherwise) and every
// column datum (null when the next label lacks it), and each collection datum row by row: a delete
// per removed key, an update per changed value and an insert per new key, never the unchanged
// rows. It appends no row for the event (the event is its log entry's header) and names the label
// it leaves, for the store to check.
#[test]
fn transitions_write_exactly_the_data_that_differs() {
    let previous = State::new(open(&["x", "y"], &[("k", "1"), ("m", "2")], 1, Some("ann")), Timestamp(5));
    let closed = TicketState::Closed {
        assignees: ["y", "z"].map(str::to_owned).into(),
        notes: [("k".to_owned(), "3".to_owned())].into(),
        priority: 1,
        reason: "done".into(),
        closed_by: "bo".into(),
        signature: Signature { who: "bo".into(), at: Timestamp(9) },
    };
    let (edge, next) = transition(&previous, closed, 9);
    assert_eq!(next.since, Timestamp(9));
    let EdgeParts { sets, rows, appends, from, .. } = edge.into_parts();
    let set = |field, values| EdgeSet { row: RowPath::Root, field, values };
    assert_eq!(
        sets,
        [
            set("state", vec![Value::Int(3), Value::Int(9)]),
            set("priority", vec![Value::Int(1)]),
            set("owner", vec![Value::Null]),
            set("hold", vec![Value::Null, Value::Null, Value::Null]),
            set("reason", vec![Value::Text("done".into())]),
            set("closed_by", vec![Value::Str("bo".into())]),
            set("signature", vec![Value::Str("bo".into()), Value::Int(9)]),
        ],
        "an unchanged priority too: the state is one unit"
    );
    let text = |text: &str| Value::Text(text.into());
    let row = |table, edit| EdgeRow { under: RowPath::Root, table, edit };
    assert_eq!(
        rows,
        [
            row("assignees", RowEdit::Delete { key: vec![text("x")] }),
            row("assignees", RowEdit::Insert { key: vec![text("z")], values: vec![] }),
            row("notes", RowEdit::Delete { key: vec![text("m")] }),
            row("notes", RowEdit::Update { key: vec![text("k")], values: vec![text("3")] }),
        ]
    );
    assert_eq!(appends.len(), 0, "the event is the log entry's header, not a row");
    assert_eq!(from, LabelCheck { field: "state", code: 2 });

    // Data only: the label and since stay, one note is added.
    let (edge, next) = transition(&previous, open(&["x", "y"], &[("k", "1"), ("m", "2"), ("n", "4")], 1, Some("ann")), 11);
    assert_eq!(next.since, Timestamp(5), "since is the time the label was entered");
    let parts = edge.into_parts();
    let fields: Vec<&str> = parts.sets.iter().map(|set| set.field).collect();
    let every = ["state", "priority", "owner", "hold", "reason", "closed_by", "signature"];
    assert_eq!(fields, every, "the whole state, its label unchanged, the data its label lacks null");
    assert_eq!(parts.sets[0], set("state", vec![Value::Int(2), Value::Int(5)]));
    assert_eq!(parts.rows, [row("notes", RowEdit::Insert { key: vec![text("n")], values: vec![text("4")] })]);

    // Into a label carrying nothing: every datum is nulled and every row deleted.
    let parts = transition(&previous, TicketState::Draft, 12).0.into_parts();
    let fields: Vec<&str> = parts.sets.iter().map(|set| set.field).collect();
    assert_eq!(fields, every, "the hold's flag goes from 0 to null");
    assert_eq!(parts.rows.len(), 4);
    assert!(parts.rows.iter().all(|row| matches!(row.edit, RowEdit::Delete { .. })));
}

// Behaviour (#3373 rulings 2, 3 and 5): a transition is refused when its event does not leave the
// previous label or enter the next one, and when the next `since` is not the label's entry time
// (the event's `at` on a label change, the previous `since` otherwise). `Edge::new`, which writes a
// state whole, refuses a state carrying data: it needs the previous value.
#[test]
fn transitions_refuse_inconsistent_events_and_since() {
    let previous = State::new(open(&[], &[], 0, None), Timestamp(5));
    let draft = State::new(TicketState::Draft, Timestamp(9));
    let attempt =
        |next: &State<TicketState>, from, to, at| Edge::transition(Ticket::STATE, &previous, next, &event(from, to, at)).map(|_| ());
    use TicketLabel::*;
    assert_eq!(attempt(&draft, Draft, Draft, 9), Err(EdgeError::From { field: "state".into(), from: 1, state: 2 }));
    assert_eq!(attempt(&draft, Open, Closed, 9), Err(EdgeError::Label { field: "state".into(), to: 3, state: 1 }));
    assert_eq!(
        attempt(&draft, Open, Draft, 8),
        Err(EdgeError::Since { field: "state".into(), expected: Timestamp(8), found: Timestamp(9) })
    );
    let moved_since = State::new(open(&["a"], &[], 0, None), Timestamp(7));
    assert_eq!(
        attempt(&moved_since, Open, Open, 7),
        Err(EdgeError::Since { field: "state".into(), expected: Timestamp(5), found: Timestamp(7) })
    );
    assert_eq!(attempt(&draft, Open, Draft, 9), Ok(()));
    assert_eq!(Edge::new(Ticket::STATE, &draft, &event(Open, Draft, 9)), Err(EdgeError::Data { field: "state".into() }));
}

/// `rows` with an edge's sets and row edits applied, as a backend would: each set replaces its
/// field's columns of the root row, each edit inserts, updates or deletes one row by key.
fn applied(mut rows: Rows, parts: &EdgeParts) -> Rows {
    let set = Ticket::table_set();
    for EdgeSet { field, values, .. } in &parts.sets {
        let columns = set.tables[0].fields.iter().find(|candidate| candidate.name == *field).expect("a field").columns.clone();
        rows.tables[0][0].values.splice(columns, values.iter().cloned());
    }
    for EdgeRow { table, edit, .. } in &parts.rows {
        let index = set.tables.iter().position(|candidate| candidate.field == *table && candidate.role == TableRole::State).expect("table");
        let table = &mut rows.tables[index];
        match edit {
            RowEdit::Insert { key, values } => {
                assert!(!table.iter().any(|row| &row.key == key), "an insert adds a new key");
                table.push(flotilla_schema::Row { key: key.clone(), values: values.clone() });
            }
            RowEdit::Update { key, values } => {
                let row = table.iter_mut().find(|row| &row.key == key).expect("an update changes an existing row");
                assert_ne!(&row.values, values, "an update changes the row");
                row.values = values.clone();
            }
            RowEdit::Delete { key } => {
                let before = table.len();
                table.retain(|row| &row.key != key);
                assert_eq!(table.len(), before - 1, "a delete removes an existing row");
            }
        }
    }
    rows.sort();
    rows
}

// Behaviour (#3373 ruling 2, #3392 A3, completeness without a backend): for any two states (labels
// equal or not, collections growing, shrinking and changing), applying a transition's sets and row
// edits to the previous value's rows gives the next value's rows; every set field holds the next
// value's columns, no field is set twice, and the label field and every column datum are set.
// Since moves only on a label change.
#[hegel::test]
fn transitions_apply_to_exactly_the_next_rows(tc: TestCase) {
    let before = ticket(&tc);
    let at = tc.draw(gs::integers::<i64>());
    let (edge, state) = transition(&before.state, state_value(&tc), at);
    let parts = edge.into_parts();
    let next = Ticket { state: state.clone(), ..before.clone() };
    let mut old = Encoder::encode(&before).expect("encode");
    old.sort();
    let mut new = Encoder::encode(&next).expect("encode");
    new.sort();
    let set = Ticket::table_set();
    for EdgeSet { field, values, .. } in &parts.sets {
        let columns = set.tables[0].fields.iter().find(|candidate| candidate.name == *field).expect("a field").columns.clone();
        assert_eq!(&new.tables[0][0].values[columns], &values[..]);
    }
    let fields: BTreeSet<&str> = parts.sets.iter().map(|set| set.field).collect();
    assert_eq!(fields.len(), parts.sets.len(), "each field once");
    let moved = before.state.value.label() != state.value.label();
    let state_fields: BTreeSet<&str> = set.tables[0].fields.iter().filter(|field| field.state).map(|field| field.name.as_str()).collect();
    assert_eq!(fields, state_fields, "the state is written as one unit");
    assert_eq!(state.since, if moved { Timestamp(at) } else { before.state.since });
    assert_eq!(applied(old, &parts), new);
}

// Behaviour (CLAUDE.md schema review, #3373 ruling 1): the review view shows a label-plus-data
// state as state data, not enum expansion: the root's label guards add no enum-expansion count,
// its data tables have the `state data` role, and a closing section names the label, its data
// columns and its data tables.
#[test]
fn the_view_shows_label_states_as_state_data() {
    let set = Ticket::table_set();
    assert_eq!(flotilla_schema::enum_columns(&set.tables[0]), []);
    let view = flotilla_schema::render_view([&set]);
    assert!(view.contains("| `ticket` | root | 15 | – | `ticket_assignees` (state data), `ticket_notes` (state data) |"), "{view}");
    assert!(view.contains("| `ticket_notes` | state data | 3 | – | – | – |"), "{view}");
    assert!(view.ends_with(
        "States with data (one label column, then data shared by name across the labels):\n\
         - `ticket`.`state`: label `state` and `state_since`; data columns `priority`, `owner`, `hold_present`, `hold_by`, `hold_until`, `reason`, `closed_by`, \
         `signature_who`, `signature_at`; \
         data tables `ticket_assignees`, `ticket_notes`\n"
    ));
}
