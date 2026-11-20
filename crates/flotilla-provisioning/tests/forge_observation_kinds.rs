mod schema_support;


use flotilla_provisioning::stored::forge_observation::{self as stored};
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): the forge observations render deterministic DDL, checked in
// so the owner's look-over and every later change show as a diff.
#[test]
fn forge_observation_ddl_matches_the_snapshot() {
    check("forge_observation.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of the forge observations' tables.
#[test]
fn forge_observation_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Forge observation store schema view\n\nKind tables only; the shared tables are in `forge_observation.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("forge_observation_schema_view.md", &view);
}

// Behaviour (#3320 production shape): every statement over the forge observations' tables searches
// a key or a declared index, never a scan.
#[test]
fn forge_observation_statements_search_by_key() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::registry().expect("registry")).expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let failures: Vec<_> = statements
        .iter()
        .map(|(label, sql)| (label, plan(&conn, sql)))
        .filter(|(label, rows)| schema_support::unkeyed(label, rows))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}
