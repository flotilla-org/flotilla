use flotilla_schema_sqlite::Store;
use hegel::{generators as gs, TestCase};

use crate::fixtures::{store, Convoy, Fixture, Message, Pick, TerminalSession, LOCAL};

/// Inserts `first`, then evolves it through `steps` writes: mostly generated edges, and a spec
/// edit about one write in four. After every write the snapshot and `load` decode to the model:
/// times truncated to microseconds and each record stamped with the version of the write that
/// appended it, whether the insert or an edge.
fn evolve<K: Fixture>(tc: &TestCase, store: &mut Store, name: &str, first: &K, steps: usize, pick: impl Fn(usize) -> Pick) {
    let mut write = store.write().expect("write");
    let inserted = write.insert(name, first).expect("insert");
    write.commit().expect("commit");
    let mut model = first.stamped(None, inserted.version).stored();
    let mut stamp = inserted;
    for step in 0..=steps {
        let mut read = store.read().expect("read");
        let snapshot = read.load_snapshot(stamp.id).expect("load").expect("present");
        assert_eq!((snapshot.kind, snapshot.name.as_str(), snapshot.home.as_str()), (K::CODE, name, LOCAL));
        assert_eq!(snapshot.version, stamp.version);
        assert_eq!(snapshot.decode::<K>().expect("decode"), model, "after write {step}");
        let current = read.load::<K>(stamp.id).expect("load").expect("present");
        assert_eq!(current.value, model);
        drop(read);
        if step == steps {
            break;
        }

        let mut write = store.write().expect("write");
        if tc.draw(gs::integers::<u8>().max_value(3)) == 0 {
            let next = current.value.evolve(tc, pick(step));
            stamp = write.update(&current, &next).expect("a spec edit keeps states and records");
            if next.stored() == current.value {
                assert_eq!(stamp, current.stamp(), "an update with no row changes is a no-op");
            }
            model = next.stored();
        } else {
            let (edge, next) = current.value.edge(tc, pick(step));
            stamp = write.apply_edge(current.stamp(), edge).expect("edge");
            model = next.stamped(Some(&current.value), stamp.version).stored();
        }
        write.commit().expect("commit");
        assert!(stamp.version >= current.version);
    }
}

fn drawn<K: Fixture>(tc: &TestCase) {
    let first = K::generate(tc, Pick::Draw);
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    evolve(tc, &mut store(), "r", &first, steps, |_| Pick::Draw);
}

/// Covering values: each index's value, evolved through the following indexes' shapes, so every
/// edge variant runs on every fixture shape and every enum also switches variant through a write.
fn covering<K: Fixture>(tc: &TestCase) {
    let mut store = store();
    for index in 0..K::COVERING {
        let first = K::generate(tc, Pick::Cover(index));
        evolve(tc, &mut store, &format!("r{index}"), &first, 3, |step| Pick::Cover((index + 1 + step) % K::COVERING));
    }
}

// Behaviour (#3320 round trip, ruling 1): a resource evolves only by edges and spec edits after its
// insert, and after every write `load_snapshot` decodes to the model value: edges append their
// event and declared rows (stamped with the edge's version) and set their declared fields, spec
// edits change spec fields and child tables, and nothing else moves.
#[hegel::test]
fn message_evolves_by_edges_and_spec_edits(tc: TestCase) {
    drawn::<Message>(&tc);
}

#[hegel::test]
fn convoy_evolves_by_edges_and_spec_edits(tc: TestCase) {
    drawn::<Convoy>(&tc);
}

#[hegel::test]
fn terminal_session_evolves_by_edges_and_spec_edits(tc: TestCase) {
    drawn::<TerminalSession>(&tc);
}

// Behaviour (#3320 round trip): the same law holds for every variant of every enum, nested,
// flattened and in child tables, for every edge, and for empty and non-empty collections at every
// depth: the covering indexes reach each of them (see each fixture's `COVERING`).
#[hegel::test]
fn every_variant_round_trips_and_switches_through_writes(tc: TestCase) {
    covering::<Message>(&tc);
    covering::<Convoy>(&tc);
    covering::<TerminalSession>(&tc);
}
