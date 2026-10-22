//! The provisioning read port over the decided store (#3629 layer 4): keyed and indexed reads,
//! and the judgements made per key at read time.

use std::sync::{Arc, Mutex};

use flotilla_engine_store::StoreLog;
use flotilla_provisioning::{
    engine::{
        reads::{judge, Ending, Judgement, ProvisioningReads, StoreReads},
        LifecycleHome, Lifecycles, Step,
    },
    stored::{
        self,
        checkout::{CheckoutKind, CheckoutRequest, Source},
        clone::CloneKind,
    },
};
use flotilla_schema::{Kind, KindRegistry, Ref, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory, Home, Outcome};

const HOME: &str = "home";

fn store() -> Arc<Mutex<Store>> {
    let mut registry = KindRegistry::new();
    stored::register(&mut registry).expect("registry");
    Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().expect("memory"), HOME, registry).expect("store")))
}

fn checkout(environment: &str, convoy: &str) -> CheckoutRequest {
    CheckoutRequest {
        repository: "repo".into(),
        environment: Ref::new(environment.to_owned(), HOME),
        branch: "feature".into(),
        base: None,
        path: "/w".into(),
        source: Source::Worktree,
        clone: Some(Ref::new("clone-a", HOME)),
        convoy: Some(Ref::new(convoy.to_owned(), HOME)),
        ensure: None,
    }
}

fn failure(detail: &str) -> Step {
    Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, detail), retryable: true })
}

// Behaviour (#3629 layer 4): each view is a keyed or indexed read judged at read time. A
// requested checkout is in progress; after a failure it is retrying, due at the failure plus the
// policy's backoff; realised it is ready under its identity; Gone is lost when it vanished and
// released when torn down, told apart by its last log entry.
#[test]
fn views_are_read_by_key_and_index_and_judged_per_key() {
    let store = store();
    let checkouts: Arc<dyn Lifecycles<CheckoutKind>> =
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("home")));
    let reads = StoreReads::new(Arc::clone(&store));
    for (name, environment, convoy) in [("a", "env-1", "convoy-1"), ("b", "env-1", "convoy-2"), ("c", "env-2", "convoy-1")] {
        checkouts.request(name, checkout(environment, convoy), Timestamp(1_000_000)).expect("requested");
    }
    assert_eq!(reads.checkout("a", HOME).expect("read").expect("held").judgement, Judgement::InProgress);
    assert!(reads.checkout("a", "elsewhere").expect("read").is_none(), "keyed by home too");

    checkouts.step("a", &failure("worktree add failed"), Timestamp(2_000_000)).expect("attempt");
    match reads.checkout("a", HOME).expect("read").expect("held").judgement {
        Judgement::Retrying { failures: 1, next, last } => {
            assert_eq!(next, Timestamp(3_000_000), "Checkout's default policy: one second after the first failure");
            assert_eq!(last.detail, "worktree add failed");
        }
        other => panic!("{other:?}"),
    }

    checkouts.step("b", &Step::Realised { identity: "/w".into() }, Timestamp(3)).expect("realised");
    checkouts.step("b", &Step::Vanished, Timestamp(4)).expect("vanished");
    checkouts.step("c", &Step::Released, Timestamp(4)).expect("released");
    assert_eq!(reads.checkout("b", HOME).expect("read").expect("held").judgement, Judgement::Lost);
    assert_eq!(reads.checkout("c", HOME).expect("read").expect("held").judgement, Judgement::Released);

    let names = |views: Vec<flotilla_provisioning::engine::reads::View<CheckoutKind>>| {
        let mut names: Vec<String> = views.into_iter().map(|view| view.name).collect();
        names.sort();
        names
    };
    assert_eq!(names(reads.environment_checkouts("env-1", HOME).expect("index")), ["a", "b"]);
    assert_eq!(names(reads.convoy_checkouts("convoy-1", HOME).expect("index")), ["a", "c"]);
    assert!(reads.environment_checkouts("env-3", HOME).expect("index").is_empty());
}

// Behaviour: the judgements over every stored state, including a spent policy before its give-up.
#[test]
fn judgements_follow_the_stored_state_and_the_policy() {
    use flotilla_state_machines::Failure;
    let cause = Cause::new(CauseCategory::Transient, "unreachable");
    let failed = |failures, retryable| LifecycleState::Requested {
        failures,
        last_failure: Some(Failure { at: Timestamp(0), cause: cause.clone(), retryable }),
    };
    assert_eq!(judge::<CheckoutKind>(&LifecycleState::requested(), Ending::Unknown), Judgement::InProgress);
    assert!(matches!(judge::<CheckoutKind>(&failed(2, true), Ending::Unknown), Judgement::Retrying { failures: 2, .. }));
    assert_eq!(judge::<CheckoutKind>(&failed(3, true), Ending::Unknown), Judgement::Exhausted { last: cause.clone() });
    assert_eq!(judge::<CloneKind>(&failed(1, false), Ending::Unknown), Judgement::Exhausted { last: cause.clone() });
    assert!(matches!(judge::<CloneKind>(&failed(50, true), Ending::Unknown), Judgement::Retrying { .. }), "a clone retries while wanted");
    assert_eq!(judge::<CheckoutKind>(&LifecycleState::GaveUp { cause: cause.clone() }, Ending::Unknown), Judgement::GaveUp { cause });
}

// Behaviour (#3731 review): a replica told of a release by its events reads Released, as its home
// does; a replica seeded from a snapshot holds no entry from before it, so it reads the
// unattributed Gone rather than guess Lost.
#[test]
fn a_replica_reads_how_a_resource_ended_only_from_an_entry_it_holds() {
    let origin = store();
    let checkouts: Arc<dyn Lifecycles<CheckoutKind>> =
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&origin))).expect("home")));
    checkouts.request("a", checkout("env-1", "convoy-1"), Timestamp(1)).expect("requested");
    checkouts.step("a", &Step::Realised { identity: "/w".into() }, Timestamp(2)).expect("realised");
    checkouts.step("a", &Step::Released, Timestamp(3)).expect("released");

    let replica_of = || {
        let mut registry = KindRegistry::new();
        stored::register(&mut registry).expect("registry");
        Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "replica", registry).expect("store")))
    };
    let followed = replica_of();
    {
        let events = origin.lock().expect("origin").read().expect("read").changes_since(HOME, 0, 100).expect("changes");
        let mut store = followed.lock().expect("replica");
        let mut write = store.write().expect("write");
        for event in &events {
            write.apply(event).expect("applied");
        }
        write.commit().expect("commit");
    }
    assert_eq!(StoreReads::new(followed).checkout("a", HOME).expect("read").expect("held").judgement, Judgement::Released);

    let seeded = replica_of();
    {
        let snapshot = {
            let mut store = origin.lock().expect("origin");
            let mut read = store.read().expect("read");
            let id = read
                .resolve(
                    flotilla_state_machines::Resource::<flotilla_state_machines::machines::lifecycle::Lifecycle<CheckoutKind>>::CODE,
                    "a",
                    HOME,
                )
                .expect("resolve")
                .expect("stored");
            read.load_snapshot(id).expect("snapshot").expect("held")
        };
        let mut store = seeded.lock().expect("replica");
        let mut write = store.write().expect("write");
        write.restore(&snapshot).expect("restored");
        write.commit().expect("commit");
    }
    assert_eq!(StoreReads::new(seeded).checkout("a", HOME).expect("read").expect("held").judgement, Judgement::Gone);
}
