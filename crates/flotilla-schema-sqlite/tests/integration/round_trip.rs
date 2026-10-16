use std::collections::BTreeSet;

use flotilla_schema::CodeEnum;
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{Coded, EdgeKind, MachineRow};
use hegel::{generators as gs, TestCase};

use crate::fixtures::{
    store, AgentSessionRow, ConvoyRow, Dossier, EnsureRow, Fixture, MessageRow, Pick, Referral, TerminalSession, Ticket, LOCAL,
};

/// Inserts `first`, then evolves it through `steps` writes: mostly generated edges, and a spec
/// edit about one write in four or whenever no edge applies (an engine kind in an end state).
/// After every write the snapshot and `load` decode to the model: times truncated to microseconds
/// and each record stamped with the version of the write that appended it, whether the insert or
/// an edge.
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
        let edge = if tc.draw(gs::integers::<u8>().max_value(3)) == 0 { None } else { current.value.edge(tc, pick(step)) };
        match edge {
            Some((edge, next)) => {
                stamp = write.apply_edge(current.stamp(), edge).expect("edge");
                model = next.stamped(Some(&current.value), stamp.version).stored();
            }
            None => {
                let next = current.value.evolve(tc, pick(step));
                stamp = write.update(&current, &next).expect("a spec edit keeps states and records");
                if next.stored() == current.value {
                    assert_eq!(stamp, current.stamp(), "an update with no row changes is a no-op");
                }
                model = next.stored();
            }
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

// Behaviour (#3320 round trip, ruling 1; #3319 engine kinds): a resource evolves only by edges and
// spec edits after its insert, and after every write `load_snapshot` decodes to the model value:
// edges append their event and declared rows (stamped with the edge's version) and write the state
// the engine moved to, spec edits change spec fields and child tables, and nothing else moves.
// The engine kinds' edges are the engine's own: applied by a home to the stored row's resource.
#[hegel::test]
fn message_evolves_by_edges_and_spec_edits(tc: TestCase) {
    drawn::<MessageRow>(&tc);
}

#[hegel::test]
fn agent_session_evolves_by_edges_and_spec_edits(tc: TestCase) {
    drawn::<AgentSessionRow>(&tc);
}

#[hegel::test]
fn convoy_evolves_by_edges_and_spec_edits(tc: TestCase) {
    drawn::<ConvoyRow>(&tc);
}

#[hegel::test]
fn ensure_evolves_by_edges_and_spec_edits(tc: TestCase) {
    drawn::<EnsureRow>(&tc);
}

#[hegel::test]
fn terminal_session_evolves_by_edges_and_spec_edits(tc: TestCase) {
    drawn::<TerminalSession>(&tc);
}

// Behaviour (#3373 ruling 4): the synthetic kinds evolve the same way: code-enum states written
// whole, edges setting root columns and records rows, records nested in records.
#[hegel::test]
fn synthetic_kinds_evolve_by_edges_and_spec_edits(tc: TestCase) {
    drawn::<Referral>(&tc);
    drawn::<Dossier>(&tc);
}

// Behaviour (#3373 rulings 1 and 2): the synthetic `Ticket` evolves by transitions (data-only and
// label-changing, its collections growing, shrinking and changing) and spec edits, and after each
// write `load_snapshot` decodes to the model.
#[hegel::test]
fn ticket_evolves_by_transitions_and_spec_edits(tc: TestCase) {
    drawn::<Ticket>(&tc);
}

// Behaviour (#3320 round trip): the same law holds for every variant of every enum, nested,
// flattened and in child tables, for every edge, and for empty and non-empty collections at every
// depth: the covering indexes reach each of them (see each fixture's `COVERING`).
#[hegel::test]
fn every_variant_round_trips_and_switches_through_writes(tc: TestCase) {
    covering::<MessageRow>(&tc);
    covering::<AgentSessionRow>(&tc);
    covering::<ConvoyRow>(&tc);
    covering::<EnsureRow>(&tc);
    covering::<TerminalSession>(&tc);
    covering::<Ticket>(&tc);
    covering::<Referral>(&tc);
    covering::<Dossier>(&tc);
}

/// The edge kinds the fixture's edges apply across its covering values, each led by every
/// candidate in turn.
fn edges_reached<K: Fixture + MachineRow>(tc: &TestCase) -> BTreeSet<&'static str> {
    let mut reached = BTreeSet::new();
    for index in 0..K::COVERING {
        let row = K::generate(tc, Pick::Cover(index));
        for lead in 0..8 {
            if let Some((_, next)) = Fixture::edge(&row, tc, Pick::Cover(lead)) {
                let events = next.parts().events;
                let edge = events.last().expect("the edge appended its event").record.edge;
                let kind = EdgeKind::<K::Machine>::ALL.iter().find(|kind| kind.code() == edge.code()).expect("a declared kind");
                reached.insert(kind.name());
            }
        }
    }
    reached
}

fn all_edges<K: MachineRow>() -> BTreeSet<&'static str> {
    EdgeKind::<K::Machine>::ALL.iter().map(|kind| kind.name()).collect()
}

// Behaviour (#3319 engine kinds): the engine kinds' fixtures reach every edge their machine
// declares: from the covering values, the engine applies each edge kind at least once, so every
// test drawing edges runs over every real transition.
#[hegel::test(test_cases = 5)]
fn engine_fixtures_reach_every_declared_edge(tc: TestCase) {
    assert_eq!(edges_reached::<MessageRow>(&tc), all_edges::<MessageRow>());
    assert_eq!(edges_reached::<AgentSessionRow>(&tc), all_edges::<AgentSessionRow>());
    assert_eq!(edges_reached::<ConvoyRow>(&tc), all_edges::<ConvoyRow>());
    assert_eq!(edges_reached::<EnsureRow>(&tc), all_edges::<EnsureRow>());
    assert_eq!(edges_reached::<TerminalSession>(&tc), all_edges::<TerminalSession>());
}
