use std::{env, fs, path::Path};

use flotilla_engine_store::{convoy_registry, registry};
use flotilla_schema::render_view;
use flotilla_schema_sqlite::schema_ddl;
use flotilla_schema_sqlite::DdlMode;

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

// Behaviour (CLAUDE.md schema review, #3587 layer 3): the Convoy stack's kinds (Convoy with its
// seats, promises, submissions and duties, and AwaitingReply) render deterministic DDL, checked in
// so the owner's look-over and every later change show as a diff.
#[test]
fn convoy_store_ddl_matches_the_snapshot() {
    check("convoy.sql", &schema_ddl(&convoy_registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review, #3587 layer 3): the readable view of the Convoy stack's
// kind tables (widths, enum expansion, child tables, indexes) is checked in beside the DDL.
#[test]
fn convoy_store_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Convoy store schema view\n\nKind tables only; the shared tables are in `convoy.sql`.\n\n{}",
        render_view(convoy_registry().expect("registry").iter())
    );
    check("convoy_schema_view.md", &view);
}

// Behaviour (#3373 ruling 6): the Convoy stack's codes are the production registry's: Convoy
// holds `cv` there and AwaitingReply `ar`, each under the same kind.
#[test]
fn convoy_store_codes_are_the_production_codes() {
    let production = flotilla_state_machines::kinds().expect("production registry");
    for kind in convoy_registry().expect("registry").iter() {
        let held = production.iter().find(|set| set.code == kind.code);
        assert_eq!(held.map(|set| set.kind.as_str()), Some(kind.kind.as_str()), "{} has no production code", kind.kind);
    }
}
