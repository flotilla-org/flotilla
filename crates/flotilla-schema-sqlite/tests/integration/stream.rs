use flotilla_schema::{Code, Kind, Timestamp};
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
#[test]
fn topic_round_trip() {
    let mut a = store("a");
    let mut b = store("b");
    insert(&mut a, "m");
    let mut event = a.read().expect("read").changes_since("a", 0, 1).expect("event").remove(0);
    event.topic = Code::new("project");
    let mut write = b.write().expect("write");
    write.apply(&event).expect("apply");
    write.commit().expect("commit");
    assert_eq!(b.read().expect("read").changes_since("a", 0, 1).expect("event")[0], event);
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
// per-resource protocol. The generator selects among all fixture kinds and their full shapes.
#[hegel::test]
fn snapshot_frames_preserve_all_kind_shapes(tc: hegel::TestCase) {
    use crate::fixtures::{
        AgentSession, AwaitingReply, Convoy, Dossier, Ensure, Fixture, Person, Pick, Referral, TerminalSession, Ticket, Vessel,
    };
    fn check<K: Fixture>(tc: &hegel::TestCase) {
        let mut a = store("a");
        let mut b = store("b");
        let value = K::generate(tc, Pick::Draw);
        let mut write = a.write().expect("write");
        let stamp = write.insert("m", &value, &origin()).expect("insert");
        write.commit().expect("commit");
        let expected = a.read().expect("read").load_snapshot(stamp.id).expect("load");
        let mut count = 0;
        Follower::new(&mut b)
            .receive("a", |sink| {
                stream::serve(&mut a, None, Timestamp(3), |bytes| {
                    if matches!(Frame::decode(bytes)?, Frame::Resource(_)) {
                        count += 1;
                    }
                    sink(bytes)
                })
                .map(|_| ())
            })
            .expect("resource snapshot");
        assert_eq!(count, 1, "one complete resource per frame");
        let mut read = b.read().expect("read");
        let id = read.resolve(K::CODE, "m", "a").expect("identity").expect("id");
        assert_eq!(read.load_snapshot(id).expect("snapshot"), expected);
    }
    match tc.draw(gs::integers::<u8>().max_value(10)) {
        0 => check::<Resource<Message>>(&tc),
        1 => check::<Resource<Convoy>>(&tc),
        2 => check::<Resource<Ensure>>(&tc),
        3 => check::<Resource<Vessel>>(&tc),
        4 => check::<Resource<AgentSession>>(&tc),
        5 => check::<TerminalSession>(&tc),
        6 => check::<Ticket>(&tc),
        7 => check::<Dossier>(&tc),
        8 => check::<Resource<AwaitingReply>>(&tc),
        9 => check::<Resource<Person>>(&tc),
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

// A recreated home may have a lower sequence than its followers. Replace the stale copy
// with a snapshot rather than retrying the impossible cursor forever, then resume normally.
#[test]
fn cursor_ahead_of_reset_home_gets_fresh_snapshot() {
    let mut a = store("a");
    let mut b = store("b");
    insert(&mut a, "old1");
    insert(&mut a, "old2");
    sync(&mut a, &mut b, None, 10).expect("old snapshot");
    let mut replacement = store("a");
    insert(&mut replacement, "replacement");
    sync(&mut replacement, &mut b, Some(2), 11).expect("reset snapshot");
    assert!(!present(&mut b, "a", "old1"));
    assert!(!present(&mut b, "a", "old2"));
    assert!(present(&mut b, "a", "replacement"));
    assert_eq!(mark(&mut b, "a"), Watermark { home_seq: 1, applied_through: Timestamp(11) });
    insert(&mut replacement, "tail");
    sync(&mut replacement, &mut b, Some(1), 12).expect("resumed tail");
    assert!(present(&mut b, "a", "tail"));
}

// Gates retain access to committed progress while a writer holds the store lock. They see
// the old safe prefix during an install, and the new prefix only after the apply commits.
#[test]
fn watermark_is_independent_of_the_store_lock() {
    use flotilla_schema_sqlite::stream::Service;
    use std::{
        sync::{mpsc, Arc},
        time::Duration,
    };
    let dir = tempfile::tempdir().expect("dir");
    let service = Arc::new(Service::open(&dir.path().join("store.sqlite"), "b", registry()).expect("service"));
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let writer = Arc::clone(&service);
    let task = std::thread::spawn(move || {
        writer.with_store(|replica| {
            entered.send(()).expect("entered");
            wait.recv().expect("release");
            let mut a = store("a");
            sync(&mut a, replica, None, 17)
        })
    });
    ready.recv_timeout(Duration::from_secs(2)).expect("writer acquired lock");
    let (tx, rx) = mpsc::channel();
    let gate = Arc::clone(&service);
    let read = std::thread::spawn(move || tx.send(gate.watermark("a")).expect("gate result"));
    let before = rx.recv_timeout(Duration::from_secs(2));
    release.send(()).expect("release writer");
    task.join().expect("writer task").expect("apply");
    read.join().expect("gate task");
    assert_eq!(before.expect("gate must not wait for store").expect("watermark"), Watermark::ZERO);
    assert_eq!(service.watermark("a").expect("published").applied_through, Timestamp(17));
}

// WORKLOAD.md ceiling: 10,000 running convoys, 100 KiB upper-width convoy bodies.
// This measures the whole-resource frame and restores every resource through Follower.
#[test]
#[ignore = "in-process ceiling measurement"]
fn workload_ceiling_resource_frames() {
    use crate::fixtures::{convoy, Convoy};
    use flotilla_schema::Frozen;
    use flotilla_state_machines::machines::convoy::{PromiseKind, PromiseKinds};
    let started = std::time::Instant::now();
    let mut a = store("a");
    let mut b = store("b");
    let base = convoy::sample();
    let mut spec = base.spec().clone();
    // Frozen charter document dominates a convoy at the upper workload width.
    spec.kinds = Frozen::new(PromiseKinds { kinds: [("k".repeat(100 * 1024), PromiseKind::default())].into() }).unwrap();
    let value = base.with_spec(spec);
    let mut write = a.write().unwrap();
    for index in 0..10_000 {
        write.insert(&format!("convoy-{index}"), &value, &origin()).unwrap();
    }
    write.commit().unwrap();
    let mut frames = 0;
    let mut resources = 0;
    let mut largest = 0;
    Follower::new(&mut b)
        .receive("a", |sink| {
            stream::serve(&mut a, None, Timestamp(10), |bytes| {
                frames += 1;
                if matches!(Frame::decode(bytes)?, Frame::Resource(_)) {
                    resources += 1;
                    largest = largest.max(bytes.len());
                }
                sink(bytes)
            })
            .map(|_| ())
        })
        .unwrap();
    assert_eq!(resources, 10_000);
    assert_eq!(frames, resources + 3);
    assert!(largest < stream::FRAMING.limits.max_message);
    let mut ar = a.read().unwrap();
    let mut br = b.read().unwrap();
    for index in 0..10_000 {
        let name = format!("convoy-{index}");
        let aid = ar.resolve(Resource::<Convoy>::CODE, &name, "a").unwrap().unwrap();
        let bid = br.resolve(Resource::<Convoy>::CODE, &name, "a").unwrap().unwrap();
        assert_eq!(ar.load_snapshot(aid).unwrap(), br.load_snapshot(bid).unwrap());
    }
    eprintln!("ceiling: resources={resources} frames={frames} largest_frame_bytes={largest} elapsed={:?}", started.elapsed());
}

#[derive(Clone, flotilla_schema::Stored)]
#[store(kind = "GrowingSnapshot", code = "gs")]
struct GrowingSnapshot {
    #[store(child)]
    entries: std::collections::BTreeMap<u32, String>,
}

// Individual writes fit the log, but accumulated children can exceed a snapshot frame.
// Only that resource is chunked; the follower assembles it and uses the same restore().
#[test]
fn oversized_resource_uses_chunks_and_restores_atomically() {
    let mut kinds = flotilla_schema::KindRegistry::new();
    kinds.register::<GrowingSnapshot>().unwrap();
    let mut a = Store::open(Connection::open_in_memory().unwrap(), "a", kinds.clone()).unwrap();
    let mut b = Store::open(Connection::open_in_memory().unwrap(), "b", kinds).unwrap();
    let mut write = a.write().unwrap();
    let stamp = write.insert("large", &GrowingSnapshot { entries: Default::default() }, &origin()).unwrap();
    write.commit().unwrap();
    for batch in 0..32 {
        let current = a.read().unwrap().load::<GrowingSnapshot>(stamp.id).unwrap().unwrap();
        let mut next = current.value.clone();
        for index in 0..32 {
            next.entries.insert(batch * 32 + index, "x".repeat(17 * 1024));
        }
        let mut write = a.write().unwrap();
        write.update(&current, &next, &origin()).unwrap();
        write.commit().unwrap();
    }
    let expected = a.read().unwrap().load_snapshot(stamp.id).unwrap().unwrap();
    assert!(Frame::Resource(expected.clone()).encode().is_err(), "measured resource exceeds one frame");
    let failed = Follower::new(&mut b).receive("a", |sink| {
        stream::serve(&mut a, None, Timestamp(9), |bytes| {
            if matches!(Frame::decode(bytes)?, Frame::ResourceChunk { last: true, .. }) {
                return Err(StoreError::Stream("disconnected".into()));
            }
            sink(bytes)
        })
        .map(|_| ())
    });
    assert!(failed.is_err());
    assert_eq!(mark(&mut b, "a"), Watermark::ZERO);
    assert!(b.read().unwrap().resolve(GrowingSnapshot::CODE, "large", "a").unwrap().is_none());
    let mut chunks = 0;
    let mut largest = 0;
    Follower::new(&mut b)
        .receive("a", |sink| {
            stream::serve(&mut a, None, Timestamp(10), |bytes| {
                largest = largest.max(bytes.len());
                if matches!(Frame::decode(bytes)?, Frame::ResourceChunk { .. }) {
                    chunks += 1;
                }
                sink(bytes)
            })
            .map(|_| ())
        })
        .unwrap();
    let mut read = b.read().unwrap();
    let id = read.resolve(GrowingSnapshot::CODE, "large", "a").unwrap().unwrap();
    assert_eq!(read.load_snapshot(id).unwrap(), Some(expected));
    assert_eq!(chunks, 1025);
    eprintln!("oversized: child_payload_bytes={} chunks={chunks} largest_frame_bytes={largest}", 1024 * 17 * 1024);
}
