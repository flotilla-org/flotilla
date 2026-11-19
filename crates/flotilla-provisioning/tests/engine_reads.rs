//! The provisioning read port over the decided store (#3629 layer 4): keyed and indexed reads,
//! and the judgements made per key at read time.

use std::sync::Arc;

use flotilla_engine_store::StoreLog;
use flotilla_provisioning::{
    engine::{
        reads::{judge, Judgement, ProvisioningReads, StoreReads},
        LifecycleHome, Lifecycles, Step,
    },
    stored::{
        self,
        checkout::{CheckoutKind, CheckoutRequest, Source},
        clone::CloneKind,
        Holder,
    },
};
use flotilla_schema::{Kind, KindRegistry, Ref, Timestamp};
use flotilla_schema_sqlite::{Store, StoreHandle};
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory, Home, Outcome};

const HOME: &str = "home";

fn store() -> Arc<StoreHandle> {
    let mut registry = KindRegistry::new();
    stored::register(&mut registry).expect("registry");
    Arc::new(StoreHandle::new(Store::open(rusqlite::Connection::open_in_memory().expect("memory"), HOME, registry).expect("store")))
}

fn checkout(name: &str, environment: &str, convoy: &str) -> CheckoutRequest {
    CheckoutRequest {
        launch_of: name.into(),
        launch: 0,
        repository: "repo".into(),
        environment: Ref::new(environment.to_owned(), HOME),
        branch: "feature".into(),
        base: None,
        path: "/w".into(),
        source: Source::Worktree,
        clone: Some(Ref::new("clone-a", HOME)),
        holder: Some(Holder::convoy(convoy)),
    }
}

fn failure(detail: &str) -> Step {
    Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, detail), retryable: true })
}

// Behaviour (#3629 layer 4): each view is a keyed or indexed read judged at read time. A
// requested checkout is in progress; after a failure it is retrying, due at the failure plus the
// policy's backoff; realised it is ready under its identity; Gone is lost when it vanished and
// released when torn down, as recorded in its stored ending.
#[test]
fn views_are_read_by_key_and_index_and_judged_per_key() {
    let store = store();
    let checkouts: Arc<dyn Lifecycles<CheckoutKind>> =
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("home")));
    let reads = StoreReads::new(Arc::clone(&store));
    let [a, b, c] =
        [("a", "env-1", "convoy-1"), ("b", "env-1", "convoy-2"), ("c", "env-2", "convoy-1")].map(|(name, environment, convoy)| {
            checkouts.request(name, checkout(name, environment, convoy), Timestamp(1_000_000)).expect("requested").name().to_owned()
        });
    assert_eq!(reads.checkout(&a, HOME).expect("read").expect("held").judgement, Judgement::InProgress);
    assert!(reads.checkout(&a, "elsewhere").expect("read").is_none(), "keyed by home too");

    checkouts.step(&a, &failure("worktree add failed"), Timestamp(2_000_000)).expect("attempt");
    match reads.checkout(&a, HOME).expect("read").expect("held").judgement {
        Judgement::Retrying { failures: 1, next, last } => {
            assert_eq!(next, Timestamp(3_000_000), "Checkout's default policy: one second after the first failure");
            assert_eq!(last.detail, "worktree add failed");
        }
        other => panic!("{other:?}"),
    }

    checkouts.step(&b, &Step::Realised { identity: "/w".into() }, Timestamp(3)).expect("realised");
    checkouts
        .step(&b, &Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::lost() }, Timestamp(4))
        .expect("vanished");
    checkouts.step(&c, &Step::Released, Timestamp(4)).expect("released");
    assert_eq!(reads.checkout(&b, HOME).expect("read").expect("held").judgement, Judgement::Lost { reason: None });
    assert_eq!(reads.checkout(&c, HOME).expect("read").expect("held").judgement, Judgement::Released);

    let names = |views: Vec<flotilla_provisioning::engine::reads::View<CheckoutKind>>| {
        let mut names: Vec<String> = views.into_iter().map(|view| view.name).collect();
        names.sort();
        names
    };
    assert_eq!(names(reads.environment_checkouts("env-1", HOME).expect("index")), [a.clone(), b.clone()]);
    assert_eq!(names(reads.held_checkouts(&Holder::convoy("convoy-1")).expect("index")), [a.clone(), c.clone()]);
    assert!(reads.held_checkouts(&Holder::vessel("convoy-1")).expect("index").is_empty(), "the holder's kind is part of it");
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
    assert_eq!(judge::<CheckoutKind>(&LifecycleState::requested()), Judgement::InProgress);
    assert!(matches!(judge::<CheckoutKind>(&failed(2, true)), Judgement::Retrying { failures: 2, .. }));
    assert_eq!(judge::<CheckoutKind>(&failed(3, true)), Judgement::Exhausted { last: cause.clone() });
    assert_eq!(judge::<CloneKind>(&failed(1, false)), Judgement::Exhausted { last: cause.clone() });
    // A clone tries three times (#3108 ruling): a third retryable failure spends it.
    assert!(matches!(judge::<CloneKind>(&failed(2, true)), Judgement::Retrying { failures: 2, .. }));
    assert_eq!(judge::<CloneKind>(&failed(3, true)), Judgement::Exhausted { last: cause.clone() });
    assert_eq!(judge::<CheckoutKind>(&LifecycleState::GaveUp { cause: cause.clone() }), Judgement::GaveUp { cause });
}

// The stored ending survives snapshot-only replication without any preceding log entry.
#[hegel::test]
fn a_replica_reads_the_ending_from_its_snapshot(tc: hegel::TestCase) {
    use flotilla_state_machines::machines::lifecycle::VanishCause;
    // Covers every ending, with empty, ordinary and Unicode failure reasons.
    let reason = ["", "provider failed", "失敗"][tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(2))];
    let (ending, expected) = match tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(2)) {
        0 => (Step::Released, Judgement::Released),
        1 => (Step::Vanished { cause: VanishCause::lost() }, Judgement::Lost { reason: None }),
        _ => (Step::Vanished { cause: VanishCause::Failed { reason: reason.into() } }, Judgement::Failed { reason: reason.into() }),
    };
    let origin = store();
    let checkouts: Arc<dyn Lifecycles<CheckoutKind>> =
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&origin))).expect("home")));
    let a = checkouts.request("a", checkout("a", "env-1", "convoy-1"), Timestamp(1)).expect("requested").name().to_owned();
    checkouts.step(&a, &Step::Realised { identity: "/w".into() }, Timestamp(2)).expect("realised");
    checkouts.step(&a, &ending, Timestamp(3)).expect("released");

    let replica_of = || {
        let mut registry = KindRegistry::new();
        stored::register(&mut registry).expect("registry");
        Arc::new(StoreHandle::new(
            Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "replica", registry).expect("store"),
        ))
    };
    let followed = replica_of();
    {
        let (events, changes) = {
            let mut origin = origin.lock().expect("origin");
            let mut read = origin.read().expect("read");
            let events = read.changes_since(HOME, 0, 100).expect("changes");
            let changes = read.changes_of(&events).expect("their changes");
            (events, changes)
        };
        let mut store = followed.lock().expect("replica");
        let mut write = store.write().expect("write");
        // Each entry's change first, as the stream sends it (#4024).
        changes.iter().for_each(|change| write.record_change(change).expect("change"));
        for event in &events {
            write.apply(event).expect("applied");
        }
        write.commit().expect("commit");
    }
    assert_eq!(StoreReads::new(followed).checkout(&a, HOME).expect("read").expect("held").judgement, expected);

    let seeded = replica_of();
    {
        let snapshot = {
            let mut store = origin.lock().expect("origin");
            let mut read = store.read().expect("read");
            let id = read
                .resolve(
                    flotilla_state_machines::Resource::<flotilla_state_machines::machines::lifecycle::Lifecycle<CheckoutKind>>::CODE,
                    &a,
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
    assert_eq!(StoreReads::new(seeded).checkout(&a, HOME).expect("read").expect("held").judgement, expected);
}

// Behaviour (#3719 look-over ruling 4): a vessel finds its environment through the holder index,
// not by a naming rule.
#[test]
fn a_vessel_finds_its_environment_by_its_holder() {
    use flotilla_provisioning::stored::environment::{EnvironmentKind, EnvironmentRequest, Recipe};
    let store = store();
    let environments: Arc<dyn Lifecycles<EnvironmentKind>> =
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("home")));
    let request = |holder: Option<Holder>| EnvironmentRequest {
        provider: "host:feta/provider:docker".parse().expect("provider"),
        image_build: None,
        recipe: flotilla_schema::Frozen::new(Recipe::of(&serde_json::json!({ "image": "x" })).expect("recipe")).expect("freeze"),
        holder,
    };
    environments.request("crew-env", request(Some(Holder::vessel("convoy-a-implement"))), Timestamp(1)).expect("requested");
    environments.request("tools", request(None), Timestamp(1)).expect("requested");
    let reads = StoreReads::new(store);
    let found = reads.held_environments(&Holder::vessel("convoy-a-implement")).expect("index");
    assert_eq!(found.iter().map(|view| view.name.as_str()).collect::<Vec<_>>(), ["crew-env"]);
}

// Behaviour (#3629 layer 8): a terminal session is read by its record name as its current launch,
// the newest by the `terminal_session.by_launch` index, and a vessel's sessions through the
// `terminal_session.by_holder` index.
#[test]
fn a_session_reads_as_its_current_launch() {
    use flotilla_provisioning::stored::terminal_session::{Launch, TerminalSessionKind, TerminalSessionRequest};
    let store = store();
    let sessions: Arc<dyn Lifecycles<TerminalSessionKind>> =
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("home")));
    let name = "terminal-vessel-a-coder";
    let request = || TerminalSessionRequest {
        launch_of: name.into(),
        launch: 0,
        environment: Ref::new("env-a", HOME),
        holder: Some(Holder::vessel("vessel-a")),
        role: "coder".into(),
        pool: "cleat".into(),
        cwd: "/w".into(),
        env: Default::default(),
        source: Launch::Tool { command: "bash".into() },
    };
    let first = sessions.request(name, request(), Timestamp(1)).expect("first launch").name().to_owned();
    sessions.step(&first, &Step::Realised { identity: "s1".into() }, Timestamp(2)).expect("ran");
    sessions.step(&first, &Step::Released, Timestamp(3)).expect("stopped");
    let second = sessions.replace_launch(name, &first, request(), Timestamp(4)).expect("second launch");
    sessions.step(&second, &Step::Realised { identity: "s2".into() }, Timestamp(5)).expect("ran again");

    let reads = StoreReads::new(store);
    let current = reads.terminal_session(name, HOME).expect("read").expect("held");
    assert_eq!(current.name, second);
    assert_eq!(current.judgement, Judgement::Ready { identity: "s2".into() });
    assert!(reads.terminal_session("terminal-vessel-b-coder", HOME).expect("read").is_none());
    let mut launches: Vec<String> =
        reads.held_sessions(&Holder::vessel("vessel-a")).expect("index").into_iter().map(|view| view.name).collect();
    launches.sort();
    let mut expected = vec![first, second.clone()];
    expected.sort();
    assert_eq!(launches, expected);

    // Found however many launches there are, numbered with the gaps the home's other writes leave.
    let mut latest = second;
    for _ in 0..9 {
        sessions.step(&latest, &Step::Released, Timestamp(10)).expect("stopped");
        latest = sessions.replace_launch(name, &latest, request(), Timestamp(11)).expect("launch");
        assert_eq!(reads.terminal_session(name, HOME).expect("read").expect("held").name, latest);
    }
}

// Behaviour (#3629 layer 8, #3108): a vessel's launch reads by its launch name, never by its
// work's bare name; Ready once realised and Lost once it vanished; and a work's launches through
// the `vessel.by_launch` index, live or ended, oldest first. (Reading a vessel by its old record's
// name went with the old Vessel's binding.)
#[test]
fn a_vessel_reads_as_its_lease() {
    use flotilla_protocol::LifecycleReading;
    use flotilla_provisioning_testkit::VesselsFixture;
    use flotilla_resources::vessel_records::{VesselRecords, VesselStep};

    let fixture = VesselsFixture::at(HOME);
    let implement = fixture.request("convoy-a", "implement", "feta");
    fixture.ready(&implement, "env-a");
    let review = fixture.request("convoy-a", "review", "feta");
    fixture.ready(&review, "env-b");
    fixture.step(&review, VesselStep::Lost { reason: "container gone".into() });
    let replacement = fixture.request("convoy-a", "review", "feta");

    let read = fixture.vessels.get(&implement, None).expect("read").expect("held");
    assert!(implement.starts_with("convoy-a-implement#"), "{implement} is a launch of its work");
    assert_eq!(read.reading, LifecycleReading::Ready { identity: "env-a".into() });
    assert_eq!(read.host, "feta");
    assert_eq!(
        fixture.vessels.get(&review, None).expect("read").expect("held").reading,
        LifecycleReading::Lost { reason: Some("container gone".into()) }
    );
    assert!(fixture.vessels.get("convoy-a-implement", None).expect("read").is_none(), "no read by the bare work name");
    let launches: Vec<String> =
        fixture.vessels.launches("convoy-a", HOME, "convoy-a-review", None).expect("index").into_iter().map(|launch| launch.name).collect();
    assert_eq!(launches, [review, replacement]);
}

// Corrupt persisted retry times remain readable in debug and release builds; diagnostics preserve a usable fallback.
#[test]
fn out_of_range_retry_times_do_not_panic() {
    use flotilla_protocol::LifecycleReading;
    for micros in [i64::MIN, i64::MAX] {
        let reading =
            Judgement::Retrying { failures: 1, next: Timestamp(micros), last: Cause::new(CauseCategory::Transient, "retry") }.reading();
        assert!(matches!(reading, LifecycleReading::Retrying { next, .. } if next == chrono::DateTime::UNIX_EPOCH));
    }
}
