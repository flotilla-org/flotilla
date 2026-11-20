mod schema_support;

use flotilla_provisioning::stored::artifact as stored;
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): Artifact renders deterministic DDL, checked in so the
// owner's look-over and every later change show as a diff.
#[test]
fn artifact_ddl_matches_the_snapshot() {
    check("artifact.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of Artifact's tables.
#[test]
fn artifact_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Artifact store schema view\n\nKind tables only; the shared tables are in `artifact.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("artifact_schema_view.md", &view);
}

// Behaviour (#3320 production shape): every statement over Artifact's tables searches a key or a
// declared index, never a scan.
#[test]
fn artifact_statements_search_by_key() {
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
