//! Derived indexes: the descriptor entry, its DDL and the typed query constructors.

use flotilla_schema::{
    render_ddl, Code, Condition, EncodeError, Identity, Index, IndexQuery, Kind, KindCode, Ref, State, Stored, TableSet, Value,
};

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
    #[store(intern)]
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
