//! The plain-kind contract (#3824 layer 0): one suite, run against every plain kind the stack
//! moves. Each layer that moves a plain kind adds one `#[hegel::test]` calling [`contract`] with
//! a generator of its specs.

use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};

use flotilla_engine_store::{
    plain::{register_plain, Plain, PlainError, PlainLog},
    rehearse_plain,
};
use flotilla_schema::{Kind, KindRegistry, Stored, Timestamp};
use flotilla_schema_sqlite::Store;
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;

const HOME: &str = "alpha";
const REPLICA: &str = "beta";

fn open(home: &str, kinds: &KindRegistry) -> Arc<Mutex<Store>> {
    Arc::new(Mutex::new(Store::open(Connection::open_in_memory().expect("SQLite"), home, kinds.clone()).expect("store")))
}

/// Applies every entry the home logged after `after` at the replica, as the change stream does,
/// and returns the last sequence applied.
fn follow(home: &Arc<Mutex<Store>>, replica: &Arc<Mutex<Store>>, after: u64) -> u64 {
    let events = home.lock().expect("home").read().expect("read").changes_since(HOME, after, 256).expect("changes");
    let mut replica = replica.lock().expect("replica");
    let mut write = replica.write().expect("write");
    let mut last = after;
    for event in &events {
        write.apply(event).expect("replica applies the home's entry");
        last = event.home_seq;
    }
    write.commit().expect("commit");
    last
}

/// One drawn operation; versions are the model's, or one behind it for a stale writer.
#[derive(Debug)]
enum Op<K> {
    Create(usize, K),
    Edit { name: usize, spec: K, stale: bool },
    Delete { name: usize, stale: bool },
}

/// The contract every plain kind keeps, as a stateful property over a drawn sequence of creates,
/// edits and deletes on a few names, against a model map:
/// - a create of a live name is refused, of an absent or deleted one stored at a new version;
/// - an edit or delete at the version read applies, at any other version is a `Conflict` that
///   changes nothing, and of an absent name is `Missing`;
/// - an edit that changes nothing keeps the version;
/// - after every step the home reads exactly the model, and a replica that applies the home's log
///   reads the same resources at the same versions;
/// - at the end, the import rehearsal restores every live resource and reads it back unchanged.
pub fn contract<K>(tc: &TestCase, kinds: KindRegistry, spec: impl Fn(&TestCase) -> K)
where
    K: Kind + Clone + PartialEq + Debug,
{
    let home = open(HOME, &kinds);
    let replica = open(REPLICA, &kinds);
    let log = PlainLog::new(Arc::clone(&home));
    let mirror = PlainLog::new(Arc::clone(&replica));
    // Three names, so creates collide with live and deleted names and edits hit absent ones.
    let names = ["one", "two", "three"];
    let mut model: BTreeMap<&str, (u64, K)> = BTreeMap::new();
    let mut applied = 0;
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    for step in 0..steps {
        let at = Timestamp::from_micros(step as i64 + 1);
        let name = tc.draw(gs::integers::<usize>().min_value(0).max_value(names.len() - 1));
        let op = match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
            0 => Op::Create(name, spec(tc)),
            1 => Op::Edit {
                name,
                spec: if tc.draw(gs::booleans()) { spec(tc) } else { model.get(names[name]).map_or_else(|| spec(tc), |(_, s)| s.clone()) },
                stale: tc.draw(gs::booleans()),
            },
            _ => Op::Delete { name, stale: tc.draw(gs::booleans()) },
        };
        match op {
            Op::Create(name, spec) => {
                let name = names[name];
                let result = log.create(name, &spec, "writer", at);
                if model.contains_key(name) {
                    assert!(matches!(result, Err(PlainError::Exists { .. })), "a live name is refused: {result:?}");
                } else {
                    let version = result.expect("an absent name is created");
                    assert!(model.values().all(|(other, _)| *other < version), "a write takes the next home sequence");
                    model.insert(name, (version, spec));
                }
            }
            Op::Edit { name, spec, stale } => {
                let name = names[name];
                let Some((version, current)) = model.get(name).cloned() else {
                    assert!(matches!(log.edit(name, 1, &spec, "writer", at), Err(PlainError::Missing { .. })));
                    continue;
                };
                if stale {
                    let result = log.edit(name, version - 1, &spec, "writer", at);
                    assert!(matches!(result, Err(PlainError::Conflict { stored, .. }) if stored == version), "{result:?}");
                } else {
                    let next = log.edit(name, version, &spec, "writer", at).expect("an edit at the version read applies");
                    if spec == current {
                        assert_eq!(next, version, "an edit that changes nothing keeps the version");
                    } else {
                        assert!(next > version);
                    }
                    model.insert(name, (next, spec));
                }
            }
            Op::Delete { name, stale } => {
                let name = names[name];
                let Some((version, _)) = model.get(name).cloned() else {
                    assert!(matches!(log.delete::<K>(name, 1, "writer", at), Err(PlainError::Missing { .. })));
                    continue;
                };
                if stale {
                    let result = log.delete::<K>(name, version - 1, "writer", at);
                    assert!(matches!(result, Err(PlainError::Conflict { .. })), "{result:?}");
                } else {
                    log.delete::<K>(name, version, "writer", at).expect("a delete at the version read applies");
                    model.remove(name);
                }
            }
        }
        applied = follow(&home, &replica, applied);
        let expected: Vec<Plain<K>> = model
            .iter()
            .map(|(name, (version, spec))| Plain { name: (*name).to_owned(), home: HOME.to_owned(), version: *version, spec: spec.clone() })
            .collect();
        let mut at_home = log.list::<K>(HOME).expect("list");
        at_home.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(at_home, expected, "the home reads the model");
        let mut at_replica = mirror.list::<K>(HOME).expect("replica list");
        at_replica.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(at_replica, expected, "the replica reads what the home wrote");
        for name in names {
            assert_eq!(log.get::<K>(name, HOME).expect("get"), expected.iter().find(|plain| plain.name == name).cloned());
        }
    }
    let imported: Vec<_> =
        model.iter().map(|(name, (version, spec))| (HOME.to_owned(), (*name).to_owned(), *version, spec.clone())).collect();
    assert_eq!(rehearse_plain(kinds, &imported), Vec::new(), "every live resource rehearses cleanly");
}

/// A plain kind with every spec shape a moved kind uses: plain columns, an optional flattened
/// part, an interned string and a child collection.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Sample", code = "zz")]
struct Sample {
    title: String,
    #[store(intern = "zo")]
    owner: String,
    #[store(flatten)]
    limit: Option<Limit>,
    #[store(child)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Limit {
    count: i64,
}

// Specs drawn from small pools, so edits often change nothing, one field, or the collection only.
fn sample(tc: &TestCase) -> Sample {
    let word = |tc: &TestCase| ["a", "b"][tc.draw(gs::integers::<usize>().min_value(0).max_value(1))].to_owned();
    let tags = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    Sample {
        title: word(tc),
        owner: word(tc),
        limit: tc.draw(gs::booleans()).then(|| Limit { count: tc.draw(gs::integers::<i64>().min_value(-1).max_value(1)) }),
        tags: (0..tags).map(|index| format!("{}{index}", word(tc))).collect(),
    }
}

#[hegel::test]
fn sample_keeps_the_plain_contract(tc: TestCase) {
    let mut kinds = KindRegistry::new();
    register_plain::<Sample>(&mut kinds).expect("register");
    contract(&tc, kinds, sample);
}

// Behaviour (ADR 0064, #3392 G9): a delete keeps the identity as a tombstone, and a create of the
// same name afterwards reuses it at a new version, which a replica applies after the delete.
#[test]
fn a_deleted_name_is_created_again_at_a_new_version() {
    let mut kinds = KindRegistry::new();
    register_plain::<Sample>(&mut kinds).expect("register");
    let (home, replica) = (open(HOME, &kinds), open(REPLICA, &kinds));
    let log = PlainLog::new(Arc::clone(&home));
    let spec = Sample { title: "a".into(), owner: "b".into(), limit: None, tags: Vec::new() };
    let first = log.create("one", &spec, "writer", Timestamp::from_micros(1)).expect("create");
    log.delete::<Sample>("one", first, "writer", Timestamp::from_micros(2)).expect("delete");
    let second = log.create("one", &spec, "writer", Timestamp::from_micros(3)).expect("create again");
    assert!(second > first);
    follow(&home, &replica, 0);
    let again = PlainLog::new(replica).get::<Sample>("one", HOME).expect("replica get");
    assert_eq!(again, Some(Plain { name: "one".into(), home: HOME.into(), version: second, spec }));
}

// Behaviour (#3824 layer 1, the log follower): a follower names each followed resource an applied
// entry wrote; a home first seen, or one whose sequence moved by a snapshot restore (which logs
// nothing), is named for a rescan instead.
#[test]
fn a_follower_names_written_resources_and_rescans_after_a_snapshot() {
    use flotilla_engine_store::follow::{Followed, Follower};
    use flotilla_schema::Identity;
    let mut kinds = KindRegistry::new();
    register_plain::<Sample>(&mut kinds).expect("register");
    let (home, replica) = (open(HOME, &kinds), open(REPLICA, &kinds));
    let log = PlainLog::new(Arc::clone(&home));
    let spec = Sample { title: "a".into(), owner: "b".into(), limit: None, tags: Vec::new() };
    let homes = [HOME.to_owned()];
    let mut follower = Follower::new([<Sample as Kind>::CODE]);
    let pass = |follower: &mut Follower| follower.pass(&mut replica.lock().expect("replica").read().expect("read"), &homes).expect("pass");
    assert_eq!(pass(&mut follower), vec![Followed::Rescan(HOME.into())], "a home first seen is rescanned");
    log.create("one", &spec, "writer", Timestamp::from_micros(1)).expect("create");
    let applied = follow(&home, &replica, 0);
    let one = Identity { kind: <Sample as Kind>::CODE, name: "one".into(), home: HOME.into() };
    assert_eq!(pass(&mut follower), vec![Followed::Changed(one)]);
    assert_eq!(pass(&mut follower), Vec::new(), "nothing new, nothing named");
    // A second resource reaches the replica as a snapshot, as a follower's catch-up delivers it.
    let version = log.create("two", &spec, "writer", Timestamp::from_micros(2)).expect("create");
    let snapshot = {
        let mut store = home.lock().expect("home");
        let mut read = store.read().expect("read");
        let id = read.resolve(<Sample as Kind>::CODE, "two", HOME).expect("resolve").expect("stored");
        read.load_snapshot(id).expect("snapshot").expect("present")
    };
    {
        let mut store = replica.lock().expect("replica");
        let mut write = store.write().expect("write");
        write.restore(&snapshot).expect("restore");
        write.finish_snapshot(HOME, version).expect("cut");
        write.commit().expect("commit");
    }
    assert!(applied < version);
    assert_eq!(pass(&mut follower), vec![Followed::Rescan(HOME.into())], "a snapshot leaves no entries to read");
}
