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

// Behaviour (#3938, #3972): a decided store created before the Person consumer keeps the old CHECK
// on `message.receiver_kind`. Opening rebuilds the table with the wider CHECK rather than letting a
// person's first acknowledgement fail; a current store opens.
#[test]
fn a_store_whose_receiver_kinds_predate_the_person_consumer_is_widened_at_open() {
    use flotilla_schema_sqlite::{Store, StoreError};
    let current = schema_ddl(&registry().expect("registry"), DdlMode::IfNotExists);
    let previous = current.replace(
        r#""receiver_kind" TEXT CHECK ("receiver_kind" IN ('as', 'pe', 'sv'))"#,
        r#""receiver_kind" TEXT CHECK ("receiver_kind" IN ('as', 'sv'))"#,
    );
    assert_ne!(previous, current, "the previous generation's CHECK is reconstructed");
    let connection = rusqlite::Connection::open_in_memory().expect("connection");
    connection.execute_batch(&previous).expect("previous schema");
    let store = Store::open(connection, "home", registry().expect("registry")).expect("the previous generation opens");
    let sql: String = store
        .into_connection()
        .query_row("SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'message'", [], |row| row.get(0))
        .expect("message table");
    assert!(sql.contains(r#"CHECK ("receiver_kind" IN ('as', 'pe', 'sv'))"#), "widened to the Person consumer: {sql}");
    // A CHECK that cannot be read fails closed rather than passing as current.
    let unchecked = current.replace(r#""receiver_kind" TEXT CHECK ("receiver_kind" IN ('as', 'pe', 'sv'))"#, r#""receiver_kind" TEXT"#);
    let connection = rusqlite::Connection::open_in_memory().expect("connection");
    connection.execute_batch(&unchecked).expect("unchecked schema");
    match Store::open(connection, "home", registry().expect("registry")) {
        Err(StoreError::KindCodeUnreadable { table, column }) => {
            assert_eq!((table.as_str(), column.as_str()), ("message", "receiver_kind"))
        }
        other => panic!("refused at open, not {:?}", other.map(|_| "opened")),
    }
    let connection = rusqlite::Connection::open_in_memory().expect("connection");
    connection.execute_batch(&current).expect("current schema");
    Store::open(connection, "home", registry().expect("registry")).expect("a current store opens");
}

// Behaviour (#3108, TL-3 `br`): a decided store created before LinkSource::Branch keeps the old
// CHECK on `convoy_subjects.source`. Opening rebuilds the table with `br` rather than letting a
// convoy's first branch link fail later, and a Branch link is then written.
#[test]
fn a_store_whose_link_sources_predate_branch_is_widened_at_open() {
    use flotilla_engine_store::StoreLog;
    use flotilla_schema::{Frozen, Timestamp};
    use flotilla_schema_sqlite::{Store, StoreHandle};
    use flotilla_state_machines::{machines::convoy::*, Home, Ref};
    use std::{collections::BTreeMap, sync::Arc};
    let current = schema_ddl(&convoy_registry().expect("registry"), DdlMode::IfNotExists);
    let previous = current
        .replace(
            r#""source" TEXT NOT NULL CHECK ("source" IN ('br', 'cl', 'op', 'rl'))"#,
            r#""source" TEXT NOT NULL CHECK ("source" IN ('cl', 'op', 'rl'))"#,
        )
        .replace("  ('cv', 'subjects.source', 'br', 'Branch'),\n", "");
    assert_ne!(previous, current, "the previous generation's CHECK is reconstructed");
    let connection = rusqlite::Connection::open_in_memory().expect("connection");
    connection.execute_batch(&previous).expect("previous schema");
    let store = Store::open(connection, "home", convoy_registry().expect("registry")).expect("the previous generation opens");
    let store = Arc::new(StoreHandle::new(store));
    let sql: String = store
        .lock()
        .expect("store")
        .connection()
        .query_row("SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'convoy_subjects'", [], |row| row.get(0))
        .expect("subjects table");
    assert!(sql.contains(r#"CHECK ("source" IN ('br', 'cl', 'op', 'rl'))"#), "widened to Branch: {sql}");

    let mut home = Home::<Convoy, _>::with_log("home", StoreLog::new(Arc::clone(&store))).expect("home");
    let project: flotilla_schema::Address = "project:flotilla".parse().expect("project");
    let authority = Ref::new("ensure");
    let mut world = MemoryConvoyWorld::default().with_project(&project).with_limits(3, 1);
    world.authority.put(&authority, true);
    let name = "project:flotilla/convoy:fix";
    let by = SeatName::new("coder", "");
    let launch = home
        .create(
            name,
            ConvoyRequest {
                launch_of: name.into(),
                workflow_ref: "w".into(),
                admission: Frozen::new(AdmissionBytes::new(Vec::new())).expect("freeze"),
                project,
                priority: Priority(0),
                kinds: Frozen::new(PromiseKinds::default()).expect("kinds"),
                seats: vec![Seat {
                    role: by.role.clone(),
                    seat: by.seat.clone(),
                    parent: None,
                    supervised_by: name.parse().expect("address"),
                    failed: None,
                    retired: false,
                    replacements: 0,
                }],
                promises: BTreeMap::new(),
                duties: BTreeMap::from([(DutyId(0), Duty { owner: by.clone(), scope: DutyScope::Lifetime, terms: Terms::default() })]),
            },
            &world,
            Timestamp(0),
        )
        .expect("launch")
        .resource
        .name()
        .to_owned();
    let link = Link { subject: "project:change".parse().expect("subject"), source: LinkSource::Branch, by, authority };
    home.apply(&launch, link, &world, Timestamp(1)).expect("a branch link is written");
    let held = home.get(&launch).expect("held");
    assert_eq!(held.spec().subjects.iter().map(|row| row.record.source).collect::<Vec<_>>(), [LinkSource::Branch]);
}
