use std::{env, fs, path::Path};

use flotilla_engine_store::registry;
use flotilla_schema::{render_view, DdlMode};
use flotilla_schema_sqlite::schema_ddl;

fn check(file: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots").join(file);
    if env::var_os("UPDATE_DDL_SNAPSHOT").is_some_and(|value| value == "1") {
        fs::write(&path, actual).expect("write snapshot");
        return;
    }
    let snapshot = fs::read_to_string(&path).expect("read snapshot; run with UPDATE_DDL_SNAPSHOT=1 to create it");
    assert_eq!(actual, snapshot, "{file} changed; review and rerun with UPDATE_DDL_SNAPSHOT=1");
}

// Behaviour (CLAUDE.md schema review): the Message switch's store DDL is deterministic and
// checked in, so any shape change shows in review as a diff.
#[test]
fn message_store_ddl_matches_the_snapshot() {
    check("message.sql", &schema_ddl(&registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of each kind table (widths, enum
// expansion, child tables, indexes) is checked in beside the DDL.
#[test]
fn message_store_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Message store schema view\n\nKind tables only; the shared tables are in `message.sql`.\n\n{}",
        render_view(registry().expect("registry").iter())
    );
    check("schema_view.md", &view);
}
