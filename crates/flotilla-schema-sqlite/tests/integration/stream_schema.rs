use std::{env, fs, path::Path};

use flotilla_schema::{quote_ident, DdlMode, KindRegistry};
use flotilla_schema_sqlite::stream_schema::{stream_schema_ddl, WHOLE_LOG_TOPIC};
use rusqlite::Connection;

use crate::fixtures::registry;

// The snapshot is the complete shared schema proposal, including the topic column and
// readable views. Kind tables use their existing renderer and are covered by fixture DDL.
#[test]
fn proposed_stream_ddl_matches_snapshot() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/integration/snapshots/stream.sql");
    let ddl = stream_schema_ddl(&KindRegistry::new(), DdlMode::Create);
    if env::var_os("UPDATE_DDL_SNAPSHOT").is_some_and(|value| value == "1") {
        fs::write(path, ddl).expect("write proposed schema snapshot for owner review");
        return;
    }
    assert_eq!(ddl, fs::read_to_string(path).expect("read schema checkpoint snapshot"));
}

// Owner checkpoint: execute the complete proposal with foreign keys enabled and read every
// view, including kind views. A text snapshot alone cannot establish SQL validity.
#[test]
fn proposed_stream_ddl_executes_and_every_view_is_readable() {
    let conn = Connection::open_in_memory().expect("in-memory SQLite");
    conn.execute_batch("PRAGMA foreign_keys = ON").expect("enable foreign keys");
    let enabled: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).expect("foreign keys");
    assert!(enabled);
    let kinds = registry();
    conn.execute_batch(&stream_schema_ddl(&kinds, DdlMode::Create)).expect("execute complete proposed DDL");
    let views: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'view' ORDER BY name")
        .expect("list views")
        .query_map([], |row| row.get(0))
        .expect("query views")
        .collect::<Result<_, _>>()
        .expect("view names");
    assert_eq!(views.len(), kinds.iter().map(|set| set.tables.len()).sum::<usize>() + 3);
    for view in views {
        let mut select = conn.prepare(&format!("SELECT * FROM {}", quote_ident(&view))).expect("prepare view read");
        assert!(select.query([]).expect("read view").next().expect("view result").is_none(), "new view {view} should be empty");
    }
    let violations: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| row.get(0)).expect("check foreign keys");
    assert_eq!(violations, 0);
    let topic_default: String = conn
        .query_row("SELECT dflt_value FROM pragma_table_info('log') WHERE name = 'topic'", [], |row| row.get(0))
        .expect("reserved topic column");
    assert_eq!(topic_default, format!("'{}'", WHOLE_LOG_TOPIC));
    let schema_version: i64 = conn.query_row("PRAGMA schema_version", [], |row| row.get(0)).expect("schema version");
    conn.execute_batch(&stream_schema_ddl(&kinds, DdlMode::IfNotExists)).expect("reapply proposed DDL");
    let reopened: i64 = conn.query_row("PRAGMA schema_version", [], |row| row.get(0)).expect("reopened schema version");
    assert_eq!(reopened, schema_version, "idempotent DDL does not change the schema");
}
