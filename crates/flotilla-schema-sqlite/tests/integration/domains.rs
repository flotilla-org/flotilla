//! Domain isolation covers cold, pending and committed caches, queries, rollback and replay.

use std::collections::BTreeSet;

use flotilla_schema::{Address, KindRegistry, Origin, Stored, Timestamp};
use flotilla_schema_sqlite::{Store, StoreError, StoreOptions};
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "DomainPair", code = "dp", index(name = "by_writer", eq(writer)), index(name = "by_address", eq(address)))]
struct Pair {
    #[store(intern = "wr")]
    writer: String,
    #[store(intern = "ad")]
    address: String,
    #[store(intern = "wr")]
    optional: Option<String>,
    #[store(child)]
    watchers: BTreeSet<Address>,
}

fn registry() -> KindRegistry {
    let mut registry = KindRegistry::default();
    registry.register::<Pair>().expect("register");
    registry
}

fn open(conn: Connection, home: &str) -> Store {
    // One entry forces cache eviction as well as pending/committed hits.
    Store::open_with(conn, home, registry(), StoreOptions { string_cache: 1, ..StoreOptions::default() }).expect("open")
}

fn origin(text: &str) -> Origin {
    Origin { writer: text.to_owned(), at: Timestamp(0), payload: None }
}

// Property (#3422): equal text is deduplicated only within one domain. Every generated step
// preserves logical loads, queries, readable views and audit results through commit or rollback,
// restart and replication into a store with different local ids.
#[hegel::test]
fn domains_survive_cache_rollback_restart_and_replication(tc: TestCase) {
    // Empty text, escaping and Unicode; short alphabet/lengths deliberately produce duplicates.
    let home = tc.draw(gs::text().alphabet("abé '").max_size(4));
    let mut store = open(Connection::open_in_memory().expect("connection"), &home);
    for step in 0..tc.draw(gs::integers::<usize>().min_value(2).max_value(6)) {
        let text = tc.draw(gs::text().alphabet("abé '").max_size(4));
        let optional = tc.draw(gs::booleans()).then(|| text.clone());
        let value = Pair { writer: text.clone(), address: text.clone(), optional, watchers: BTreeSet::new() };
        let name = format!("r{step}");
        let rollback = tc.draw(gs::booleans());
        let mut write = store.write().expect("write");
        let stamp = write.insert(&name, &value, &origin(&text)).expect("insert");
        assert_eq!(write.load::<Pair>(stamp.id).expect("load").expect("present").value, value);
        if rollback {
            drop(write);
        } else {
            write.commit().expect("commit");
            let mut read = store.read().expect("read");
            assert!(read.page(&Pair::by_writer(&text), None, 100).expect("writer query").ids.contains(&stamp.id));
            assert!(read.page(&Pair::by_address(&text), None, 100).expect("address query").ids.contains(&stamp.id));
            drop(read);
        }
        store.audit_domains().expect("every step's domains");
        if tc.draw(gs::booleans()) {
            store = open(store.into_connection(), &home);
        }
    }
    let events = store.read().expect("read").changes_since(&home, 0, 100).expect("events");
    let mut replica = open(Connection::open_in_memory().expect("connection"), "elsewhere");
    let mut write = replica.write().expect("replica write");
    for event in &events {
        write.apply(event).expect("apply");
    }
    write.commit().expect("commit");
    replica.audit_domains().expect("replicated domains");
    let conn = store.into_connection();
    let crossed: i64 = conn.query_row("SELECT count(*) FROM domain_pair WHERE writer = address", [], |r| r.get(0)).expect("ids");
    assert_eq!(crossed, 0, "equal text in distinct domains has different ids");
    let repeated: i64 = conn
        .query_row("SELECT count(*) FROM domain_pair WHERE optional IS NOT NULL AND optional != writer", [], |r| r.get(0))
        .expect("ids");
    assert_eq!(repeated, 0, "equal text in one domain shares its id");
    let bad_view: i64 = conn.query_row("SELECT count(*) FROM domain_pair_view WHERE writer != address", [], |r| r.get(0)).expect("view");
    assert_eq!(bad_view, 0, "the readable view returns equal text from different domains");
}

// Behaviour (owner review): the diagnostic detects valid foreign keys from the wrong domain,
// including optional fields, child keys and shared columns. NULL optional fields remain valid.
#[test]
fn audit_detects_wrong_domains_in_kind_and_shared_columns() {
    for (table, column) in [
        ("domain_pair", "writer"),
        ("domain_pair", "address"),
        ("domain_pair", "optional"),
        ("domain_pair_watchers", "value"),
        ("resource_identity", "name"),
        ("resource_identity", "home"),
        ("home_sequence", "home"),
        ("log", "home"),
        ("log", "writer"),
    ] {
        let mut store = open(Connection::open_in_memory().expect("connection"), "local");
        let value =
            Pair { writer: "x".into(), address: "x".into(), optional: None, watchers: [Address::project("p").expect("address")].into() };
        let mut write = store.write().expect("write");
        write.insert("name", &value, &origin("x")).expect("insert");
        write.commit().expect("commit");
        store.audit_domains().expect("valid domains");
        let conn = store.into_connection();
        conn.execute("INSERT INTO strings(domain,s) VALUES ('wrong','x')", []).expect("wrong domain");
        // Foreign keys remain enabled: the injected dictionary id exists, so FK checks
        // succeed even though its domain disagrees with the referencing column.
        conn.execute(&format!("UPDATE {table} SET {column} = (SELECT id FROM strings WHERE domain='wrong')"), []).expect("corrupt");
        let store = open(conn, "local");
        assert!(
            matches!(store.audit_domains(), Err(StoreError::Corrupt { table: found_table, column: found_column }) if found_table == table && found_column == column),
            "{table}.{column}"
        );
    }
}

// Behaviour (review #8): a query for text present only in the other domain is empty and cannot
// create a dictionary row, both before and after warming caches and reopening the connection.
#[hegel::test]
fn queries_never_intern_text_seen_only_in_another_domain(tc: TestCase) {
    let text = tc.draw(gs::text().alphabet("abé '").max_size(4));
    let other = format!("{text}!");
    let mut store = open(Connection::open_in_memory().expect("connection"), "local");
    let value = Pair { writer: text.clone(), address: other.clone(), optional: None, watchers: BTreeSet::new() };
    let mut write = store.write().expect("write");
    write.insert("pair", &value, &origin(&text)).expect("insert");
    write.commit().expect("commit");
    let conn = store.into_connection();
    let before: i64 = conn.query_row("SELECT count(*) FROM strings", [], |row| row.get(0)).expect("count");
    let mut store = open(conn, "local");
    for _ in 0..2 {
        let mut read = store.read().expect("read");
        assert!(read.page(&Pair::by_address(&text), None, 10).expect("other domain").ids.is_empty());
        assert!(read.page(&Pair::by_writer(&other), None, 10).expect("other domain").ids.is_empty());
        assert_eq!(read.page(&Pair::by_writer(&text), None, 10).expect("own domain").ids.len(), 1);
        assert_eq!(read.page(&Pair::by_address(&other), None, 10).expect("own domain").ids.len(), 1);
    }
    let conn = store.into_connection();
    let after: i64 = conn.query_row("SELECT count(*) FROM strings", [], |row| row.get(0)).expect("count");
    assert_eq!(after, before, "lookup never interns the missing pair");
}

// Behaviour (review #6): opening an old text-only dictionary fails at the schema boundary,
// before an insert/query can produce an opaque missing-column error. No migration is implied.
#[test]
fn a_pre_domain_dictionary_is_refused_at_open() {
    let conn = Connection::open_in_memory().expect("connection");
    conn.execute_batch(
        "CREATE TABLE strings (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE) STRICT; INSERT INTO strings(s) VALUES ('old');",
    )
    .expect("old baseline");
    assert!(matches!(flotilla_schema_sqlite::audit_domains(&conn, &registry()), Err(StoreError::LegacyDictionarySchema)));
    assert!(matches!(Store::open(conn, "local", registry()), Err(StoreError::LegacyDictionarySchema)));
}

// Behaviour (review #5): the standalone audit works with a read-only connection and leaves the
// database unchanged. The same production function is used by the operator example.
#[test]
fn the_standalone_audit_needs_no_write_access() {
    let mut store = open(Connection::open_in_memory().expect("connection"), "local");
    let mut write = store.write().expect("write");
    write
        .insert("pair", &Pair { writer: "w".into(), address: "a".into(), optional: None, watchers: BTreeSet::new() }, &origin("w"))
        .expect("insert");
    write.commit().expect("commit");
    let conn = store.into_connection();
    conn.pragma_update(None, "query_only", true).expect("read-only enforcement");
    let changes = conn.total_changes();
    flotilla_schema_sqlite::audit_domains(&conn, &registry()).expect("audit");
    assert_eq!(conn.total_changes(), changes);
}
