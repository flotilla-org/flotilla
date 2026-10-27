//! The access layer's per-access status (#3964): AccessDelivery. Deterministic DDL and
//! a readable view, checked in for the owner's schema look-over, the code it would hold
//! beside the production and account access kinds, and the plans of its statements.

mod schema_support;

use flotilla_provisioning::stored;
use flotilla_resources::{access_delivery, account_access};
use flotilla_schema::{render_view, KindCode};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): the access delivery kind renders deterministic DDL,
// checked in so the owner's look-over and every later change show as a diff.
#[test]
fn access_delivery_store_ddl_matches_the_snapshot() {
    check("access_delivery.sql", &schema_ddl(&access_delivery::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of the access delivery kind tables.
#[test]
fn access_delivery_store_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Access delivery store schema view\n\nKind tables only; the shared tables are in `access_delivery.sql`.\n\n{}",
        render_view(access_delivery::registry().expect("registry").iter())
    );
    check("access_delivery_schema_view.md", &view);
}

// Behaviour (#3373 ruling 6): the access delivery kind registers beside every production
// kind and the account access kinds under a code none of them holds.
#[test]
fn access_delivery_kind_code_is_free_beside_the_production_kinds() {
    let mut registry = stored::production_kinds().expect("production kinds");
    for set in account_access::registry().expect("registry").iter().chain(access_delivery::registry().expect("registry").iter()) {
        registry.register_table_set(set.clone()).expect("no code, name or table is held twice");
    }
    assert_eq!(registry.iter().find(|set| set.kind == "AccessDelivery").map(|set| set.code), Some(KindCode::new("ad")));
}

// Behaviour (#3320 production shape, #3392 A1): every statement for the kind searches a key
// or a declared index, and removing a place finds its deliveries by index.
#[test]
fn access_delivery_statements_search_by_key() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", access_delivery::registry().expect("registry"))
            .expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let mut failures = Vec::new();
    for (label, sql) in &statements {
        let rows = plan(&conn, sql);
        if rows.iter().any(|row| row.contains("SCAN") || row.contains("TEMP B-TREE")) {
            failures.push(format!("{label}: {rows:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
