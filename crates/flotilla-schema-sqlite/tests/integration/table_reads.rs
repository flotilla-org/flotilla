//! Table reads (#4214): the rows wire carries each enum column's readable name, never its stored
//! code, on every table of every fixture kind and on the kind's log. Codes stay in storage and in
//! the read's SQL; filters compare names.

use std::collections::BTreeMap;

use flotilla_schema::{ColumnType, NewChange, Table, Value};
use flotilla_schema_sqlite::Store;
use hegel::TestCase;
use rigging_condition::{CheckError, Checked, Condition, Field, Literal, Operand, Relop};

use crate::fixtures::{
    origin, store, AgentSession, AwaitingReply, Convoy, Dossier, Ensure, Fixture, Message, Person, Pick, Referral, Resource,
    TerminalSession, Ticket, Vessel, LOCAL,
};

type Rows = Vec<BTreeMap<String, Value>>;

/// Every covering value inserted, then moved by one edge where one applies: the tables hold every
/// variant, and the log holds creates and edges between labels.
fn filled<K: Fixture>(tc: &TestCase) -> Store {
    let mut store = store();
    for index in 0..K::COVERING {
        let mut write = store.write().expect("write");
        let stamp = write.insert(&format!("r{index}"), &K::generate(tc, Pick::Cover(index)), &origin()).expect("insert");
        write.commit().expect("commit");
        let current = store.read().expect("read").load::<K>(stamp.id).expect("load").expect("present");
        if let Some((edge, _)) = current.value.edge(tc, Pick::Cover(index)) {
            let mut write = store.write().expect("write");
            write.apply_edge(current.stamp(), edge.clone(), &NewChange::new("test", edge.at())).expect("edge");
            write.commit().expect("commit");
        }
    }
    store
}

fn read(store: &Store, table: &str, filter: Option<&Checked<Condition>>) -> Result<(Table, Rows), flotilla_schema_sqlite::StoreError> {
    store.table_read(LOCAL, table, filter, 10_000).map(|read| (read.descriptor, read.rows))
}

/// `column == "value"`, checked against the read's description.
fn equal(descriptor: &Table, column: &str, value: &str) -> Result<Checked<Condition>, CheckError> {
    let fields = descriptor.filterable();
    let field = Field::new(fields.path(column).expect("a filterable column").clone());
    let condition = Condition::Compare { field, op: Relop::Eq, operand: Operand::Literal(Literal::String(value.into())) };
    Checked::new(condition, fields.schema())
}

/// The column's cells in an independent reading of storage: the table's readable view, which
/// names codes through the `codes` table, or for the log, `log_view` with `cause` named by the
/// log's declared vocabulary. `None` for an inherited key column, which the view leaves coded.
fn oracle(store: &Store, stored: &Table, column: &str, log: Option<&[(String, String)]>) -> Option<Vec<Value>> {
    let sql = match log {
        Some(_) => format!("SELECT \"{column}\" FROM log_view WHERE kind = '{}'", store_kind(stored)),
        None if stored.parent_key().iter().any(|key| key.name == column) => return None,
        None => format!("SELECT \"{column}\" FROM \"{}_view\"", stored.name),
    };
    let mut statement = store.connection().prepare(&sql).expect("prepare");
    let cells = statement
        .query_map([], |row| row.get::<_, Option<String>>(0))
        .expect("query")
        .map(|cell| {
            let cell = cell.expect("cell");
            let named = match (log, cell) {
                (Some(cause), Some(code)) if column == "cause" => {
                    cause.iter().find(|(value, _)| *value == code).map(|(_, name)| name.clone())
                }
                (_, cell) => cell,
            };
            named.map_or(Value::Null, Value::Text)
        })
        .collect();
    Some(cells)
}

/// The kind code a log projection's rows carry, from its `<root>.log` name.
fn store_kind(log: &Table) -> String {
    let root = log.name.strip_suffix(".log").expect("a log projection");
    crate::fixtures::registry().iter().find(|set| set.tables[0].name == root).expect("a registered kind").code.to_string()
}

fn sorted(mut cells: Vec<Value>) -> Vec<Value> {
    cells.sort_by_key(|cell| format!("{cell:?}"));
    cells
}

// Behaviour (#4214): on every table of `K` and on its log, the read's description declares each
// enum column's values as its readable names, in code order; every cell is null or one of them,
// and is the name storage's own readable view gives that row's code; a filter on a name selects
// exactly the rows carrying it, and a stored code that is no name does not check as a filter value.
fn enums_read_as_names<K: Fixture>(tc: &TestCase) {
    let store = filled::<K>(tc);
    let set = K::table_set();
    let mut tables: Vec<(Table, bool)> = set.tables.iter().map(|table| (table.clone(), false)).collect();
    tables.push((set.log_projection(), true));
    for (stored, log) in tables {
        let (descriptor, rows) = read(&store, &stored.name, None).expect("read");
        let natural = stored.with_natural_references();
        for column in descriptor.key.iter().chain(&descriptor.columns) {
            let ColumnType::Code(values) = &column.ty else { continue };
            let source = natural.key.iter().chain(&natural.columns).find(|source| source.name == column.name).expect("a stored column");
            let names: Vec<String> = source.categories.iter().map(|(_, name)| name.clone()).collect();
            let at = format!("{}.{}", stored.name, column.name);
            assert_eq!(values, &names, "{at}: the description declares the names");
            let cells: Vec<Value> = rows.iter().map(|row| row[&column.name].clone()).collect();
            for cell in &cells {
                match cell {
                    Value::Null => assert!(column.nullable, "{at}: null in a required column"),
                    Value::Text(name) => assert!(names.contains(name), "{at}: {name:?} is not one of {names:?}"),
                    other => panic!("{at}: {other:?} is not text"),
                }
            }
            let categories = log.then_some(&source.categories[..]);
            if let Some(expected) = oracle(&store, &stored, &column.name, categories) {
                assert_eq!(sorted(cells.clone()), sorted(expected), "{at}: the read names each row's code as storage does");
            }
            let Some(Value::Text(name)) = cells.iter().find(|cell| **cell != Value::Null) else { continue };
            let filter = equal(&descriptor, &column.name, name).expect("a name checks");
            let (_, selected) = read(&store, &stored.name, Some(&filter)).expect("a name filters");
            assert_eq!(selected.len(), cells.iter().filter(|cell| **cell == Value::Text(name.clone())).count(), "{at} == {name:?}");
            assert!(selected.iter().all(|row| row[&column.name] == Value::Text(name.clone())));
            let code = &source.categories.iter().find(|(_, candidate)| candidate == name).expect("a declared name").0;
            if !names.contains(code) {
                assert!(equal(&descriptor, &column.name, code).is_err(), "{at}: the code {code:?} is no wire value");
            }
        }
    }
}

#[hegel::test(test_cases = 10)]
fn message_tables_read_enum_names(tc: TestCase) {
    enums_read_as_names::<Resource<Message>>(&tc);
}

#[hegel::test(test_cases = 10)]
fn agent_session_and_person_tables_read_enum_names(tc: TestCase) {
    enums_read_as_names::<Resource<AgentSession>>(&tc);
    enums_read_as_names::<Resource<Person>>(&tc);
}

#[hegel::test(test_cases = 10)]
fn awaiting_reply_tables_read_enum_names(tc: TestCase) {
    enums_read_as_names::<Resource<AwaitingReply>>(&tc);
}

#[hegel::test(test_cases = 10)]
fn convoy_tables_read_enum_names(tc: TestCase) {
    enums_read_as_names::<Resource<Convoy>>(&tc);
}

#[hegel::test(test_cases = 10)]
fn ensure_vessel_and_terminal_session_tables_read_enum_names(tc: TestCase) {
    enums_read_as_names::<Resource<Ensure>>(&tc);
    enums_read_as_names::<Resource<Vessel>>(&tc);
    enums_read_as_names::<TerminalSession>(&tc);
}

#[hegel::test(test_cases = 10)]
fn synthetic_tables_read_enum_names(tc: TestCase) {
    enums_read_as_names::<Ticket>(&tc);
    enums_read_as_names::<Referral>(&tc);
    enums_read_as_names::<Dossier>(&tc);
}
