mod schema_support;

use flotilla_provisioning::stored::placement as stored;
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

#[test]
fn placement_ddl_matches_the_snapshot() {
    check("placement.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

#[test]
fn placement_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Placement store schema view\n\nKind tables only; the shared tables are in `placement.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("placement_schema_view.md", &view);
}

#[test]
fn placement_statements_search_by_key() {
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
