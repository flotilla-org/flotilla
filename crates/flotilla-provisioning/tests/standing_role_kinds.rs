//! StandingRole's stored shape: deterministic DDL and a readable view, checked in for the owner's
//! schema look-over, and the plans of its statements.

mod schema_support;

use flotilla_provisioning::stored::standing_role as stored;
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): StandingRole renders deterministic DDL, checked in so the
// owner's look-over and every later change show as a diff.
#[test]
fn standing_role_ddl_matches_the_snapshot() {
    check("standing_role.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of StandingRole's tables.
#[test]
fn standing_role_schema_view_matches_the_snapshot() {
    let view = format!(
        "# StandingRole store schema view\n\nKind tables only; the shared tables are in `standing_role.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("standing_role_schema_view.md", &view);
}

// Behaviour (#3320 production shape): every statement over StandingRole's tables searches a key,
// never a scan.
#[test]
fn standing_role_statements_search_by_key() {
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
