//! The change log: every write's row operations, and `apply` of another home's changes.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use flotilla_schema::{Hash, Kind, KindCode, RowChange, RowOp, Value};
use flotilla_schema_sqlite::{Applied, Malformed, ResourceId, Snapshot, Stamp, Store, StoreError, Write};
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;

use crate::fixtures::{
    convoy::{workflow, ConvoyRepository},
    registry, store, Convoy, Fixture, Message, Pick, TerminalSession, LOCAL,
};

const REPLICA: &str = "replica";

fn replica() -> Store {
    Store::open(Connection::open_in_memory().expect("in-memory database"), REPLICA, registry()).expect("open replica")
}

/// The one change a write logged after sequence `before`.
fn logged(write: &mut Write<'_>, before: u64) -> RowChange {
    let mut changes = write.changes_since(LOCAL, before, 10).expect("changes");
    assert_eq!(changes.len(), 1, "one log row per write: {changes:?}");
    changes.remove(0)
}

/// One generated write to slot `live` of kind `K`: insert when absent, else an allowed update
/// (possibly changing no row) or a delete. Checks that the write logged exactly one change at
/// its version naming the resource, or nothing when it changed no row.
fn step<K: Fixture>(tc: &TestCase, write: &mut Write<'_>, live: &mut Option<Stamp>, name: &str, last: &mut u64) {
    let change = match *live {
        None => {
            let stamp = write.insert(name, &K::generate(tc, Pick::Draw)).expect("insert");
            *live = Some(stamp);
            let change = logged(write, *last);
            assert!(matches!(&change.ops[..], [.., RowOp::Insert { .. }]));
            assert_eq!(change.home_seq, stamp.version);
            change
        }
        Some(stamp) if tc.draw(gs::booleans()) => {
            let current = write.load::<K>(stamp.id).expect("load").expect("present");
            let next = write.update(&current, &current.value.evolve(tc, Pick::Draw)).expect("update");
            *live = Some(next);
            if next == stamp {
                assert_eq!(write.changes_since(LOCAL, *last, 10).expect("changes"), [], "a no-op logs nothing");
                return;
            }
            let change = logged(write, *last);
            assert_eq!(change.home_seq, next.version);
            change
        }
        Some(stamp) => {
            write.delete::<K>(stamp).expect("delete");
            *live = None;
            let change = logged(write, *last);
            assert_eq!(change.ops, [RowOp::DeleteResource]);
            change
        }
    };
    assert_eq!((change.kind, change.name.as_str(), change.home.as_str()), (K::CODE, name, LOCAL));
    assert!(change.home_seq > *last);
    *last = change.home_seq;
}

/// Every slot's resource: kind code and name.
fn slots() -> Vec<(KindCode, String)> {
    [Message::CODE, Convoy::CODE, TerminalSession::CODE].into_iter().flat_map(|code| (0..2).map(move |n| (code, format!("r{n}")))).collect()
}

fn snapshot(store: &mut Store, code: KindCode, name: &str) -> Option<(ResourceId, Option<Snapshot>)> {
    let mut read = store.read().expect("read");
    let id = read.resolve(code, name, LOCAL).expect("resolve")?;
    Some((id, read.load_snapshot(id).expect("snapshot")))
}

/// Applies every change homed at `LOCAL` in `source`, paged three at a time in home order.
fn replay(source: &mut Store, target: &mut Store) -> Vec<Applied> {
    let mut results = Vec::new();
    let mut after = 0;
    loop {
        let page = source.read().expect("read").changes_since(LOCAL, after, 3).expect("page");
        let Some(last) = page.last() else { return results };
        after = last.home_seq;
        let mut write = target.write().expect("write");
        for change in &page {
            results.push(write.apply(change).expect("apply"));
        }
        write.commit().expect("commit");
    }
}

/// The local id of an interned string, reopening the store with its home.
fn string_id(store: Store, string: &str) -> (Store, i64) {
    let home = store.home().to_owned();
    let conn = store.into_connection();
    let id = conn.query_row("SELECT id FROM strings WHERE s = ?1", [string], |row| row.get(0)).expect("interned");
    (Store::open(conn, &home, registry()).expect("reopen"), id)
}

// Behaviour (DESIGN change log, completeness): every write appends exactly one change at its
// version (a no-op none), and replaying a store's log in home order through `apply` into a store
// with a different home reproduces every resource: equal snapshots (version, name, home, rows,
// objects) although the two databases number strings and identities differently. The target
// relays the same log and the same home sequence. Replaying again is idempotent: every change is
// a `Duplicate` and nothing moves. Writes span all three fixture kinds, two names each, with
// inserts, allowed updates (records replaced and appended, history appended), deletes and
// re-inserts, over several transactions.
#[hegel::test]
fn the_change_log_replays_into_another_store(tc: TestCase) {
    let mut source = store();
    let mut live: Vec<Option<Stamp>> = vec![None; slots().len()];
    let mut last = 0;
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(14));
    let mut write = source.write().expect("write");
    for _ in 0..steps {
        let slot = tc.draw(gs::integers::<usize>().min_value(0).max_value(slots().len() - 1));
        let (code, name) = &slots()[slot];
        match *code {
            Message::CODE => step::<Message>(&tc, &mut write, &mut live[slot], name, &mut last),
            Convoy::CODE => step::<Convoy>(&tc, &mut write, &mut live[slot], name, &mut last),
            _ => step::<TerminalSession>(&tc, &mut write, &mut live[slot], name, &mut last),
        }
        if tc.draw(gs::booleans()) {
            write.commit().expect("commit");
            write = source.write().expect("write");
        }
    }
    write.commit().expect("commit");

    // The target interns its own strings and identities first, so its local ids differ.
    let mut target = replica();
    let mut write = target.write().expect("write");
    write.insert("pre", &Message::sample()).expect("pre-existing local resource");
    write.commit().expect("commit");

    let results = replay(&mut source, &mut target);
    assert!(results.iter().all(|result| matches!(result, Applied::Applied(_))), "{results:?}");
    for (code, name) in slots() {
        let (left, right) = (snapshot(&mut source, code, &name), snapshot(&mut target, code, &name));
        assert_eq!(left.as_ref().map(|(_, snapshot)| snapshot), right.as_ref().map(|(_, snapshot)| snapshot), "{name} of kind {code}");
        if let (Some((left, _)), Some((right, _))) = (left, right) {
            assert_ne!(left, right, "identity ids differ across the databases");
        }
    }
    let (mut source_read, mut target_read) = (source.read().expect("read"), target.read().expect("read"));
    assert_eq!(source_read.last_sequence(LOCAL).expect("last"), last);
    assert_eq!(target_read.last_sequence(LOCAL).expect("last"), last, "apply raises the home's sequence");
    assert_eq!(target_read.changes_since(LOCAL, 0, 100).expect("log"), source_read.changes_since(LOCAL, 0, 100).expect("log"));
    drop((source_read, target_read));
    let (mut source, local_in_source) = string_id(source, LOCAL);
    let (mut target, local_in_target) = string_id(target, LOCAL);
    assert_ne!(local_in_source, local_in_target, "string ids differ across the databases");

    let before: Vec<_> = slots().iter().map(|(code, name)| snapshot(&mut target, *code, name)).collect();
    let again = replay(&mut source, &mut target);
    assert_eq!(again.len(), results.len());
    assert!(again.iter().all(|result| matches!(result, Applied::Duplicate(_))), "{again:?}");
    let after: Vec<_> = slots().iter().map(|(code, name)| snapshot(&mut target, *code, name)).collect();
    assert_eq!(before, after);
}

fn first_change(store: &mut Store, home: &str) -> RowChange {
    store.read().expect("read").changes_since(home, 0, 1).expect("log").remove(0)
}

// Behaviour (#3320 ownership): a generated write to a resource homed elsewhere is `NotHome`,
// while `apply` of that resource's change is accepted; `apply` refuses a change homed here
// (`OwnHome`) and a kind this store does not register (`UnknownKind`), writing nothing.
#[test]
fn apply_writes_other_homes_and_generated_writes_do_not() {
    let mut remote = Store::open(Connection::open_in_memory().expect("db"), "remote", registry()).expect("open remote");
    let mut write = remote.write().expect("write");
    let stamp = write.insert("far", &Message::sample()).expect("insert");
    let current = write.load::<Message>(stamp.id).expect("load").expect("present");
    write.update(&current, &Message { body: "edited".into(), ..Message::sample() }).expect("update");
    write.commit().expect("commit");
    let changes = remote.read().expect("read").changes_since("remote", 0, 10).expect("log");

    let mut local = store();
    let mut write = local.write().expect("write");
    for change in &changes {
        assert!(matches!(write.apply(change).expect("apply"), Applied::Applied(_)));
    }
    let id = write.resolve(Message::CODE, "far", "remote").expect("resolve").expect("present");
    let current = write.load::<Message>(id).expect("load").expect("present");
    assert_eq!((current.version, current.value.body.as_str()), (changes[1].home_seq, "edited"));
    let next = Message { body: "local edit".into(), ..current.value.clone() };
    assert!(matches!(write.update(&current, &next), Err(StoreError::NotHome { .. })));
    assert!(matches!(write.delete::<Message>(current.stamp()), Err(StoreError::NotHome { .. })));

    let own = RowChange { home: LOCAL.into(), name: "mine".into(), ..changes[0].clone() };
    assert!(matches!(write.apply(&own), Err(StoreError::OwnHome { home }) if home == LOCAL));
    assert_eq!(write.resolve(Message::CODE, "mine", LOCAL).expect("resolve"), None, "a refused apply writes nothing");
    let unknown = RowChange { kind: KindCode(999), ..changes[0].clone() };
    assert!(matches!(write.apply(&unknown), Err(StoreError::UnknownKind { code: KindCode(999) })));
    write.commit().expect("commit");
}

// Behaviour (DESIGN change log, self-contained changes): a change carries a `PutObject` for every
// object it sets (any object of an inserted row, a changed object column of an updated row), even
// one an earlier write already stored; an update leaving a frozen column alone carries none. So a
// resource's changes apply into a store that never saw another resource's writes. A change whose rows name an object it neither carries nor finds is refused
// with `MissingObject`, not a foreign-key failure, and writes nothing.
#[test]
fn every_change_carries_the_objects_its_rows_name() {
    let mut source = store();
    let mut write = source.write().expect("write");
    write.insert("a", &Convoy::sample(0)).expect("insert a");
    let b = write.insert("b", &Convoy::sample(0)).expect("insert b");
    let current = write.load::<Convoy>(b.id).expect("load").expect("present");
    let root = Convoy { instruction: Some("go".into()), ..Convoy::sample(0) };
    let b = write.update(&current, &root).expect("root update");
    let current = write.load::<Convoy>(b.id).expect("load").expect("present");
    let repository = ConvoyRepository {
        url: "u".into(),
        repo_key: "k".into(),
        source_ref: "s".into(),
        target_ref: "t".into(),
        workspace_slug: "w".into(),
    };
    let child = Convoy { repositories: vec![repository], ..root };
    let b = write.update(&current, &child).expect("child update");
    let current = write.load::<Convoy>(b.id).expect("load").expect("present");
    write.update(&current, &Convoy { workflow_snapshot: workflow(1), ..child }).expect("frozen column update");
    write.commit().expect("commit");
    let changes = source.read().expect("read").changes_since(LOCAL, 0, 10).expect("log");
    let puts = |change: &RowChange| change.ops.iter().filter(|op| matches!(op, RowOp::PutObject { .. })).count();
    assert_eq!(
        changes.iter().map(puts).collect::<Vec<_>>(),
        [1, 1, 0, 0, 1],
        "two inserts, a root update keeping the snapshot, a child update, a root update replacing it"
    );
    let RowOp::PutObject { hash, media: 1, bytes } = &changes[1].ops[0] else { panic!("object first: {:?}", changes[1]) };
    assert_eq!(Hash::of(bytes), *hash);

    let mut target = replica();
    let mut write = target.write().expect("write");
    let stripped = RowChange { ops: changes[1].ops[1..].to_vec(), ..changes[1].clone() };
    assert!(matches!(write.apply(&stripped), Err(StoreError::MissingObject { hash: missing }) if missing == *hash));
    assert_eq!(write.resolve(Convoy::CODE, "b", LOCAL).expect("resolve"), None, "a refused apply writes nothing");
    for change in &changes[1..] {
        assert!(matches!(write.apply(change).expect("b's changes apply without a's"), Applied::Applied(_)));
    }
    let id = write.resolve(Convoy::CODE, "b", LOCAL).expect("resolve").expect("present");
    let newest = source.read().expect("read").load_snapshot(b.id).expect("snapshot");
    assert_eq!(write.load_snapshot(id).expect("snapshot"), newest);
}

// Behaviour (DESIGN change log): `apply` verifies each `PutObject` against its hash and refuses
// tampered bytes, leaving nothing behind.
#[test]
fn a_tampered_object_is_refused() {
    let mut source = store();
    let mut write = source.write().expect("write");
    write.insert("c", &Convoy::sample(1)).expect("insert");
    write.commit().expect("commit");
    let mut change = first_change(&mut source, LOCAL);
    let Some(RowOp::PutObject { bytes, .. }) = change.ops.first_mut() else { panic!("object first: {change:?}") };
    bytes[0] ^= 1;

    let mut target = replica();
    let mut write = target.write().expect("write");
    assert!(matches!(write.apply(&change), Err(StoreError::TamperedObject { .. })));
    assert_eq!(write.resolve(Convoy::CODE, "c", LOCAL).expect("resolve"), None);
}

// Behaviour: `apply` refuses ops that do not fit the kind: an unknown table, a row whose values
// do not match the table's columns, and a root row deleted by row rather than `DeleteResource`.
#[test]
fn malformed_changes_are_refused() {
    let mut source = store();
    let mut write = source.write().expect("write");
    write.insert("m", &Message::sample()).expect("insert");
    write.commit().expect("commit");
    let change = first_change(&mut source, LOCAL);
    let root = Message::table_set().tables[0].name.clone();
    let with = |op: RowOp| RowChange { ops: vec![op], ..change.clone() };
    let mut target = replica();
    let mut write = target.write().expect("write");
    let unknown = with(RowOp::Insert { table: "nope".into(), key: vec![], values: vec![] });
    assert!(matches!(write.apply(&unknown), Err(StoreError::MalformedChange(Malformed::UnknownTable { .. }))));
    let short = with(RowOp::Insert { table: root.clone(), key: vec![], values: vec![Value::Null] });
    assert!(matches!(write.apply(&short), Err(StoreError::MalformedChange(Malformed::Shape { .. }))));
    write.apply(&change).expect("the real insert applies");
    let root_delete = RowChange { home_seq: change.home_seq + 1, ..with(RowOp::Delete { table: root, key: vec![] }) };
    assert!(matches!(write.apply(&root_delete), Err(StoreError::MalformedChange(Malformed::RootDelete { .. }))));
}

static FILES: AtomicUsize = AtomicUsize::new(0);

/// A database file of its own, removed on drop.
struct TempDb(PathBuf);

impl TempDb {
    fn new() -> TempDb {
        let n = FILES.fetch_add(1, Ordering::Relaxed);
        TempDb(std::env::temp_dir().join(format!("flotilla-schema-change-log-{}-{n}.sqlite", std::process::id())))
    }

    fn open(&self) -> Store {
        Store::open(Connection::open(&self.0).expect("open file"), LOCAL, registry()).expect("open store")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// Behaviour (DESIGN shared tables): the home sequence never depends on `change_log` rows, so
// deleting the oldest and the newest log rows, and reopening the store on the same file, never
// lets a sequence repeat.
#[test]
fn sequences_never_repeat_after_log_rows_are_deleted() {
    let db = TempDb::new();
    let mut store = db.open();
    let mut write = store.write().expect("write");
    let versions: Vec<u64> = (0..4).map(|n| write.insert(&format!("m{n}"), &Message::sample()).expect("insert").version).collect();
    write.commit().expect("commit");
    let conn = store.into_connection();
    conn.execute(
        "DELETE FROM change_log WHERE home_seq IN ((SELECT min(home_seq) FROM change_log), (SELECT max(home_seq) FROM change_log))",
        [],
    )
    .expect("delete log rows");
    drop(conn);

    let mut store = db.open();
    let mut write = store.write().expect("write");
    let next = write.insert("m9", &Message::sample()).expect("insert after reopen");
    assert!(next.version > versions[3], "{} after {}", next.version, versions[3]);
    assert_eq!(
        write.changes_since(LOCAL, 0, 10).expect("log").iter().map(|change| change.home_seq).collect::<Vec<_>>(),
        [versions[1], versions[2], next.version]
    );
    write.commit().expect("commit");
}

/// A change that recreates a snapshot's rows: the objects it carries, then every row in table
/// pre-order. Test-only: replicas are seeded this way in the snapshot-plus-tail law below.
fn seed(snapshot: &Snapshot) -> RowChange {
    let set = registry().get(snapshot.kind).expect("registered").clone();
    let mut ops: Vec<RowOp> =
        snapshot.rows.objects.iter().map(|(hash, bytes)| RowOp::PutObject { hash: *hash, media: 1, bytes: bytes.clone() }).collect();
    for (table, rows) in set.tables.iter().zip(&snapshot.rows.tables) {
        ops.extend(rows.iter().map(|row| RowOp::Insert { table: table.name.clone(), key: row.key.clone(), values: row.values.clone() }));
    }
    RowChange { kind: snapshot.kind, name: snapshot.name.clone(), home: snapshot.home.clone(), home_seq: snapshot.version, ops }
}

fn snapshot_plus_tail<K: Fixture>(tc: &TestCase) {
    let mut source = store();
    let mut write = source.write().expect("write");
    let mut stamp = write.insert("r", &K::generate(tc, Pick::Draw)).expect("insert");
    write.commit().expect("commit");
    let updates = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let at = tc.draw(gs::integers::<usize>().min_value(0).max_value(updates));
    let mut taken = None;
    for n in 0..=updates {
        if n == at {
            taken = source.read().expect("read").load_snapshot(stamp.id).expect("snapshot");
        }
        if n < updates {
            let mut write = source.write().expect("write");
            let current = write.load::<K>(stamp.id).expect("load").expect("present");
            stamp = write.update(&current, &current.value.evolve(tc, Pick::Draw)).expect("update");
            write.commit().expect("commit");
        }
    }
    let taken = taken.expect("present");

    let mut target = replica();
    let mut write = target.write().expect("write");
    write.apply(&seed(&taken)).expect("seed");
    let mut after = taken.version;
    loop {
        let tail = source.read().expect("read").changes(stamp.id, after, 2).expect("tail");
        let Some(last) = tail.last() else { break };
        after = last.home_seq;
        for change in &tail {
            assert!(matches!(write.apply(change).expect("apply"), Applied::Applied(_)));
        }
    }
    let id = write.resolve(K::CODE, "r", LOCAL).expect("resolve").expect("present");
    let newest = source.read().expect("read").load_snapshot(stamp.id).expect("snapshot");
    assert_eq!(write.load_snapshot(id).expect("snapshot"), newest);
}

// Behaviour (#3208 snapshot plus tail): a replica holding only a resource's snapshot at version v
// (its rows and the objects they name), plus that resource's logged changes after v (read by
// `changes`, paged two at a time), holds the newest snapshot: each tail change carries its objects.
#[hegel::test]
fn a_snapshot_plus_its_tail_is_the_newest_snapshot(tc: TestCase) {
    snapshot_plus_tail::<Message>(&tc);
    snapshot_plus_tail::<Convoy>(&tc);
    snapshot_plus_tail::<TerminalSession>(&tc);
}
