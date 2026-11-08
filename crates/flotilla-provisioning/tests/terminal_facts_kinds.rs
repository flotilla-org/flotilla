//! PROPOSAL (#3108): TerminalSession with its launch number, and the record its launches would
//! need on the engine, checked in for the owner's schema look-over before anything reads or
//! writes them.

mod schema_support;

use flotilla_provisioning::stored;
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): the proposed kinds render deterministic DDL.
#[test]
fn terminal_proposal_ddl_matches_the_snapshot() {
    check("terminal_proposal.sql", &schema_ddl(&stored::terminal_proposal_registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of the proposed tables.
#[test]
fn terminal_proposal_schema_view_matches_the_snapshot() {
    let view = format!(
        "# TerminalSession proposal schema view\n\nKind tables only; the shared tables are in `terminal_proposal.sql`.\n\n{}",
        render_view(stored::terminal_proposal_registry().expect("registry").iter())
    );
    check("terminal_proposal_schema_view.md", &view);
}

// Behaviour (#3320 production shape): every statement for the proposed kinds searches a key or a
// declared index. Only plans are read.
#[test]
fn terminal_proposal_statements_search_by_key() {
    let registry = stored::terminal_proposal_registry().expect("registry");
    let store = Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", registry).expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let failures: Vec<_> = statements
        .iter()
        .filter_map(|(label, sql)| {
            let rows = plan(&conn, sql);
            schema_support::unkeyed(label, &rows).then(|| format!("{label}: {rows:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}
