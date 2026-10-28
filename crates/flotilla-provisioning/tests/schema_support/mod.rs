//! Helpers shared by the stored-kind snapshot tests.

use std::{env, fs, path::Path};

/// Compares `actual` with the checked-in snapshot `file`, or rewrites it when
/// `UPDATE_DDL_SNAPSHOT=1`.
pub fn check(file: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots").join(file);
    if env::var_os("UPDATE_DDL_SNAPSHOT").is_some_and(|value| value == "1") {
        fs::write(&path, actual).expect("write snapshot");
        return;
    }
    let snapshot = fs::read_to_string(&path).expect("read snapshot; run with UPDATE_DDL_SNAPSHOT=1 to create it");
    assert_eq!(actual, snapshot, "{file} changed; review and rerun with UPDATE_DDL_SNAPSHOT=1");
}

/// The detail column of each row of `sql`'s query plan, with every parameter null.
pub fn plan(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let nulls = vec![rusqlite::types::Value::Null; statement.parameter_count()];
    let rows = statement.query_map(rusqlite::params_from_iter(nulls), |row| row.get::<_, String>(3)).expect("plan");
    rows.collect::<Result<_, _>>().expect("plan rows")
}

/// Whether `label`'s plan scans, or sorts through a temporary b-tree when it is not one of the
/// shared statements whose sort is reasoned (`SORTED_STATEMENTS`).
pub fn unkeyed(label: &str, rows: &[String]) -> bool {
    let sorts = !flotilla_schema_sqlite::SORTED_STATEMENTS.contains(&label);
    rows.iter().any(|row| row.contains("SCAN") || (sorts && row.contains("TEMP B-TREE")))
}
