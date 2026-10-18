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
        // Disable foreign-key update checks only for shared parent keys; all resulting references
        // still use existing dictionary ids, which is the gap the domain audit covers.
        conn.pragma_update(None, "foreign_keys", false).expect("disable");
        conn.execute(&format!("UPDATE {table} SET {column} = (SELECT id FROM strings WHERE domain='wrong')"), []).expect("corrupt");
        let store = open(conn, "local");
        assert!(matches!(store.audit_domains(), Err(StoreError::Corrupt { .. })), "{table}.{column}");
    }
}
