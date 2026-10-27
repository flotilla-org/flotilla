//! Project's stored shape (#3824 layer 3b): the Project definition and project operations'
//! ProjectOps record. Deterministic DDL and a readable view, checked in for the owner's schema
//! look-over, and the plans of their statements. The import is proven with the other definition
//! kinds' (`definition_import`).

mod schema_support;

use flotilla_provisioning::stored::project as stored;
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): Project's kinds render deterministic DDL, checked in so the
// owner's look-over and every later change show as a diff.
#[test]
fn project_ddl_matches_the_snapshot() {
    check("project.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of Project's tables.
#[test]
fn project_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Project store schema view\n\nKind tables only; the shared tables are in `project.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("project_schema_view.md", &view);
}

// Behaviour (#3320 production shape, #3392 A1): every statement over Project's tables searches a
// key or a declared index, never a scan.
#[test]
fn project_statements_search_by_key() {
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
