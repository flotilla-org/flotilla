//! Derived indexes: the descriptor entry, its DDL and the typed query constructors.

use flotilla_schema::{Code, Condition, EncodeError, Identity, Index, IndexQuery, Kind, KindCode, Ref, State, Stored, TableSet, Value};
use flotilla_schema_sqlite::render_ddl;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum Phase {
    // Codes deliberately unlike declaration positions: predicates must read them from the enum.
    #[store(code = "c9")]
    Open,
    #[store(code = "c7")]
    Waiting,
    #[store(code = "c3")]
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Pool(String);

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Desk", code = "k2")]
struct Desk {
    label: String,
}

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(
    kind = "Ticket",
    code = "k1",
    index(name = "open_by_desk", eq(desk), order(seq), when(phase in [Open, Waiting])),
    index(name = "by_parent", eq(parent, pool), when(parent is_some)),
    index(name = "due", order(due), when(phase in [Waiting], due is_some))
)]
struct Ticket {
    desk: Ref<Desk>,
    seq: u64,
    parent: Option<Ref<Ticket>>,
    #[store(intern = "pl")]
    pool: Pool,
    due: Option<i64>,
    #[store(state)]
    phase: State<Phase>,
}

/// A generic kind still gets its constructor.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Holder", code = "k3", index(name = "by_seq", order(seq)))]
struct Holder<T> {
    seq: i64,
    extra: T,
}

fn index<'s>(set: &'s TableSet, name: &str) -> &'s Index {
    set.indexes.iter().find(|index| index.name == name).expect("declared index")
}

// Behaviour (DESIGN indexes): each `index(...)` becomes a descriptor entry on the root table with
// its eq and order columns and a structured predicate whose codes are the enum's own codes.
#[test]
fn indexes_become_descriptor_entries() {
    let set = Ticket::table_set();
    set.validate().expect("valid");
    let open = index(&set, "open_by_desk");
    assert_eq!(
        (open.table.as_str(), open.eq.as_slice(), open.order.as_slice()),
        ("ticket", &["desk".to_owned()][..], &["seq".to_owned()][..])
    );
    assert_eq!(open.predicate, [Condition::In { column: "phase".into(), codes: vec![Code::new("c9"), Code::new("c7")] }]);
    let parent = index(&set, "by_parent");
    assert_eq!(parent.eq, ["parent", "pool"]);
    assert_eq!(parent.predicate, [Condition::NotNull { column: "parent".into() }]);
    let due = index(&set, "due");
    assert_eq!(
        due.predicate,
        [Condition::In { column: "phase".into(), codes: vec![Code::new("c7")] }, Condition::NotNull { column: "due".into() }]
    );
    assert_eq!(Holder::<String>::table_set().indexes[0].order, ["seq"]);
}

// Behaviour (DESIGN DDL): indexes render after the tables, sorted by object name `<table>_<name>`.
#[test]
fn indexes_render_as_partial_indexes() {
    let ddl = render_ddl(&Ticket::table_set());
    let indexes: Vec<&str> = ddl.lines().filter(|line| line.starts_with("CREATE INDEX")).collect();
    assert_eq!(
        indexes,
        [
            r#"CREATE INDEX "ticket_by_parent" ON "ticket" ("parent", "pool") WHERE "parent" IS NOT NULL;"#,
            r#"CREATE INDEX "ticket_due" ON "ticket" ("due") WHERE "phase" IN ('c7') AND "due" IS NOT NULL;"#,
            r#"CREATE INDEX "ticket_open_by_desk" ON "ticket" ("desk", "seq") WHERE "phase" IN ('c9', 'c7');"#,
        ]
    );
}

// Behaviour (DESIGN queries): constructors take the eq fields' types (inside `Option` when the
// field is optional) and carry logical values: identities and strings, never local ids.
#[test]
fn constructors_carry_logical_arguments() {
    let query: IndexQuery<Ticket> = Ticket::open_by_desk(&Ref::new("front", "home-a"));
    assert_eq!(query.index(), "open_by_desk");
    let desk = Identity { kind: KindCode::new("k2"), name: "front".into(), home: "home-a".into() };
    assert_eq!(query.eq(), Ok(&[Value::Ref(desk)][..]));

    let query = Ticket::by_parent(&Ref::new("t", "h"), &Pool("blue".into()));
    let parent = Identity { kind: KindCode::new("k1"), name: "t".into(), home: "h".into() };
    assert_eq!(query.eq(), Ok(&[Value::Ref(parent), Value::Str("blue".into())][..]));

    assert_eq!(Ticket::due().eq(), Ok(&[][..]));
    assert_eq!(Holder::<u8>::by_seq().index(), "by_seq");
}

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Counter", code = "k4", index(name = "by_count", eq(count)))]
struct Counter {
    count: u64,
}

// Behaviour (DESIGN u64): an argument that cannot be stored surfaces as the encode error when the
// query runs, not as a wrapped value.
#[test]
fn unstorable_arguments_are_refused() {
    assert_eq!(Counter::by_count(&u64::MAX).eq(), Err(EncodeError::OutOfRange { column: String::new(), value: u64::MAX }));
    assert_eq!(Counter::by_count(&5).eq(), Ok(&[Value::Int(5)][..]));
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Book {
    title: String,
    copy: u64,
}

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Shelf", code = "k5", index(name = "holding", eq(books)))]
struct Shelf {
    label: String,
    #[store(child)]
    books: Vec<Book>,
}

// Behaviour: an index on a child list is an index of the list's table on every column of its
// element, and its constructor carries one element's values.
#[test]
fn child_list_indexes_key_the_lists_table_by_its_element() {
    let set = Shelf::table_set();
    set.validate().expect("valid");
    let holding = index(&set, "holding");
    assert_eq!((holding.table.as_str(), holding.eq.as_slice()), ("shelf_books", &["title".to_owned(), "copy".to_owned()][..]));
    let ddl = render_ddl(&set);
    assert!(ddl.contains(r#"CREATE INDEX "shelf_books_holding" ON "shelf_books" ("title", "copy");"#), "{ddl}");
    let query = Shelf::holding(&Book { title: "a".into(), copy: 2 });
    assert_eq!(query.eq(), Ok(&[Value::Text("a".into()), Value::Int(2)][..]));
}

// Behaviour: a child-list index query pages the resources holding an equal row, each once however
// many equal rows it holds, in id order, through the index alone.
#[hegel::test]
fn child_list_queries_page_the_holders(tc: hegel::TestCase) {
    use std::collections::BTreeSet;

    use flotilla_schema::{KindRegistry, NewChange, Timestamp};
    use flotilla_schema_sqlite::{Cursor, Store};
    use hegel::generators as gs;

    let book = |tc: &hegel::TestCase| Book {
        title: tc.draw(gs::sampled_from(vec!["a".to_owned(), "b".to_owned()])),
        copy: tc.draw(gs::integers::<u64>().max_value(1)),
    };
    let mut registry = KindRegistry::new();
    registry.register::<Shelf>().expect("register");
    let mut store = Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", registry).expect("open");
    let shelves: Vec<Shelf> = (0..tc.draw(gs::integers::<usize>().max_value(6)))
        .map(|n| Shelf { label: n.to_string(), books: (0..tc.draw(gs::integers::<usize>().max_value(3))).map(|_| book(&tc)).collect() })
        .collect();
    let mut write = store.write().expect("write");
    for shelf in &shelves {
        write.insert(&shelf.label, shelf, &NewChange::new("test", Timestamp(0))).expect("insert");
    }
    write.commit().expect("commit");

    let wanted = book(&tc);
    let limit = tc.draw(gs::integers::<u32>().min_value(1).max_value(3));
    let mut read = store.read().expect("read");
    let mut expected = BTreeSet::new();
    for shelf in shelves.iter().filter(|shelf| shelf.books.contains(&wanted)) {
        expected.insert(read.resolve(Shelf::CODE, &shelf.label, "home").expect("resolve").expect("stored"));
    }
    let query = Shelf::holding(&wanted);
    let (mut ids, mut after): (Vec<_>, Option<Cursor>) = (Vec::new(), None);
    loop {
        let page = read.page(&query, after.as_ref(), limit).expect("page");
        ids.extend(page.ids);
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    assert_eq!(ids, expected.into_iter().collect::<Vec<_>>());
}
