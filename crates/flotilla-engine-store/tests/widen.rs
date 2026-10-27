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

/// The engine registry as a build before the Controller consumer rendered it: every
/// `receiver_kind` column without `co`.
fn registry_without_controllers() -> KindRegistry {
    let mut kinds = KindRegistry::new();
    for set in registry().expect("registry").iter() {
        let mut set = set.clone();
        for column in set.tables.iter_mut().flat_map(|table| table.columns.iter_mut()).filter(|column| column.name == "receiver_kind") {
            let ColumnType::Code(codes) = &mut column.ty else { panic!("receiver_kind is coded") };
            codes.retain(|code| code != "co");
            column.categories.retain(|(code, _)| code != "co");
        }
        kinds.register_table_set(set).expect("narrowed registry");
    }
    kinds
}

fn message(id: &str, kind: ConsumerKind) -> Resource<Message> {
    let inbox: Address = "project:fleet/convoy:demo/role:coder".parse().expect("inbox");
    let name = Address::message(&inbox, id).expect("message address").as_str().to_owned();
    let spec = MessageSpec {
        references: vec![MessageReference { address: inbox.clone(), revision: "r1".into(), subject: true }],
        ..MessageSpec::new(
            "project:fleet/role:turn-rules".parse().expect("sender"),
            inbox,
            Relation::System,
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

// Behaviour (#3972): a store whose message table was created before `receiver_kind` had `co`
// opens under this build, which rebuilds the table with the wider CHECK: every message and its
// references (a child table whose foreign key cascades from it) survive, the table is the one
// this build renders, and a message for a Controller is written.
#[test]
fn a_store_with_fewer_codes_opens_wider_and_keeps_its_rows() {
    let mut old = Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry_without_controllers()).expect("old store");
    let kept: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|id| {
            let id = insert(&mut old, &message(id, ConsumerKind::AgentSession)).expect("session message");
            (id, old.read().expect("read").load::<Resource<Message>>(id).expect("load").expect("stored").value)
        })
        .collect();
    assert!(insert(&mut old, &message("early", ConsumerKind::Controller)).is_err(), "the old table refuses `co`");
    let conn = old.into_connection();
    let (messages, references) = (count(&conn, "message"), count(&conn, "message_references"));
    assert_eq!((messages, references), (2, 2));

    let mut store = Store::open(conn, HOME, registry().expect("registry")).expect("opens with wider codes");
    for (id, value) in &kept {
        let loaded = store.read().expect("read").load::<Resource<Message>>(*id).expect("load").expect("kept");
        assert_eq!(&loaded.value, value);
    }
    let controller = message("controller", ConsumerKind::Controller);
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
    insert(&mut store, &message("controller", ConsumerKind::Controller)).expect("controller message");
    let refused = Store::open(store.into_connection(), HOME, registry_without_controllers());
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
            if table == "message" && column == "receiver_kind" && missing == &["co"]),
        "{:?}",
        refused.err()
    );
}
