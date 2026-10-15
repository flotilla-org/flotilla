use super::*;

const BODY: &str = r#"{"metadata":{"labels":{"test":"yes"},"ownerReferences":[{"apiVersion":"flotilla.work/v1","kind":"Host","name":"owner"}]},"spec":{"repositories":[{"repo":"repo"}]}}"#;

fn seed(connection: &RusqliteConnection) {
    connection
        .execute("INSERT INTO resource_objects VALUES ('flotilla.work','v1','Project','flotilla','test',1,?1)", [BODY])
        .expect("seed local object");
    connection
        .execute_batch(
            "INSERT INTO replica_objects (origin_root,group_name,version,kind,namespace,name,body_json,last_synced_at)
        SELECT 'foreign',group_name,version,kind,namespace,name,body_json,'2026-10-14T00:00:00Z' FROM resource_objects;",
        )
        .expect("seed replica");
}

fn initialized(connection: &RusqliteConnection) -> bool {
    connection.query_row("SELECT EXISTS(SELECT 1 FROM resource_query_projection)", [], |row| row.get(0)).expect("projection marker")
}

fn assert_projections(connection: &RusqliteConnection) {
    for table in ["resource_objects", "replica_objects"] {
        let count: i64 = connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).expect("object count");
        for relation in [
            format!("{table}_labels"),
            format!("{table}_owners"),
            format!(
                "{}_values",
                SqliteBackend::field_index(table, ResourceKind::of::<flotilla_resources::Project>(), "spec.repositories[].repo")
            ),
        ] {
            let projected: i64 =
                connection.query_row(&format!("SELECT count(*) FROM {relation}"), [], |row| row.get(0)).expect("projection count");
            assert_eq!(projected, count, "{relation} agrees with authoritative objects");
        }
    }
}

// Issue 3197: native writes preserve the marker, committed unconfigured writes
// invalidate it, and rolled-back writes invalidate nothing. A native write after
// an unconfigured write must never bless that writer's stale field projections.
#[hegel::test]
fn writer_interleavings_preserve_projection_validity(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Both object tables, all mutation kinds, both connection types and commit /
    // rollback, with empty sequences and repeated/no-op deletions included.
    let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
    // Keep shared-cache writes sequential: contention can yield SQLITE_LOCKED.
    // Use file-backed WAL connections if this scenario becomes concurrent.
    let uri = format!("file:query-projections-{}?mode=memory&cache=shared", uuid::Uuid::new_v4());
    let mut native = RusqliteConnection::open(&uri).expect("shared in-memory native connection");
    SqliteBackend::initialize_connection(&mut native, EventRetention::default()).expect("initialize");
    seed(&native);
    let mut foreign = RusqliteConnection::open(&uri).expect("unconfigured connection to same in-memory store");
    let mut clean = true;
    for _ in 0..steps {
        let configured = tc.draw(gs::booleans());
        let table = if tc.draw(gs::booleans()) { "resource_objects" } else { "replica_objects" };
        let operation = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let rollback = tc.draw(gs::booleans());
        let connection = if configured { &mut native } else { &mut foreign };
        let tx = connection.transaction().expect("mutation transaction");
        let affected = match operation {
            0 => tx.execute(&format!("UPDATE {table} SET body_json=body_json"), []).expect("unchanged-version update"),
            1 => tx.execute(&format!("DELETE FROM {table}"), []).expect("delete"),
            _ if table == "resource_objects" => tx
                .execute("INSERT OR REPLACE INTO resource_objects VALUES ('flotilla.work','v1','Project','flotilla','test',1,?1)", [BODY])
                .expect("insert local"),
            _ => tx
                .execute(
                    "INSERT OR REPLACE INTO replica_objects (origin_root,group_name,version,kind,namespace,name,body_json,last_synced_at)
                VALUES ('foreign','flotilla.work','v1','Project','flotilla','test',?1,'2026-10-14T00:00:00Z')",
                    [BODY],
                )
                .expect("insert replica"),
        };
        if rollback {
            tx.rollback().expect("rollback");
        } else {
            tx.commit().expect("commit");
            clean &= configured || affected == 0;
        }
        assert_eq!(initialized(&native), clean, "marker reflects every committed writer");
        let proofs: i64 = native.query_row("SELECT count(*) FROM resource_query_native_write", [], |row| row.get(0)).expect("write proofs");
        assert_eq!(proofs, 0, "write proof cannot escape the object mutation");
        if clean {
            assert_projections(&native);
        }
    }
    SqliteBackend::initialize_query_indexes(&mut native).expect("repair after interleaving");
    assert!(initialized(&native));
    assert_projections(&native);
    let before = native.total_changes();
    SqliteBackend::initialize_query_indexes(&mut native).expect("steady-state reopen");
    assert_eq!(native.total_changes(), before, "successful repair is not repeated");
}

// Issue 3197: a changed projection schema repairs labels, owners and fields
// together, publishes its marker only after success, and preserves object bodies.
#[test]
fn schema_change_repairs_atomically_and_retries_after_failure() {
    let mut connection = RusqliteConnection::open_in_memory().expect("connection");
    SqliteBackend::initialize_connection(&mut connection, EventRetention::default()).expect("initialize");
    seed(&connection);
    let field = format!(
        "{}_values",
        SqliteBackend::field_index("replica_objects", ResourceKind::of::<flotilla_resources::Project>(), "spec.repositories[].repo")
    );
    connection
        .execute_batch(&format!(
            "UPDATE resource_query_projection SET schema='old';
        DELETE FROM resource_objects_labels; DELETE FROM resource_objects_owners;
        DELETE FROM replica_objects_labels; DELETE FROM replica_objects_owners; DELETE FROM {field};
        CREATE TEMP TRIGGER reject_repair BEFORE INSERT ON {field} BEGIN SELECT RAISE(ABORT,'repair failure'); END;"
        ))
        .expect("stale schema and failed field repair");
    assert!(SqliteBackend::initialize_query_indexes(&mut connection).is_err());
    let schema: String =
        connection.query_row("SELECT schema FROM resource_query_projection", [], |row| row.get(0)).expect("marker after failure");
    assert_eq!(schema, "old", "failed repair cannot publish the new schema");
    let count: i64 =
        connection.query_row("SELECT count(*) FROM resource_objects_labels", [], |row| row.get(0)).expect("labels after failure");
    assert_eq!(count, 0, "field repair failure rolls back metadata repair too");
    connection.execute_batch("DROP TRIGGER temp.reject_repair").expect("allow repair");
    SqliteBackend::initialize_query_indexes(&mut connection).expect("retry repair");
    assert_projections(&connection);
    for table in ["resource_objects", "replica_objects"] {
        let body: String = connection.query_row(&format!("SELECT body_json FROM {table}"), [], |row| row.get(0)).expect("body");
        assert_eq!(body, BODY, "repair preserves stored records");
    }
    let before = connection.total_changes();
    SqliteBackend::initialize_query_indexes(&mut connection).expect("reopen");
    assert_eq!(connection.total_changes(), before);
}

// Issue 3197: actual sync and async file opens are write-free after bootstrap,
// including persistent schema, not just derived row counts.
#[tokio::test]
async fn native_file_reopens_do_not_commit_writes() {
    for populated in [false, true] {
        let directory = tempfile::tempdir().expect("store directory");
        let path = directory.path().join("resources.sqlite");
        let native = SqliteBackend::open(&path).expect("bootstrap");
        if populated {
            native
                .call("seed projections", |connection| {
                    seed(connection);
                    Ok::<_, ResourceError>(())
                })
                .await
                .expect("seed");
        }
        let observer = RusqliteConnection::open(&path).expect("observer");
        let version = || observer.query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0)).expect("data version");
        let before = version();
        for asynchronous in [false, true, true, false] {
            let reopened = if asynchronous {
                SqliteBackend::open_async(&path).await.expect("async reopen")
            } else {
                SqliteBackend::open(&path).expect("sync reopen")
            };
            assert_eq!(version(), before, "native reopen commits no persisted changes");
            drop(reopened);
        }
    }
}

// A failed implicit object statement rolls back its native proof and projection
// changes, including earlier rows of a multi-row statement, without an explicit
// transaction. Subsequent native writes still maintain the clean marker.
#[test]
fn failed_native_statement_preserves_marker_and_projections() {
    let mut connection = RusqliteConnection::open_in_memory().expect("connection");
    SqliteBackend::initialize_connection(&mut connection, EventRetention::default()).expect("initialize");
    seed(&connection);
    let field = format!(
        "{}_values",
        SqliteBackend::field_index("resource_objects", ResourceKind::of::<flotilla_resources::Project>(), "spec.repositories[].repo")
    );
    connection
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER reject_native_projection BEFORE INSERT ON {field}
        WHEN NEW.name='reject' BEGIN SELECT RAISE(ABORT,'projection constraint'); END;"
        ))
        .expect("reject second inserted row");
    let error = connection.execute(
        "INSERT INTO resource_objects VALUES
        ('flotilla.work','v1','Project','flotilla','first',1,?1),
        ('flotilla.work','v1','Project','flotilla','reject',1,?1)",
        [BODY],
    );
    let error = error.expect_err("native multi-row statement fails during field projection");
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(_, Some(ref message)) if message == "projection constraint"),
        "the injected field constraint causes the failure"
    );
    assert!(connection.is_autocommit(), "SQLite owns the implicit transaction");
    assert!(initialized(&connection), "failed native write preserves the clean marker");
    let proofs: i64 = connection.query_row("SELECT count(*) FROM resource_query_native_write", [], |row| row.get(0)).expect("proof count");
    assert_eq!(proofs, 0, "failed statement leaves no write proof");
    let inserted: i64 = connection
        .query_row("SELECT count(*) FROM resource_objects WHERE name IN ('first','reject')", [], |row| row.get(0))
        .expect("rows after failure");
    assert_eq!(inserted, 0, "every row of the failed statement rolls back");
    assert_projections(&connection);
    connection.execute("UPDATE resource_objects SET body_json=body_json", []).expect("subsequent native write");
    assert!(initialized(&connection));
    assert_projections(&connection);
}

// A projection-version change upgrades persisted invalidation logic as well as
// data. An old no-op dirty trigger must not survive the schema repair.
#[test]
fn projection_schema_change_upgrades_persistent_invalidation_triggers() {
    // Shared-cache connections are used sequentially; a concurrent version
    // needs file-backed WAL connections to avoid table-lock contention.
    let uri = format!("file:query-trigger-upgrade-{}?mode=memory&cache=shared", uuid::Uuid::new_v4());
    let mut native = RusqliteConnection::open(&uri).expect("native connection");
    SqliteBackend::initialize_connection(&mut native, EventRetention::default()).expect("initialize");
    seed(&native);
    native
        .execute_batch(
            "DROP TRIGGER main.resource_objects_query_dirty_UPDATE;
        CREATE TRIGGER resource_objects_query_dirty_UPDATE AFTER UPDATE ON resource_objects BEGIN SELECT 1; END;
        UPDATE resource_query_projection SET schema='old-projection-schema';",
        )
        .expect("old invalidation schema");
    SqliteBackend::initialize_query_indexes(&mut native).expect("upgrade projection schema");
    assert!(initialized(&native));
    let foreign = RusqliteConnection::open(&uri).expect("unconfigured writer");
    foreign.execute("UPDATE resource_objects SET body_json=body_json", []).expect("foreign mutation");
    assert!(!initialized(&native), "schema repair replaces old invalidation trigger logic");
}

// Issue 3197: native reopen avoids all row rewrites, even uncommitted ones.
// File-backed data_version separately checks for persistent commits.
#[test]
fn native_reopen_does_not_rewrite_query_projections() {
    for populated in [false, true] {
        let mut connection = RusqliteConnection::open_in_memory().expect("connection");
        SqliteBackend::initialize_connection(&mut connection, EventRetention::default()).expect("initialize");
        if populated {
            seed(&connection);
        }
        let before = connection.total_changes();
        SqliteBackend::initialize_connection(&mut connection, EventRetention::default()).expect("reopen");
        assert_eq!(connection.total_changes(), before, "steady-state open must not rewrite derived rows");
    }
}
