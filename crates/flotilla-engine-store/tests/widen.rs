//! Opening a store created by a build with fewer codes (#3972): the engine's real Message
//! tables, rendered from a registry whose `receiver_kind` predates the Service consumer. Also a
//! store whose table predates a column (ADR 0047 N→N+1): one the table lacks.

use flotilla_engine_store::registry;
use flotilla_schema::{Address, ColumnType, KindRegistry, NewChange, State, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store, StoreError, StoreHandle};
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

/// `kinds` with `table`'s `column` declaring a previous-generation default of 0, as a kind's
/// `STORED_DEFAULTS` would, so opening may add it to a table that lacks it.
fn defaulted(kinds: KindRegistry, table: &str, column: &str) -> KindRegistry {
    let mut widened = KindRegistry::new();
    for set in kinds.iter() {
        let mut set = set.clone();
        for def in set.tables.iter_mut().filter(|def| def.name == table).flat_map(|def| def.columns.iter_mut()) {
            if def.name == column {
                def.properties.insert("previous_generation_default".into(), 0.into());
            }
        }
        widened.register_table_set(set).expect("defaulted registry");
    }
    widened
}

/// The engine registry before the Service consumer and the Dependency relation.
fn registry_without_services() -> KindRegistry {
    narrowed(registry().expect("registry"), &[("message", "receiver_kind", "sv"), ("message", "relation", "dy")])
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
    Resource::<Message>::stored(
        &name,
        spec,
        State::new(MessageState::Awaiting { receiver, delivered_at: Timestamp(1) }, Timestamp(1)),
        Version(0),
    )
}

fn insert(store: &mut Store, value: &Resource<Message>) -> Result<flotilla_schema_sqlite::ResourceId, StoreError> {
    let mut write = store.write()?;
    let stamp = write.insert(value.name(), value, &NewChange::new("test", Timestamp(1)))?;
    write.commit()?;
    Ok(stamp.id)
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).expect("count")
}

// Behaviour (#3972): a store whose message table was created before `receiver_kind` had `co` and
// `relation` had `dy` opens under this build, which rebuilds the table with both wider CHECKs: every message and its
// references (a child table whose foreign key cascades from it) survive, the table is the one
// this build renders, and a Dependency message for a Service is written.
#[test]
fn a_store_with_fewer_codes_opens_wider_and_keeps_its_rows() {
    let mut old = Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry_without_services()).expect("old store");
    let kept: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|id| {
            let id = insert(&mut old, &message(id, ConsumerKind::AgentSession, Relation::System)).expect("session message");
            (id, old.read().expect("read").load::<Resource<Message>>(id).expect("load").expect("stored").value)
        })
        .collect();
    assert!(insert(&mut old, &message("early", ConsumerKind::Service, Relation::System)).is_err(), "the old table refuses `sv`");
    let conn = old.into_connection();
    let (messages, references) = (count(&conn, "message"), count(&conn, "message_references"));
    assert_eq!((messages, references), (2, 2));

    let mut store = Store::open(conn, HOME, registry().expect("registry")).expect("opens with wider codes");
    for (id, value) in &kept {
        let loaded = store.read().expect("read").load::<Resource<Message>>(*id).expect("load").expect("kept");
        assert_eq!(&loaded.value, value);
    }
    let service = message("service", ConsumerKind::Service, Relation::Dependency);
    let id = insert(&mut store, &service).expect("a Service's message is written");
    let written = store.read().expect("read").load::<Resource<Message>>(id).expect("load").expect("written").value;
    assert_eq!(written.state(), service.state());

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
    insert(&mut store, &message("service", ConsumerKind::Service, Relation::System)).expect("service message");
    let refused =
        Store::open(store.into_connection(), HOME, narrowed(registry().expect("registry"), &[("message", "receiver_kind", "sv")]));
    assert!(
        matches!(&refused, Err(StoreError::KindCodeRemoved { table, column, removed })
            if table == "message" && column == "receiver_kind" && removed == &["sv"]),
        "{:?}",
        refused.err()
    );
}

// Behaviour (#3972): a table missing a code that also differs in more than its codes, here a
// column added by hand-written `ALTER TABLE`, is not rebuilt: opening refuses it.
#[test]
fn a_store_with_fewer_codes_and_another_difference_is_refused() {
    let conn =
        Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry_without_services()).expect("old store").into_connection();
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
    let conn =
        Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry_without_services()).expect("old store").into_connection();
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
    let mut old = Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry_without_services()).expect("old store");
    insert(&mut old, &message("first", ConsumerKind::AgentSession, Relation::System)).expect("session message");
    let conn = old.into_connection();
    conn.execute_batch("PRAGMA foreign_keys = OFF; UPDATE message SET id = id + 1000;").expect("dangle the row");
    let refused = Store::open(conn, HOME, registry().expect("registry"));
    assert!(matches!(&refused, Err(StoreError::KindCodeWidening { table, violations: 1 }) if table == "message"), "{:?}", refused.err());
}

// Behaviour (#3972 review): a table that both lacks a column opening adds with its default and
// lacks a code is widened, not refused: the rebuild adds the column with its default. Here the
// Convoy table predates the `ab` code (its guard is kept, see [`narrowed`]) and a root column
// given a previous-generation default, since Convoy no longer declares a defaulted root column.
#[test]
fn a_table_missing_a_defaulted_column_and_a_code_is_widened() {
    use flotilla_engine_store::{convoy_registry, StoreLog};
    use flotilla_schema::Frozen;
    use flotilla_state_machines::{machines::convoy::*, Home};
    use std::{collections::BTreeMap, sync::Arc};

    let current = defaulted(convoy_registry().expect("registry"), "convoy", "priority");
    let older = narrowed(current.clone(), &[("convoy", "state", "ab")]);
    let store = Arc::new(StoreHandle::new(Store::open(Connection::open_in_memory().expect("SQLite"), HOME, older).expect("old store")));
    let mut home = Home::<Convoy, _>::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("convoy home");
    let project: Address = "project:fleet".parse().expect("project");
    let owner = SeatName::new("governor", "");
    let seat = Seat {
        role: owner.role.clone(),
        seat: owner.seat.clone(),
        parent: None,
        supervised_by: project.clone(),
        failed: None,
        retired: false,
        replacements: 0,
    };
    let request = ConvoyRequest {
        role: String::new(),
        launch_of: "c".into(),
        workflow_ref: "w".into(),
        admission: Frozen::new(AdmissionBytes::new(Vec::new())).expect("freeze"),
        project: project.clone(),
        priority: Priority(0),
        kinds: Frozen::new(PromiseKinds::default()).expect("kinds"),
        seats: vec![seat],
        promises: BTreeMap::new(),
        duties: BTreeMap::from([(DutyId(0), Duty { owner, scope: DutyScope::Lifetime, terms: Terms::default() })]),
    };
    home.create("standing", request, &MemoryConvoyWorld::default().with_project(&project), Timestamp(0)).expect("convoy");
    drop(home);
    let conn = Arc::try_unwrap(store).unwrap_or_else(|_| panic!("store still held")).into_inner().into_connection();
    let (id, before): (i64, i64) =
        conn.query_row("SELECT id, version FROM convoy", [], |row| Ok((row.get(0)?, row.get(1)?))).expect("convoy row");
    let duties = count(&conn, "convoy_duties");
    conn.execute_batch("DROP VIEW convoy_view; ALTER TABLE convoy DROP COLUMN priority;").expect("predate priority");

    let store = Store::open(conn, HOME, current.clone()).expect("widened with the default");
    let conn = store.into_connection();
    let row: (i64, i64, i64) = conn
        .query_row("SELECT id, version, priority FROM convoy", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("convoy row");
    assert_eq!(row, (id, before, 0));
    assert_eq!(count(&conn, "convoy_duties"), duties);
    let stored: String = conn.query_row("SELECT sql FROM sqlite_schema WHERE name = 'convoy'", [], |row| row.get(0)).expect("table");
    assert!(schema_ddl(&current, DdlMode::Create).contains(&format!("{stored};\n")), "{stored}");
}

// Behaviour (ADR 0047 N→N+1, #3108): the previous generation stored each convoy's quota in
// `convoy_quota` (with its view and the Readmit edge's code row). A store holding one, with a
// quota row, opens under this build, which reads the convoy without it; dropping the retired
// tables discards the rows, and the store reopens to this build's schema. Remove this test with
// `drop_retired_tables`, one fleet roll after the quota's removal ships.
#[test]
fn a_store_holding_convoy_quotas_opens_and_drops_them() {
    use flotilla_engine_store::{drop_retired_tables, StoreLog};
    use flotilla_schema::{Frozen, Kind};
    use flotilla_state_machines::{machines::convoy::*, Home};
    use std::{collections::BTreeMap, sync::Arc};

    let store = Arc::new(StoreHandle::new(
        Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry().expect("registry")).expect("store"),
    ));
    let mut home = Home::<Convoy, _>::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("convoy home");
    let project: Address = "project:fleet".parse().expect("project");
    let owner = SeatName::new("governor", "");
    let seat = Seat {
        role: owner.role.clone(),
        seat: owner.seat.clone(),
        parent: None,
        supervised_by: project.clone(),
        failed: None,
        retired: false,
        replacements: 0,
    };
    let request = ConvoyRequest {
        role: String::new(),
        launch_of: "c".into(),
        workflow_ref: "w".into(),
        admission: Frozen::new(AdmissionBytes::new(Vec::new())).expect("freeze"),
        project: project.clone(),
        priority: Priority(0),
        kinds: Frozen::new(PromiseKinds::default()).expect("kinds"),
        seats: vec![seat],
        promises: BTreeMap::new(),
        duties: BTreeMap::from([(DutyId(0), Duty { owner, scope: DutyScope::Lifetime, terms: Terms::default() })]),
    };
    let created = home
        .create("standing", request, &MemoryConvoyWorld::default().with_project(&project), Timestamp(0))
        .expect("convoy")
        .resource
        .clone();
    drop(home);
    let conn = Arc::try_unwrap(store).unwrap_or_else(|_| panic!("store still held")).into_inner().into_connection();
    // The previous generation's quota table, view and code row, as it rendered them, and a quota.
    conn.execute_batch(
        r#"CREATE TABLE "convoy_quota" (
  "convoy" INTEGER NOT NULL,
  "key" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "value" INTEGER NOT NULL,
  PRIMARY KEY ("convoy", "key"),
  FOREIGN KEY ("convoy") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE VIEW "convoy_quota_view" AS SELECT
  "convoy_quota"."convoy",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy_quota"."key") AS "key",
  "convoy_quota"."value"
FROM "convoy_quota";
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES ('cv', 'log:edge', 'ra', 'Readmit');
INSERT INTO "strings" ("domain", "s") VALUES ('vc', 'standard');
INSERT INTO "convoy_quota" SELECT "convoy"."id", "strings"."id", 2 FROM "convoy", "strings" WHERE "strings"."s" = 'standard';"#,
    )
    .expect("the previous generation's quota");
    assert_eq!(count(&conn, "convoy_quota"), 1);
    let schema = |conn: &Connection| -> i64 {
        conn.query_row("SELECT count(*) FROM sqlite_schema WHERE name IN ('convoy_quota', 'convoy_quota_view')", [], |row| row.get(0))
            .expect("schema")
    };
    let load = |store: &mut Store| {
        let mut read = store.read().expect("read");
        let id = read.resolve(Resource::<Convoy>::CODE, created.name(), HOME).expect("resolve").expect("stored");
        let loaded = read.load::<Resource<Convoy>>(id).expect("load").expect("kept");
        loaded.value.with_identity(&loaded.name, loaded.version)
    };

    let mut accepted = Store::open(conn, HOME, registry().expect("registry")).expect("opens with the quota table present");
    assert_eq!(load(&mut accepted), created, "read without the quota");
    let conn = accepted.into_connection();
    assert_eq!(schema(&conn), 2, "opening leaves a table no kind declares");

    drop_retired_tables(&conn).expect("dropped");
    assert_eq!(schema(&conn), 0);
    let mut store = Store::open(conn, HOME, registry().expect("registry")).expect("reopens");
    assert_eq!(load(&mut store), created);
}

/// The store an older build left: one `message` written by this build, then `column` dropped
/// from its table, so the row reads as a build before the column stored it.
fn predating(kinds: KindRegistry, column: &str) -> (Connection, Resource<Message>, flotilla_schema_sqlite::ResourceId) {
    let mut store = Store::open(Connection::open_in_memory().expect("SQLite"), HOME, kinds).expect("store");
    let id = insert(&mut store, &message("earlier", ConsumerKind::AgentSession, Relation::System)).expect("written");
    let earlier = store.read().expect("read").load::<Resource<Message>>(id).expect("load").expect("written").value;
    let conn = store.into_connection();
    conn.execute_batch(&format!("DROP VIEW message_view; ALTER TABLE message DROP COLUMN {column};")).expect("predate the column");
    (conn, earlier, id)
}

// Behaviour (ADR 0047 N→N+1): a store whose message table predates a nullable column, here
// `delivery_condition`, opens under this build, which adds it as NULL: the message the older
// build wrote reads it as absent, and this build writes it.
#[test]
fn a_store_missing_a_nullable_column_gains_it_at_open() {
    let (conn, earlier, earlier_id) = predating(registry().expect("registry"), "delivery_condition");
    let mut store = Store::open(conn, HOME, registry().expect("registry")).expect("opens, adding the column");
    let loaded = store.read().expect("read").load::<Resource<Message>>(earlier_id).expect("load").expect("kept").value;
    assert_eq!(loaded, earlier);
    assert_eq!(loaded.spec().delivery_condition, None);
    let later = message("later", ConsumerKind::AgentSession, Relation::System);
    let condition = "project:fleet/convoy:demo/role:coder .phase == Done".parse().expect("a condition");
    let later = later.clone().with_spec(MessageSpec { delivery_condition: Some(condition), ..later.spec().clone() });
    let later_id = insert(&mut store, &later).expect("written with the column");
    let loaded = store.read().expect("read").load::<Resource<Message>>(later_id).expect("load").expect("written").value;
    assert!(loaded.spec().delivery_condition.is_some());
    assert_eq!(loaded.spec(), later.spec());

    let conn = store.into_connection();
    let held: Vec<bool> = conn
        .prepare("SELECT delivery_condition IS NOT NULL FROM message ORDER BY id")
        .expect("statement")
        .query_map([], |row| row.get(0))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("conditions");
    assert_eq!(held, [false, true]);
    assert_eq!(table_sql(&conn, "message"), rendered(&registry().expect("registry"), "message"), "rebuilt, not altered");
    Store::open(conn, HOME, registry().expect("registry")).expect("reopens unchanged");
}

/// `table`'s stored `CREATE TABLE` text.
fn table_sql(conn: &Connection, table: &str) -> String {
    conn.query_row("SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1", [table], |row| row.get(0)).expect("table")
}

/// `table`'s `CREATE TABLE` text as `kinds` renders it.
fn rendered(kinds: &KindRegistry, table: &str) -> String {
    let ddl = schema_ddl(kinds, DdlMode::Create);
    let start = ddl.find(&format!("CREATE TABLE \"{table}\" (")).expect("rendered table");
    let end = start + ddl[start..].find(";\n").expect("end of table");
    ddl[start..end].to_owned()
}

// Behaviour (ADR 0047 N→N+1): an earlier build's opening added a missing column with `ALTER TABLE
// … ADD COLUMN`, which left text it does not render and dropped the table's CHECKs. Opening
// rebuilds that table once to the text it renders, keeping every row and its children, and a
// later build's new codes then widen it instead of being refused as drift.
#[test]
fn a_table_an_earlier_open_altered_is_rebuilt_and_later_widens() {
    let (conn, earlier, earlier_id) = predating(registry_without_services(), "delivery_condition");
    // The statement an earlier opening ran for the missing column.
    conn.execute_batch(r#"ALTER TABLE "message" ADD COLUMN "delivery_condition" INTEGER REFERENCES "strings" ("id");"#)
        .expect("the earlier opening's ADD COLUMN");
    assert_ne!(table_sql(&conn, "message"), rendered(&registry_without_services(), "message"));
    let references = count(&conn, "message_references");

    let store = Store::open(conn, HOME, registry_without_services()).expect("rebuilt to its text");
    let conn = store.into_connection();
    assert_eq!(table_sql(&conn, "message"), rendered(&registry_without_services(), "message"));
    assert_eq!((count(&conn, "message"), count(&conn, "message_references")), (1, references));

    let mut store = Store::open(conn, HOME, registry().expect("registry")).expect("a later build's codes widen it");
    let loaded = store.read().expect("read").load::<Resource<Message>>(earlier_id).expect("load").expect("kept").value;
    assert_eq!(loaded, earlier);
    insert(&mut store, &message("service", ConsumerKind::Service, Relation::Dependency)).expect("a Service's message is written");
    assert_eq!(table_sql(&store.into_connection(), "message"), rendered(&registry().expect("registry"), "message"));
}

// Behaviour (ADR 0047 N→N+1): a table that both lacks a nullable column and lacks codes is
// widened, not refused: the rebuild adds the column as NULL and the codes, and keeps the row.
#[test]
fn a_table_missing_a_nullable_column_and_a_code_is_widened() {
    let (conn, earlier, earlier_id) = predating(registry_without_services(), "delivery_condition");
    let mut store = Store::open(conn, HOME, registry().expect("registry")).expect("widened, adding the column");
    let loaded = store.read().expect("read").load::<Resource<Message>>(earlier_id).expect("load").expect("kept").value;
    assert_eq!(loaded, earlier);
    insert(&mut store, &message("service", ConsumerKind::Service, Relation::Dependency)).expect("a Service's message is written");
    let conn = store.into_connection();
    let stored: String = conn.query_row("SELECT sql FROM sqlite_schema WHERE name = 'message'", [], |row| row.get(0)).expect("table");
    assert!(schema_ddl(&registry().expect("registry"), DdlMode::Create).contains(&format!("{stored};\n")), "{stored}");
}

// Behaviour (ADR 0047 N→N+1): a store whose message table predates a NOT NULL column with no
// previous-generation default, here `unbound_after`, is refused at open: the older build's rows
// have no value for it.
#[test]
fn a_store_missing_a_required_column_is_refused() {
    let (conn, _, _) = predating(registry().expect("registry"), "unbound_after");
    let refused = Store::open(conn, HOME, registry().expect("registry"));
    assert!(
        matches!(&refused, Err(StoreError::KindTableDrift { table, found, .. })
            if table == "message" && !found.iter().any(|column| column == "unbound_after")),
        "{:?}",
        refused.err()
    );
}

// Behaviour (#4438, ADR 0047 N→N+1): a store whose `agent_session` table predates a session's
// convoy, vessel and role opens under this build, which adds them: the session the earlier build
// wrote reads back with none of them, and a new session is written with all three.
#[test]
fn a_session_table_predating_its_convoy_gains_it_at_open() {
    use std::sync::Arc;

    use flotilla_engine_store::{open_home, StoreLog};
    use flotilla_quint_testkit::agent_session::agent_session_request;
    use flotilla_schema::Ref;
    use flotilla_state_machines::{
        machines::agent_session::{AgentSession, AgentSessionRequest, AgentSessionSpec, MemoryAgentSessionWorld},
        Home,
    };

    let open = |conn: Connection| Arc::new(StoreHandle::new(Store::open(conn, HOME, registry().expect("registry")).expect("store")));
    let store = open(Connection::open_in_memory().expect("SQLite"));
    let mut sessions = Home::<AgentSession, _>::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("session home");
    sessions.create("earlier", agent_session_request(), &MemoryAgentSessionWorld::default(), Timestamp(1)).expect("session");
    drop(sessions);
    let conn = Arc::try_unwrap(store).unwrap_or_else(|_| panic!("store still held")).into_inner().into_connection();
    // SQLite drops no referencing column, so the earlier table is rebuilt without them.
    let before: String = rendered(&registry().expect("registry"), "agent_session")
        .replace("\"agent_session\" (", "\"agent_session_before\" (")
        .lines()
        .filter(|line| !["  \"convoy\"", "  \"vessel\"", "  \"role\""].iter().any(|column| line.starts_with(column)))
        .collect::<Vec<_>>()
        .join("\n");
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = OFF;
         DROP VIEW agent_session_view; DROP INDEX agent_session_by_convoy; DROP INDEX agent_session_by_vessel;
         {before};
         INSERT INTO agent_session_before SELECT id, version, state, state_since, channel, activity, activity_since, activity_cause, reason
           FROM agent_session;
         DROP TABLE agent_session; ALTER TABLE agent_session_before RENAME TO agent_session;
         PRAGMA foreign_keys = ON;"
    ))
    .expect("the earlier build's table");

    let mut sessions = open_home::<AgentSession>(StoreLog::new(open(conn))).expect("restored sessions");
    assert_eq!(sessions.get("earlier").expect("kept").spec(), &AgentSessionSpec::default(), "an earlier session names no convoy");
    let request = AgentSessionRequest { vessel: Some(Ref::new("convoy-work#2", HOME)), ..agent_session_request() };
    sessions.create("later", request.clone(), &MemoryAgentSessionWorld::default(), Timestamp(2)).expect("a session with its convoy");
    let spec = sessions.get("later").expect("written").spec().clone();
    assert_eq!((spec.convoy, spec.vessel, spec.role), (Some(request.convoy), request.vessel, Some(request.role)));
}
