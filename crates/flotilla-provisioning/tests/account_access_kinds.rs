//! The account access kinds' stored shape (#3907): AccountBinding and AccountProvision.
//! Deterministic DDL and a readable view, checked in for the owner's schema look-over, the codes
//! they would hold beside the production kinds, and the plans of their statements.

mod schema_support;

use flotilla_provisioning::stored;
use flotilla_resources::account_access;
use flotilla_schema::{render_view, KindCode};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): the account access kinds render deterministic DDL, checked
// in so the owner's look-over and every later change show as a diff.
#[test]
fn account_access_store_ddl_matches_the_snapshot() {
    check("account_access.sql", &schema_ddl(&account_access::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of the account access kind tables.
#[test]
fn account_access_store_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Account access store schema view\n\nKind tables only; the shared tables are in `account_access.sql`.\n\n{}",
        render_view(account_access::registry().expect("registry").iter())
    );
    check("account_access_schema_view.md", &view);
}

// Behaviour (#3373 ruling 6): the account access kinds register beside every production kind
// under codes none of them holds, so the engine store can take them on at the cut-over.
#[test]
fn account_access_kind_codes_are_free_beside_the_production_kinds() {
    let mut registry = stored::production_kinds().expect("production kinds");
    for set in account_access::registry().expect("registry").iter() {
        registry.register_table_set(set.clone()).expect("no code, name or table is held twice");
    }
    let held = |kind: &str| registry.iter().find(|set| set.kind == kind).map(|set| set.code);
    assert_eq!(held("AccountBinding"), Some(KindCode::new("ab")));
    assert_eq!(held("AccountProvision"), Some(KindCode::new("ap")));
}

// Behaviour (#3320 production shape, #3392 A1): every statement for these kinds searches a key or
// a declared index, and removing a binding's identity finds its provision records by index.
#[test]
fn account_access_statements_search_by_key() {
    let store = Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", account_access::registry().expect("registry"))
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
    let sql = &statements.iter().find(|(label, _)| label == "identity_delete").expect("shared statement").1;
    let rows = plan(&conn, sql);
    assert!(rows.iter().any(|row| row.contains("account_provision_by_binding")), "{rows:?}");
}
