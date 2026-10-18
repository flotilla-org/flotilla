use std::{env, fs, path::Path};

use flotilla_schema::DdlMode;
use flotilla_schema_sqlite::stream_schema::stream_schema_ddl;

// Schema is the review contract. One exact snapshot is sufficient for this pure renderer;
// no tables are installed or queried before the owner's schema checkpoint approval.
#[test]
fn proposed_stream_ddl_matches_snapshot() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/integration/snapshots/stream.sql");
    let ddl = stream_schema_ddl(DdlMode::Create);
    if env::var_os("UPDATE_DDL_SNAPSHOT").is_some_and(|value| value == "1") {
        fs::write(path, ddl).expect("write reviewed schema snapshot");
        return;
    }
    assert_eq!(ddl, fs::read_to_string(path).expect("read schema checkpoint snapshot"));
}

// Glue: all table/index/view creation is idempotent in the alternate render mode. This
// checks the textual contract only; executing it is deferred until owner schema approval.
#[test]
fn proposed_stream_idempotent_ddl_guards_every_create() {
    let ddl = stream_schema_ddl(DdlMode::IfNotExists);
    for line in ddl.lines().filter(|line| line.starts_with("CREATE ")) {
        assert!(line.contains(" IF NOT EXISTS "), "unguarded statement: {line}");
    }
    assert_eq!(ddl.matches("CREATE TABLE IF NOT EXISTS ").count(), 6);
    assert_eq!(ddl.matches("CREATE VIEW IF NOT EXISTS ").count(), 6);
    assert_eq!(ddl.matches("CREATE INDEX IF NOT EXISTS ").count(), 5);
}
