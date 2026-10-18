use flotilla_schema::{Code, Identity, Kind, Timestamp};
use flotilla_schema_sqlite::{
    stream::{self, Follower, Frame, Watermark},
    Store, StoreError,
};
use hegel::generators as gs;
use rusqlite::Connection;

use crate::fixtures::{addr, message, origin, registry, Message, Resource};

fn store(home: &str) -> Store {
    Store::open(Connection::open_in_memory().expect("memory"), home, registry()).expect("store")
}
fn insert(store: &mut Store, name: &str) -> flotilla_schema_sqlite::Stamp {
    let mut write = store.write().expect("write");
    let stamp = write.insert(name, &message::sample_in(addr("inbox")), &origin()).expect("insert");
    write.commit().expect("commit");
    stamp
}
fn sync(source: &mut Store, target: &mut Store, after: Option<u64>, time: i64) -> Result<(), StoreError> {
    let home = source.home().to_owned();
    Follower::new(target).receive(&home, |sink| stream::serve(source, after, Timestamp(time), sink).map(|_| ()))
}
fn mark(store: &mut Store, home: &str) -> Watermark {
    store.read().expect("read").watermark(home).expect("mark")
}
fn present(store: &mut Store, home: &str, name: &str) -> bool {
    let mut read = store.read().expect("read");
    let id = read.resolve(Resource::<Message>::CODE, name, home).expect("identity");
    id.is_some_and(|id| read.load_snapshot(id).expect("snapshot").is_some())
}

// Every host holds every home's resource after a snapshot and resumed tail. A three-store
// full mesh proves copied events are not re-originated under the receiving home's sequence.
#[test]
fn snapshot_tail_resume_and_three_stores() {
    let mut a = store("a");
    let mut b = store("b");
    let mut c = store("c");
    insert(&mut a, "a1");
    insert(&mut b, "b1");
    insert(&mut c, "c1");
    sync(&mut a, &mut b, None, 1).expect("snapshot");
    sync(&mut a, &mut c, None, 1).expect("snapshot");
    sync(&mut b, &mut a, None, 1).expect("snapshot");
    sync(&mut b, &mut c, None, 1).expect("snapshot");
    sync(&mut c, &mut a, None, 1).expect("snapshot");
    sync(&mut c, &mut b, None, 1).expect("snapshot");
    for host in [&mut a, &mut b, &mut c] {
        for home in ["a", "b", "c"] {
            assert!(present(host, home, &format!("{home}1")));
        }
        assert_eq!(host.read().expect("read").last_sequence("a").expect("last"), 1);
    }
    insert(&mut a, "a2");
    sync(&mut a, &mut b, Some(1), 2).expect("tail");
    assert!(present(&mut b, "a", "a2"));
    let before = mark(&mut b, "a");
    sync(&mut a, &mut b, Some(2), 3).expect("resume idle");
    assert_eq!(mark(&mut b, "a").home_seq, before.home_seq);
    assert_eq!(mark(&mut b, "a").applied_through, Timestamp(3));
}

// Expunge removes history and advances its floor in the same transaction. Rollback restores
// both. A cursor below the committed floor receives a fresh snapshot, clearing stale bodies.
#[test]
fn expunge_floor_rollback_and_fresh_snapshot() {
    let mut a = store("a");
    let mut b = store("b");
    let old = insert(&mut a, "old");
    sync(&mut a, &mut b, None, 100).expect("snapshot");
    let mut write = a.write().expect("write");
    write.delete::<Resource<Message>>(old, &origin()).expect("delete");
    write.commit().expect("commit");
    {
        let mut write = a.write().expect("write");
        write.expunge::<Resource<Message>>(old.id, &origin()).expect("expunge");
    }
    assert_eq!(a.read().expect("read").resume_floor("a").expect("floor"), 0);
    assert_eq!(a.read().expect("read").changes_since("a", 0, 10).expect("log").len(), 2);
    let mut write = a.write().expect("write");
    write.expunge::<Resource<Message>>(old.id, &origin()).expect("expunge");
    write.commit().expect("commit");
    assert_eq!(a.read().expect("read").resume_floor("a").expect("floor"), 2);
    insert(&mut a, "new");
    sync(&mut a, &mut b, Some(1), 4).expect("resnapshot");
    assert_eq!(mark(&mut b, "a").applied_through, Timestamp(100), "resnapshot cannot regress applied time");
    assert!(b.read().expect("read").resolve(Resource::<Message>::CODE, "old", "a").expect("identity").is_none());
    assert!(!present(&mut b, "a", "old"));
    assert!(present(&mut b, "a", "new"));
    sync(&mut a, &mut b, Some(4), 5).expect("tail from fresh cut");
}

// Disconnecting in a snapshot or before a round commits preserves the old rows and watermark.
// Keepalives cannot acknowledge unseen rows; decreasing watermark time is refused atomically.
#[test]
fn stalled_stream_and_invalid_watermarks_do_not_advance() {
    let mut a = store("a");
    let mut b = store("b");
    insert(&mut a, "m");
    assert_eq!(mark(&mut b, "unknown"), Watermark::ZERO);
    assert_eq!(b.read().expect("read").resume_floor("unknown").expect("floor"), 0);
    let failed = Follower::new(&mut b).receive("a", |sink| {
        stream::serve(&mut a, None, Timestamp(10), |bytes| {
            if matches!(Frame::decode(bytes)?, Frame::End { .. }) {
                return Err(StoreError::Stream("disconnected".into()));
            }
            sink(bytes)
        })
        .map(|_| ())
    });
    assert!(failed.is_err());
    assert_eq!(mark(&mut b, "a"), Watermark::ZERO);
    assert!(!present(&mut b, "a", "m"));
    sync(&mut a, &mut b, None, 10).expect("snapshot");
    let before = mark(&mut b, "a");
    let mut write = b.write().expect("write");
    assert!(write.advance_watermark("a", Watermark { home_seq: 1, applied_through: Timestamp(9) }).is_err());
    assert!(write.advance_watermark("a", Watermark { home_seq: 2, applied_through: Timestamp(11) }).is_err());
    write.commit().expect("commit refusals");
    assert_eq!(mark(&mut b, "a"), before);
    sync(&mut a, &mut b, Some(1), 12).expect("keepalive");
    assert_eq!(mark(&mut b, "a").applied_through, Timestamp(12));
    {
        let mut write = b.write().expect("write");
        write.advance_watermark("a", Watermark { home_seq: 1, applied_through: Timestamp(20) }).expect("pending mark");
    }
    assert_eq!(b.applied_watermark("a").applied_through, Timestamp(12), "rolled-back marks never reach resident gate inputs");
    let b = Store::open(b.into_connection(), "b", registry()).expect("restart");
    assert_eq!(b.applied_watermark("a").applied_through, Timestamp(12), "startup rebuilds committed resident inputs");
}

// The reserved code is supplied explicitly for writes and preserved on apply and CBOR replay.
// A surface miss performs exactly one home query, marks the result remote and reports age.
#[test]
fn topic_round_trip_and_routed_miss() {
    let mut a = store("a");
    let mut b = store("b");
    insert(&mut a, "m");
    let mut event = a.read().expect("read").changes_since("a", 0, 1).expect("event").remove(0);
    event.topic = Code::new("project");
    let mut write = b.write().expect("write");
    write.apply(&event).expect("apply");
    write.commit().expect("commit");
    assert_eq!(b.read().expect("read").changes_since("a", 0, 1).expect("event")[0], event);
    let mut c = store("c");
    let identity = Identity { kind: Resource::<Message>::CODE, name: "m".into(), home: "a".into() };
    let mut calls = 0;
    let result = stream::read_or_route(&mut c, &identity, Timestamp(12), |identity| {
        calls += 1;
        let mut read = a.read()?;
        let id = read.resolve(identity.kind, &identity.name, &identity.home)?.expect("id");
        Ok((read.load_snapshot(id)?, Timestamp(9)))
    })
    .expect("remote");
    assert_eq!(calls, 1);
    assert!(result.remote);
    assert_eq!(result.age_micros, 3);
    assert!(result.value.is_some());
    let result = stream::read_or_route(&mut b, &identity, Timestamp(12), |_| panic!("local read must not route")).expect("local");
    assert!(!result.remote);
}

// Differential conformance with unchanged world/stream.qnt: watermark is the maximum applied
// keepalive time. Enumerate its finite marks (zero, equal, increasing, decreasing), prefixes
// and links; a disconnected transport delivers no additional prefix, independent of stored time.
#[test]
fn quint_watermark_prefix_conformance() {
    for a in 0..=2 {
        for b in 0..=2 {
            for prefix in 0..=2 {
                for linked in [false, true] {
                    let mut replica = store("replica");
                    for time in [a, b].into_iter().take(prefix) {
                        let frame = Frame::Keepalive {
                            home: "home".into(),
                            watermark: Watermark { home_seq: 0, applied_through: Timestamp(time) },
                        }
                        .encode()
                        .expect("frame");
                        Follower::new(&mut replica).receive("home", |sink| sink(&frame)).expect("apply prefix");
                    }
                    let expected = [a, b].into_iter().take(prefix).max().unwrap_or(0);
                    // Link state after application cannot invalidate an observed prefix. A connected
                    // duplicate old mark also cannot lower it; disconnected links deliver nothing.
                    if linked {
                        let old = Frame::Keepalive { home: "home".into(), watermark: Watermark::ZERO }.encode().expect("old mark");
                        Follower::new(&mut replica).receive("home", |sink| sink(&old)).expect("older mark");
                    }
                    assert_eq!(mark(&mut replica, "home").applied_through, Timestamp(expected));
                }
            }
        }
    }
}

// Generated two-store interleavings: writes during partitions hold the follower's watermark;
// reconnecting from its committed prefix converges without resnapshotting or re-running machines.
#[hegel::test]
fn generated_partition_resume(tc: hegel::TestCase) {
    let mut a = store("a");
    let mut b = store("b");
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let mut seen = 0;
    for step in 1..=steps {
        insert(&mut a, &format!("m{step}"));
        if tc.draw(gs::booleans()) {
            sync(&mut a, &mut b, Some(seen), step as i64).expect("resume");
            seen = step as u64;
        }
        assert_eq!(mark(&mut b, "a").home_seq, seen);
        for version in 1..=step {
            assert_eq!(present(&mut b, "a", &format!("m{version}")), version as u64 <= seen);
        }
    }
    sync(&mut a, &mut b, Some(seen), steps as i64 + 1).expect("heal");
    assert_eq!(mark(&mut b, "a").home_seq, steps as u64);
}

// Every kind's rows, nested records, frozen objects and identity references survive the same
// streaming-row protocol. The generator selects among all fixture kinds and their full shapes.
#[hegel::test]
fn snapshot_chunks_preserve_all_kind_shapes(tc: hegel::TestCase) {
    use crate::fixtures::{AgentSession, Convoy, Dossier, Ensure, Fixture, Pick, Referral, TerminalSession, Ticket, Vessel};
    fn check<K: Fixture>(tc: &hegel::TestCase) {
        let mut a = store("a");
        let mut b = store("b");
        let value = K::generate(tc, Pick::Draw);
        let mut write = a.write().expect("write");
        let stamp = write.insert("m", &value, &origin()).expect("insert");
        write.commit().expect("commit");
        let expected = a.read().expect("read").load_snapshot(stamp.id).expect("load");
        sync(&mut a, &mut b, None, 3).expect("chunked snapshot");
        let mut read = b.read().expect("read");
        let id = read.resolve(K::CODE, "m", "a").expect("identity").expect("id");
        assert_eq!(read.load_snapshot(id).expect("snapshot"), expected);
    }
    match tc.draw(gs::integers::<u8>().max_value(8)) {
        0 => check::<Resource<Message>>(&tc),
        1 => check::<Resource<Convoy>>(&tc),
        2 => check::<Resource<Ensure>>(&tc),
        3 => check::<Resource<Vessel>>(&tc),
        4 => check::<Resource<AgentSession>>(&tc),
        5 => check::<TerminalSession>(&tc),
        6 => check::<Ticket>(&tc),
        7 => check::<Dossier>(&tc),
        _ => check::<Referral>(&tc),
    }
}

// A stored maximum sequence is not proof of a complete prefix: direct per-resource apply
// can see an independent resource's later event first. Gate progress must refuse the hole.
#[test]
fn watermark_rejects_holes_even_when_later_rows_are_stored() {
    let mut a = store("a");
    let mut b = store("b");
    insert(&mut a, "one");
    insert(&mut a, "two");
    insert(&mut a, "three");
    let events = a.read().expect("read").changes_since("a", 0, 10).expect("events");
    let mut write = b.write().expect("write");
    write.apply(&events[2]).expect("independent resource");
    assert!(write.advance_watermark("a", Watermark { home_seq: 3, applied_through: Timestamp(1) }).is_err());
    assert!(write.advance_watermark("a", Watermark { home_seq: 1, applied_through: Timestamp(1) }).is_err());
    write.commit().expect("commit rows only");
    assert_eq!(b.applied_watermark("a"), Watermark::ZERO);
}

// A snapshot larger than an identity page/cache journal still installs atomically and can
// be read after restart. Clearing optional lookup overlays must not lose uncommitted IDs.
#[test]
fn large_snapshot_crosses_identity_pages_and_cache_bounds() {
    let mut a = store("a");
    let mut b = store("b");
    let value = message::sample_in(addr("inbox"));
    let mut write = a.write().expect("write");
    for index in 0..3000 {
        write.insert(&format!("m{index}"), &value, &origin()).expect("insert");
    }
    write.commit().expect("commit");
    sync(&mut a, &mut b, None, 10).expect("large snapshot");
    let mut b = Store::open(b.into_connection(), "b", registry()).expect("restart");
    assert_eq!(b.applied_watermark("a").home_seq, 3000);
    for index in [0, 1023, 1024, 2048, 2999] {
        assert!(present(&mut b, "a", &format!("m{index}")));
    }
}

// The production service reads WAL snapshots on a separate read-only connection. A writer
// during an emitted frame cannot deadlock the snapshot reader or leak into its earlier cut.
#[test]
fn wal_snapshot_does_not_hold_the_writer_mutex() {
    use flotilla_schema_sqlite::stream::Service;
    use std::{
        sync::{mpsc, Arc},
        time::Duration,
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(Service::open(&dir.path().join("store.sqlite"), "home", registry()).expect("file service"));
    service
        .with_store(|store| {
            insert(store, "committed");
            Ok(())
        })
        .expect("seed");
    let (tx, rx) = mpsc::channel();
    let source = Arc::clone(&service);
    let task = std::thread::spawn(move || {
        let mut names = Vec::new();
        let result = source
            .serve(None, Timestamp(3), |bytes| {
                match Frame::decode(bytes)? {
                    Frame::Begin { .. } => source.with_store(|store| {
                        let mut write = store.write()?;
                        write.insert("later", &message::sample_in(addr("inbox")), &flotilla_schema::Origin::new("writer", Timestamp(4)))?;
                        write.commit()
                    })?,
                    Frame::Resource(value) => names.push(value.name),
                    _ => {}
                }
                Ok(())
            })
            .map(|_| names);
        tx.send(result).expect("result");
    });
    let names = rx.recv_timeout(Duration::from_secs(5)).expect("reader must not wait on writer lock").expect("snapshot");
    task.join().expect("worker");
    assert!(names.iter().any(|name| name == "committed"));
    assert!(!names.iter().any(|name| name == "later"));
}

// A pooled WAL reader must not carry an old local ID into the next snapshot after expunge
// reuses it. The tail cache also delivers identical authoritative events to two followers.
#[test]
fn pooled_readers_and_shared_tail_survive_expunge_and_id_reuse() {
    use flotilla_schema_sqlite::stream::Service;
    let dir = tempfile::tempdir().expect("dir");
    let source = Service::open(&dir.path().join("store.sqlite"), "a", registry()).expect("service");
    let old = source.with_store(|store| Ok(insert(store, "old"))).expect("seed");
    let mut first = store("b");
    let mut second = store("c");
    Follower::new(&mut first).receive("a", |sink| source.serve(None, Timestamp(1), sink).map(|_| ())).expect("snapshot");
    source.serve(Some(1), Timestamp(1), |_| Ok(())).expect("prime resident tail");
    source
        .with_store(|store| {
            let mut write = store.write()?;
            write.delete::<Resource<Message>>(old, &origin())?;
            write.expunge::<Resource<Message>>(old.id, &origin())?;
            write.commit()?;
            let new = insert(store, "new");
            assert_eq!(new.id, old.id);
            Ok(())
        })
        .expect("expunge/reuse");
    for replica in [&mut first, &mut second] {
        Follower::new(replica).receive("a", |sink| source.serve(None, Timestamp(4), sink).map(|_| ())).expect("new snapshot");
        assert!(present(replica, "a", "new"));
        assert!(!present(replica, "a", "old"));
    }
    source
        .with_store(|store| {
            insert(store, "tail");
            Ok(())
        })
        .expect("new write");
    for replica in [&mut first, &mut second] {
        Follower::new(replica).receive("a", |sink| source.serve(Some(4), Timestamp(5), sink).map(|_| ())).expect("shared tail");
        assert!(present(replica, "a", "tail"));
        assert_eq!(replica.applied_watermark("a").home_seq, 5);
    }
}

// Transport wake decisions track only committed own-home writes. Rollback and applying
// another home's copied events do not publish new local work or cause replication loops.
#[test]
fn resident_home_cursor_changes_only_on_local_commit() {
    use flotilla_schema_sqlite::stream::Service;
    let service = Service::new(store("a"));
    service
        .with_store(|store| {
            let mut write = store.write()?;
            write.insert("rolledback", &message::sample_in(addr("inbox")), &origin())?;
            Ok(())
        })
        .expect("rollback");
    assert_eq!(service.local_sequence(), 0);
    service
        .with_store(|store| {
            insert(store, "local");
            Ok(())
        })
        .expect("commit");
    assert_eq!(service.local_sequence(), 1);
    let mut remote = store("remote");
    insert(&mut remote, "remote");
    let event = remote.read().expect("read").changes_since("remote", 0, 1).expect("event").remove(0);
    service
        .with_store(|store| {
            let mut write = store.write()?;
            write.apply(&event)?;
            write.commit()
        })
        .expect("copy");
    assert_eq!(service.local_sequence(), 1);
}
