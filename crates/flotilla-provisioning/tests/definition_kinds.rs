//! The declared definition kinds' stored shape (#3824 layer 2b): Forge, FleetDesignation,
//! CredentialSpec, CredentialGrant, CrewDefaults, ImageLayer and WorkflowTemplate. Deterministic
//! DDL and a readable view, checked in for the owner's schema look-over, and the plans of their
//! statements.

mod schema_support;

use flotilla_provisioning::stored;
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): the definition kinds render deterministic DDL, checked in
// so the owner's look-over and every later change show as a diff.
#[test]
fn definition_store_ddl_matches_the_snapshot() {
    check("definitions.sql", &schema_ddl(&stored::definition::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of the definition kind tables.
#[test]
fn definition_store_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Definition store schema view\n\nKind tables only; the shared tables are in `definitions.sql`.\n\n{}",
        render_view(stored::definition::registry().expect("registry").iter())
    );
    check("definitions_schema_view.md", &view);
}

// Behaviour (#3320 production shape, #3392 A1): every statement over the definition tables
// searches a key or a declared index, never a scan.
#[test]
fn definition_statements_search_by_key() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::definition::registry().expect("registry"))
            .expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let failures: Vec<_> = statements
        .iter()
        .map(|(label, sql)| (label, plan(&conn, sql)))
        .filter(|(_, rows)| rows.iter().any(|row| row.contains("SCAN") || row.contains("TEMP B-TREE")))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}
