//! Placement's stored shape (#3824 layer 4a): FulfilmentKind and the host's EnvironmentProvider
//! settings. Deterministic DDL and a readable view, checked in for the owner's schema look-over,
//! and the plans of their statements.

mod schema_support;

use flotilla_provisioning::stored::placement as stored;
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): placement's kinds render deterministic DDL, checked in so
// the owner's look-over and every later change show as a diff.
#[test]
fn placement_ddl_matches_the_snapshot() {
    check("placement.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of placement's tables.
#[test]
fn placement_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Placement store schema view\n\nKind tables only; the shared tables are in `placement.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("placement_schema_view.md", &view);
}

// Behaviour (#3320 production shape, #3392 A1): every statement over placement's tables searches
// a key or a declared index, never a scan.
#[test]
fn placement_statements_search_by_key() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::registry().expect("registry")).expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let failures: Vec<_> = statements
        .iter()
        .map(|(label, sql)| (label, plan(&conn, sql)))
        .filter(|(_, rows)| rows.iter().any(|row| row.contains("SCAN") || row.contains("TEMP B-TREE")))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}
