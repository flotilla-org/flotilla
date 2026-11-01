//! Table filters (#4108): a `where` condition in the `rigging-condition` language, translated to
//! SQL through the read's column map. `under` on an address column agrees with the language's
//! evaluator and with `Address::is_under`, and searches the `strings` index.

use std::collections::BTreeSet;

use flotilla_address::{Address, Forge, Kind};
use flotilla_schema::{KindRegistry, Table};
use flotilla_schema_sqlite::Store;
use hegel::{generators as gs, TestCase};
use rigging_condition::{Checked, Condition, Value, Values};
use rusqlite::Connection;

use crate::fixtures::{origin, LOCAL};

#[derive(flotilla_schema::Stored)]
#[store(kind = "Mailbox", code = "mb")]
struct Mailbox {
    inbox: Address,
    cc: Option<Address>,
}

/// Whether a row with this `inbox` and `cc` is kept, by `Address::is_under`.
type Oracle<'a> = dyn Fn(&Address, Option<&Address>) -> bool + 'a;

/// Addresses on and around segment boundaries: `demo` beside `demo2`, `demo-x` and `demo.x`
/// (bytes just below `/`), values holding an encoded `/` (`demo%2Fx`, `org%2Frepo`), and
/// descendants several segments down.
fn pool() -> Vec<Address> {
    let project = Address::project("flotilla").expect("project");
    let convoy = |name: &str| project.child(Kind::Convoy, name).expect("convoy");
    let github = Forge::github();
    let repository = |name: &str| Address::repository(&github, name).expect("repository");
    let coder = convoy("demo").child(Kind::Role, "coder").expect("role");
    vec![
        project.clone(),
        Address::project("flotilla2").expect("project"),
        convoy("demo"),
        convoy("demo2"),
        convoy("demo-x"),
        convoy("demo.x"),
        convoy("demo/x"),
        coder.clone(),
        Address::message(&coder, "m1").expect("message"),
        convoy("demo2").child(Kind::Role, "coder").expect("role"),
        Address::forge(&github),
        repository("org/repo"),
        repository("org/repo2"),
        repository("org/re"),
        Address::change_request(&github, "org/repo", 7).expect("change request"),
    ]
}

fn store() -> Store {
    let mut registry = KindRegistry::new();
    registry.register::<Mailbox>().expect("register");
    Store::open(Connection::open_in_memory().expect("in-memory database"), LOCAL, registry).expect("open store")
}

fn described(store: &Store) -> Table {
    store.table_read(LOCAL, "mailbox", None, 1).expect("read").descriptor
}

fn check(table: &Table, text: &str) -> Checked<Condition> {
    Checked::new(Condition::parse_strict(text).expect("canonical text"), table.filterable().schema()).expect("checks")
}

fn names(store: &Store, filter: &Checked<Condition>) -> BTreeSet<String> {
    let read = store.table_read(LOCAL, "mailbox", Some(filter), 1).expect("filtered read");
    read.rows
        .iter()
        .map(|row| match &row["resource.name"] {
            flotilla_schema::Value::Text(name) => name.clone(),
            other => panic!("resource.name {other:?}"),
        })
        .collect()
}

// Behaviour (#4108, §4.6 and §4.9 of the condition design note): over rows whose `inbox` and
// nullable `cc` are drawn from addresses on segment boundaries, a filter of `under` (alone,
// negated, or in a disjunction) keeps in SQL exactly the rows the language's evaluator gives T,
// and exactly those `Address::is_under` says, with an absent `cc` never kept.
#[hegel::test(test_cases = 200)]
fn under_in_sql_agrees_with_the_evaluator_and_is_under(tc: TestCase) {
    let pool = pool();
    let pick = |tc: &TestCase| pool[tc.draw(gs::integers::<usize>().min_value(0).max_value(pool.len() - 1))].clone();
    let mut store = store();
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(10));
    let rows: Vec<(String, Address, Option<Address>)> =
        (0..count).map(|index| (format!("r{index}"), pick(&tc), tc.draw(gs::booleans()).then(|| pick(&tc)))).collect();
    let mut write = store.write().expect("write");
    for (name, inbox, cc) in &rows {
        write.insert(name, &Mailbox { inbox: inbox.clone(), cc: cc.clone() }, &origin()).expect("insert");
    }
    write.commit().expect("commit");
    let prefix = pick(&tc);
    let p = serde_json::to_string(prefix.as_str()).expect("json string");
    let under = |address: &Address| address.is_under(&prefix);
    let shapes: [(String, &Oracle); 4] = [
        (format!("inbox.under({p})"), &|inbox, _| under(inbox)),
        (format!("cc.under({p})"), &|_, cc| cc.is_some_and(under)),
        (format!("!cc.under({p})"), &|_, cc| cc.is_some_and(|cc| !under(cc))),
        (format!("inbox.under({p}) || cc.under({p})"), &|inbox, cc| under(inbox) || cc.is_some_and(under)),
    ];
    let shape = tc.draw(gs::integers::<usize>().min_value(0).max_value(shapes.len() - 1));
    let (text, oracle) = &shapes[shape];
    let filter = check(&described(&store), text);
    let evaluated: BTreeSet<String> = rows
        .iter()
        .filter(|(_, inbox, cc)| {
            let mut row = Values::default();
            row.present.insert("inbox".parse().expect("path"), Value::String(inbox.as_str().into()));
            if let Some(cc) = cc {
                row.present.insert("cc".parse().expect("path"), Value::String(cc.as_str().into()));
            }
            filter.evaluate(&row).is_true()
        })
        .map(|(name, ..)| name.clone())
        .collect();
    let expected: BTreeSet<String> =
        rows.iter().filter(|(_, inbox, cc)| oracle(inbox, cc.as_ref())).map(|(name, ..)| name.clone()).collect();
    assert_eq!(evaluated, expected, "{text}: the evaluator and is_under");
    assert_eq!(names(&store, &filter), expected, "{text}: SQL and is_under");
}

/// The detail column of each row of the statement's query plan.
fn plan(store: &Store, filter: &Checked<Condition>) -> Vec<String> {
    let (sql, arguments) = store.table_read_statement(LOCAL, "mailbox", Some(filter)).expect("statement");
    let mut statement = store.connection().prepare(&format!("EXPLAIN QUERY PLAN {sql}")).expect("prepare");
    let rows = statement.query_map(rusqlite::params_from_iter(arguments), |row| row.get::<_, String>(3)).expect("plan");
    rows.collect::<Result<_, _>>().expect("plan rows")
}

// Behaviour (#4108, §4.9): `under`, equality and `in` on an interned address column find the
// matching strings by a search of the `strings` table's `UNIQUE(domain, s)` index (a range for
// `under`), never by resolving each row's text.
#[test]
fn address_filters_search_the_strings_index() {
    let store = store();
    let table = described(&store);
    for text in [
        r#"inbox.under("project:flotilla/convoy:demo")"#,
        r#"inbox == "project:flotilla/convoy:demo""#,
        r#"cc in ["project:flotilla/convoy:demo", "project:flotilla"]"#,
    ] {
        let rows = plan(&store, &check(&table, text));
        let index = "SEARCH strings USING COVERING INDEX sqlite_autoindex_strings_1 (domain=? AND s";
        assert!(rows.iter().any(|row| row.starts_with(index)), "{text}: {rows:#?}");
        if text.contains("under") {
            assert!(rows.iter().any(|row| row.starts_with(index) && row.contains("s>? AND s<?")), "{text}: a range: {rows:#?}");
        }
    }
}
