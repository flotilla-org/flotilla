//! Opening a store created by a build with fewer codes (#3972): the engine's real Message
//! tables, rendered from a registry whose `receiver_kind` predates the Controller consumer.

use flotilla_engine_store::registry;
use flotilla_schema::{Address, ColumnType, KindRegistry, Origin, State, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store, StoreError};
use flotilla_state_machines::{
    machines::{
        consumer::{ConsumerKind, Receiver},
        message::{Message, MessageBody, MessageReference, MessageSpec, MessageState, Relation},
    },
    Resource, Version,
};
use rusqlite::Connection;

const HOME: &str = "alpha";

/// `kinds` as an older build rendered it: each `(table, column, code)` without that code, in the
/// column's codes and its categories. Guards are kept: a guard added with a code is a difference
/// in more than codes, which opening refuses.
fn narrowed(kinds: KindRegistry, without: &[(&str, &str, &str)]) -> KindRegistry {
    let mut older = KindRegistry::new();
    for set in kinds.iter() {
        let mut set = set.clone();
        for table in &mut set.tables {
            for (_, column, code) in without.iter().filter(|(name, _, _)| *name == table.name) {
                let def = table.columns.iter_mut().find(|def| def.name == *column).expect("narrowed column");
                let ColumnType::Code(codes) = &mut def.ty else { panic!("{column} is coded") };
                codes.retain(|kept| kept != code);
                def.categories.retain(|(kept, _)| kept != code);
            }
        }
        older.register_table_set(set).expect("narrowed registry");
    }
    older
}

/// The engine registry before the Controller consumer and the Dependency relation.
fn registry_without_controllers() -> KindRegistry {
    narrowed(registry().expect("registry"), &[("message", "receiver_kind", "co"), ("message", "relation", "dy")])
}

fn message(id: &str, kind: ConsumerKind, relation: Relation) -> Resource<Message> {
    let inbox: Address = "project:fleet/convoy:demo/role:coder".parse().expect("inbox");
    let name = Address::message(&inbox, id).expect("message address").as_str().to_owned();
    let spec = MessageSpec {
        references: vec![MessageReference { address: inbox.clone(), revision: "r1".into(), subject: true }],
        ..MessageSpec::new(
            "project:fleet/role:turn-rules".parse().expect("sender"),
            inbox,
            relation,
            MessageBody::text("take a turn").expect("small body"),
        )
    };
    let receiver = Receiver { kind, id: format!("{id}-consumer"), host: HOME.into() };
    Resource::<Message>::stored(&name, spec, State::new(MessageState::Awaiting { receiver }, Timestamp(1)), Version(0))
}

fn insert(store: &mut Store, value: &Resource<Message>) -> Result<flotilla_schema_sqlite::ResourceId, StoreError> {
    let mut write = store.write()?;
    let stamp = write.insert(value.name(), value, &Origin::new("test", Timestamp(1)))?;
    write.commit()?;
    Ok(stamp.id)
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).expect("count")
}

// Behaviour (#3972): a store whose message table was created before `receiver_kind` had `co` and
// `relation` had `dy` opens under this build, which rebuilds the table with both wider CHECKs: every message and its
// references (a child table whose foreign key cascades from it) survive, the table is the one
// this build renders, and a Dependency message for a Controller is written.
#[test]
fn a_store_with_fewer_codes_opens_wider_and_keeps_its_rows() {
    let mut old = Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry_without_controllers()).expect("old store");
    let kept: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|id| {
            let id = insert(&mut old, &message(id, ConsumerKind::AgentSession, Relation::System)).expect("session message");
            (id, old.read().expect("read").load::<Resource<Message>>(id).expect("load").expect("stored").value)
        })
        .collect();
    assert!(insert(&mut old, &message("early", ConsumerKind::Controller, Relation::System)).is_err(), "the old table refuses `co`");
    let conn = old.into_connection();
    let (messages, references) = (count(&conn, "message"), count(&conn, "message_references"));
    assert_eq!((messages, references), (2, 2));

    let mut store = Store::open(conn, HOME, registry().expect("registry")).expect("opens with wider codes");
    for (id, value) in &kept {
        let loaded = store.read().expect("read").load::<Resource<Message>>(*id).expect("load").expect("kept");
        assert_eq!(&loaded.value, value);
    }
    let controller = message("controller", ConsumerKind::Controller, Relation::Dependency);
    let id = insert(&mut store, &controller).expect("a Controller's message is written");
    let written = store.read().expect("read").load::<Resource<Message>>(id).expect("load").expect("written").value;
    assert_eq!(written.state(), controller.state());

    let conn = store.into_connection();
    assert_eq!((count(&conn, "message"), count(&conn, "message_references")), (messages + 1, references + 1));
    let stored: String =
        conn.query_row("SELECT sql FROM sqlite_schema WHERE name = 'message'", [], |row| row.get(0)).expect("message table");
    assert!(schema_ddl(&registry().expect("registry"), DdlMode::Create).contains(&format!("{stored};\n")), "{stored}");
    let index: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_schema WHERE type = 'index' AND tbl_name = 'message' AND sql IS NOT NULL", [], |row| {
            row.get(0)
        })
        .expect("indexes");
    assert!(index > 0, "the message table's indexes are recreated");
    conn.execute("DELETE FROM message WHERE id = (SELECT min(id) FROM message)", []).expect("delete a kept message");
    assert_eq!(count(&conn, "message_references"), references, "the references' foreign key cascades from the new table");
    Store::open(conn, HOME, registry().expect("registry")).expect("reopens unchanged");
}

// Behaviour (#3972): a store whose stored CHECK accepts a code this build no longer declares
// is refused at open, since its rows may hold that code.
#[test]
fn a_store_with_a_removed_code_is_refused() {
    let mut store = Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry().expect("registry")).expect("store");
    insert(&mut store, &message("controller", ConsumerKind::Controller, Relation::System)).expect("controller message");
    let refused =
        Store::open(store.into_connection(), HOME, narrowed(registry().expect("registry"), &[("message", "receiver_kind", "co")]));
    assert!(
        matches!(&refused, Err(StoreError::KindCodeRemoved { table, column, removed })
            if table == "message" && column == "receiver_kind" && removed == &["co"]),
        "{:?}",
        refused.err()
    );
}

// Behaviour (#3972): a table missing a code that also differs in more than its codes, here a
// column added by hand-written `ALTER TABLE`, is not rebuilt: opening refuses it.
#[test]
fn a_store_with_fewer_codes_and_another_difference_is_refused() {
    let conn = Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry_without_controllers())
        .expect("old store")
        .into_connection();
    conn.execute_batch(
        "DROP VIEW message_view;
         ALTER TABLE message DROP COLUMN delivery_condition;
         ALTER TABLE message ADD COLUMN delivery_condition INTEGER REFERENCES strings (id);",
    )
    .expect("re-add a column");
    let refused = Store::open(conn, HOME, registry().expect("registry"));
    assert!(
        matches!(&refused, Err(StoreError::KindCodeDrift { table, column, missing })
            if table == "message" && column == "relation" && missing == &["dy"]),
        "{:?}",
        refused.err()
    );
}

// Behaviour (#3972): a coded column whose stored CHECK cannot be read is refused, never taken as
// accepting its kind's codes. The old table is rebuilt here without `receiver_kind`'s CHECK.
#[test]
fn a_store_with_an_unreadable_code_check_is_refused() {
    let conn = Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry_without_controllers())
        .expect("old store")
        .into_connection();
    let sql: String = conn.query_row("SELECT sql FROM sqlite_schema WHERE name = 'message'", [], |row| row.get(0)).expect("table");
    let start = sql.find(" CHECK (\"receiver_kind\" IN (").expect("receiver_kind CHECK");
    let end = start + sql[start..].find("))").expect("end of CHECK") + 2;
    let unchecked = format!("{}{}", &sql[..start], &sql[end..]).replace("CREATE TABLE \"message\"", "CREATE TABLE \"unchecked\"");
    assert_ne!(unchecked, sql);
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = OFF; DROP VIEW message_view; DROP TABLE message; {unchecked}; ALTER TABLE unchecked RENAME TO message;"
    ))
    .expect("table without the CHECK");
    let refused = Store::open(conn, HOME, registry().expect("registry"));
    assert!(
        matches!(&refused, Err(StoreError::KindCodeUnreadable { table, column }) if table == "message" && column == "receiver_kind"),
        "{:?}",
        refused.err()
    );
}

// Behaviour (#3972): widening checks the rebuilt table's foreign keys; a row whose identity is
// gone (written while foreign keys were off) fails the open, which rolls the rebuild back.
#[test]
fn widening_a_table_with_a_dangling_row_is_refused() {
    let mut old = Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry_without_controllers()).expect("old store");
    insert(&mut old, &message("first", ConsumerKind::AgentSession, Relation::System)).expect("session message");
    let conn = old.into_connection();
    conn.execute_batch("PRAGMA foreign_keys = OFF; UPDATE message SET id = id + 1000;").expect("dangle the row");
    let refused = Store::open(conn, HOME, registry().expect("registry"));
    assert!(matches!(&refused, Err(StoreError::KindCodeWidening { table, violations: 1 }) if table == "message"), "{:?}", refused.err());
}

// Behaviour (#3972 review): a table that both lacks a column opening adds with its default and
// lacks a code is widened, not refused: the rebuild adds the column with its default. Here the
// Convoy table predates `promise_floor` and the `ab` code (its guard is kept, see [`narrowed`]).
#[test]
fn a_table_missing_a_defaulted_column_and_a_code_is_widened() {
    use flotilla_engine_store::{convoy_registry, StoreLog};
    use flotilla_schema::Frozen;
    use flotilla_state_machines::{machines::convoy::*, Home};
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    let older = narrowed(convoy_registry().expect("registry"), &[("convoy", "state", "ab")]);
    let store = Arc::new(Mutex::new(Store::open(Connection::open_in_memory().expect("SQLite"), HOME, older).expect("old store")));
    let mut home = Home::<Convoy, _>::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("convoy home");
    let project: Address = "project:fleet".parse().expect("project");
    let owner = SeatName::new("governor", "");
    let seat = Seat {
        role: owner.role.clone(),
        seat: owner.seat.clone(),
        parent: None,
        supervised_by: project.clone(),
        retired: false,
        replacements: 0,
    };
    let request = ConvoyRequest {
        project: project.clone(),
        priority: Priority(0),
        kinds: Frozen::new(PromiseKinds::default()).expect("kinds"),
        seats: vec![seat],
        promises: BTreeMap::new(),
        duties: BTreeMap::from([(DutyId(0), Duty { owner, scope: DutyScope::Lifetime, terms: Terms::default() })]),
        quota: BTreeMap::new(),
    };
    home.create("standing", request, &MemoryConvoyWorld::default().with_project(&project), Timestamp(0)).expect("convoy");
    drop(home);
    let conn = Arc::try_unwrap(store).unwrap_or_else(|_| panic!("store still held")).into_inner().expect("store").into_connection();
    let (id, before): (i64, i64) =
        conn.query_row("SELECT id, version FROM convoy", [], |row| Ok((row.get(0)?, row.get(1)?))).expect("convoy row");
    let duties = count(&conn, "convoy_duties");
    conn.execute_batch("DROP VIEW convoy_view; ALTER TABLE convoy DROP COLUMN promise_floor;").expect("predate promise_floor");

    let store = Store::open(conn, HOME, convoy_registry().expect("registry")).expect("widened with the default");
    let conn = store.into_connection();
    let row: (i64, i64, i64) = conn
        .query_row("SELECT id, version, promise_floor FROM convoy", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("convoy row");
    assert_eq!(row, (id, before, 0));
    assert_eq!(count(&conn, "convoy_duties"), duties);
    let stored: String = conn.query_row("SELECT sql FROM sqlite_schema WHERE name = 'convoy'", [], |row| row.get(0)).expect("table");
    assert!(schema_ddl(&convoy_registry().expect("registry"), DdlMode::Create).contains(&format!("{stored};\n")), "{stored}");
}
