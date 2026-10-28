//! The provisioning read port over the decided store (#3629 layer 4): keyed and indexed reads,
//! and the judgements made per key at read time.

use std::sync::{Arc, Mutex};

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
    checkouts
        .step("b", &Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::Lost }, Timestamp(4))
        .expect("vanished");
    checkouts.step("c", &Step::Released, Timestamp(4)).expect("released");
    assert_eq!(reads.checkout("b", HOME).expect("read").expect("held").judgement, Judgement::Lost);
    assert_eq!(reads.checkout("c", HOME).expect("read").expect("held").judgement, Judgement::Released);

    let names = |views: Vec<flotilla_provisioning::engine::reads::View<CheckoutKind>>| {
        let mut names: Vec<String> = views.into_iter().map(|view| view.name).collect();
        names.sort();
        names
    };
    assert_eq!(names(reads.environment_checkouts("env-1", HOME).expect("index")), ["a", "b"]);
    assert_eq!(names(reads.held_checkouts(&Holder::convoy("convoy-1")).expect("index")), ["a", "c"]);
    assert!(reads.held_checkouts(&Holder::ensure("convoy-1")).expect("index").is_empty(), "the holder's kind is part of it");
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
    assert!(matches!(judge::<CloneKind>(&failed(50, true)), Judgement::Retrying { .. }), "a clone retries while wanted");
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
        1 => (Step::Vanished { cause: VanishCause::Lost }, Judgement::Lost),
        _ => (Step::Vanished { cause: VanishCause::Failed { reason: reason.into() } }, Judgement::Failed { reason: reason.into() }),
    };
    let origin = store();
    let checkouts: Arc<dyn Lifecycles<CheckoutKind>> =
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&origin))).expect("home")));
    checkouts.request("a", checkout("env-1", "convoy-1"), Timestamp(1)).expect("requested");
    checkouts.step("a", &Step::Realised { identity: "/w".into() }, Timestamp(2)).expect("realised");
    checkouts.step("a", &ending, Timestamp(3)).expect("released");

    let replica_of = || {
        let mut registry = KindRegistry::new();
        stored::register(&mut registry).expect("registry");
        Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "replica", registry).expect("store")))
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
    assert_eq!(StoreReads::new(followed).checkout("a", HOME).expect("read").expect("held").judgement, expected);

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
    assert_eq!(StoreReads::new(seeded).checkout("a", HOME).expect("read").expect("held").judgement, expected);
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
// the latest `#<n>`, and a vessel's sessions through the `terminal_session.by_holder` index.
#[test]
fn a_session_reads_as_its_current_launch() {
    use flotilla_provisioning::{
        engine::launch,
        stored::terminal_session::{Launch, TerminalSessionKind, TerminalSessionRequest},
    };
    let store = store();
    let sessions: Arc<dyn Lifecycles<TerminalSessionKind>> =
        Arc::new(LifecycleHome::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("home")));
    let request = || TerminalSessionRequest {
        environment: Ref::new("env-a", HOME),
        holder: Some(Holder::vessel("vessel-a")),
        role: "coder".into(),
        pool: "cleat".into(),
        cwd: "/w".into(),
        env: Default::default(),
        source: Launch::Tool { command: "bash".into() },
    };
    let name = "terminal-vessel-a-coder";
    sessions.request(&launch(name, 1), request(), Timestamp(1)).expect("launch 1");
    sessions.step(&launch(name, 1), &Step::Realised { identity: "s1".into() }, Timestamp(2)).expect("ran");
    sessions.step(&launch(name, 1), &Step::Released, Timestamp(3)).expect("stopped");
    sessions.request(&launch(name, 2), request(), Timestamp(4)).expect("launch 2");
    sessions.step(&launch(name, 2), &Step::Realised { identity: "s2".into() }, Timestamp(5)).expect("ran again");

    let reads = StoreReads::new(store);
    let current = reads.terminal_session(name, HOME).expect("read").expect("held");
    assert_eq!(current.name, launch(name, 2));
    assert_eq!(current.judgement, Judgement::Ready { identity: "s2".into() });
    assert!(reads.terminal_session("terminal-vessel-b-coder", HOME).expect("read").is_none());
    let mut launches: Vec<String> =
        reads.held_sessions(&Holder::vessel("vessel-a")).expect("index").into_iter().map(|view| view.name).collect();
    launches.sort();
    assert_eq!(launches, [launch(name, 1), launch(name, 2)]);

    // Found however many launches there are (#3748 review): doubling past the latest, halving back.
    for n in 3..=11 {
        sessions.request(&launch(name, n), request(), Timestamp(10 + i64::from(n))).expect("launch");
        assert_eq!(reads.terminal_session(name, HOME).expect("read").expect("held").name, launch(name, n));
    }
    // A missing launch bounds the contiguous-prefix search; a later orphan is not the current launch.
    sessions.request(&launch(name, 13), request(), Timestamp(23)).expect("orphan launch");
    assert_eq!(reads.terminal_session(name, HOME).expect("read").expect("held").name, launch(name, 11));
}

// Behaviour (#3629 layer 8, Vessel): a vessel's lease reads by its name, Ready once realised and
// Lost once it vanished, and a convoy's vessels through the `vessel.by_convoy` index, live or ended.
#[test]
fn a_vessel_reads_as_its_lease() {
    use std::collections::BTreeMap;

    use flotilla_provisioning::engine::vessel::{PlacementLifecycles, PINNED_CLASS};
    use flotilla_state_machines::{
        machines::{
            convoy::{ConvoyId, Priority, VesselClass},
            vessel::{VesselKind, VesselSpec},
        },
        placement::Host,
    };
    // The crew kinds' store, which holds the Vessel kind.
    let registry = stored::crew_registry().expect("registry");
    let store = Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().expect("memory"), HOME, registry).expect("store")));
    let vessels: Arc<dyn Lifecycles<VesselKind>> = Arc::new(
        PlacementLifecycles::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("home")).expect("placements"),
    );
    let lease = || VesselSpec {
        convoy: ConvoyId::new("convoy-a", HOME),
        class: VesselClass::new(PINNED_CLASS),
        shape: BTreeMap::new(),
        priority: Priority(0),
        host: Host::new("feta"),
    };
    vessels.request("convoy-a-implement", lease(), Timestamp(1)).expect("granted");
    vessels.step("convoy-a-implement", &Step::Realised { identity: "env-a".into() }, Timestamp(2)).expect("ready");
    vessels.request("convoy-a-review", lease(), Timestamp(3)).expect("granted");
    vessels.step("convoy-a-review", &Step::Realised { identity: "env-b".into() }, Timestamp(4)).expect("ready");
    vessels
        .step("convoy-a-review", &Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::Lost }, Timestamp(5))
        .expect("lost");

    let reads = StoreReads::new(store);
    let implement = reads.vessel("convoy-a-implement", HOME).expect("read").expect("held");
    assert_eq!(implement.judgement, Judgement::Ready { identity: "env-a".into() });
    assert_eq!(implement.request.host, Host::new("feta"));
    assert_eq!(reads.vessel("convoy-a-review", HOME).expect("read").expect("held").judgement, Judgement::Lost);
    assert!(reads.vessel("convoy-b-implement", HOME).expect("read").is_none());
    let mut convoy: Vec<String> =
        reads.convoy_vessels(&ConvoyId::new("convoy-a", HOME)).expect("index").into_iter().map(|view| view.name).collect();
    convoy.sort();
    assert_eq!(convoy, ["convoy-a-implement", "convoy-a-review"]);
}

// Behaviour (#3771 review): the port reads a vessel by its old record's name, because the write
// path names the engine's resource after it. Decided through the binding, read back by
// `metadata.name`.
#[test]
fn a_vessel_written_through_the_binding_reads_by_its_old_name() {
    use std::collections::BTreeMap;

    use flotilla_protocol::{CanonicalHostId, PlacementDecision, PlacementTargetHost};
    use flotilla_provisioning::engine::{
        decide,
        vessel::{PlacementLifecycles, VesselBinding},
    };
    use flotilla_resources::{ObjectMeta, ResourceObject, StatusPatch, VesselSpec as OldVesselSpec, VesselStatusPatch};
    use flotilla_state_machines::machines::vessel::VesselKind;
    let registry = stored::crew_registry().expect("registry");
    let store = Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().expect("memory"), HOME, registry).expect("store")));
    let vessels: Arc<dyn Lifecycles<VesselKind>> = Arc::new(
        PlacementLifecycles::new(HOME, Home::with_log(HOME, StoreLog::new(Arc::clone(&store))).expect("home")).expect("placements"),
    );
    let mut obj = ResourceObject {
        metadata: ObjectMeta {
            name: "convoy-a-implement".into(),
            namespace: "flotilla".into(),
            resource_version: "1".into(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: vec![],
            finalizers: vec![],
            deletion_timestamp: None,
            creation_timestamp: chrono::DateTime::UNIX_EPOCH,
            merge: None,
        },
        spec: OldVesselSpec {
            convoy_ref: "convoy-a".into(),
            vessel_name: "implement".into(),
            fulfilment_kind_ref: "docker".into(),
            adopted_checkout_refs: BTreeMap::new(),
        },
        status: None,
    };
    let provisioning = VesselStatusPatch::MarkProvisioning {
        observed_kind_ref: "docker".into(),
        observed_kind_version: "1".into(),
        placement_decision: Some(
            PlacementDecision::builder()
                .kind("docker".into())
                .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved("feta"), display_name: "feta".into() })
                .build(),
        ),
        started_at: chrono::DateTime::UNIX_EPOCH,
        message: None,
        image: None,
    };
    let decided = decide::<VesselBinding>(vessels.as_ref(), &obj, Some(provisioning), chrono::Utc::now()).patch.expect("granted");
    decided.apply(obj.status.get_or_insert_with(Default::default));

    let reads = StoreReads::new(store);
    let view = reads.vessel(&obj.metadata.name, HOME).expect("read").expect("read by the old record's name");
    assert_eq!(view.name, obj.metadata.name);
    assert_eq!(view.judgement, Judgement::InProgress);

    // Failure is visible only through the engine: this snapshot deliberately receives no status patch.
    let failure = VesselStatusPatch::MarkFailed { message: "provider refused provisioning".into() };
    let decision = decide::<VesselBinding>(vessels.as_ref(), &obj, Some(failure), chrono::Utc::now());
    assert!(decision.ended);
    let view = reads.vessel(&obj.metadata.name, HOME).expect("read").expect("ended vessel");
    assert!(matches!(view.judgement, Judgement::GaveUp { ref cause } if cause.detail == "provider refused provisioning"));
    assert_eq!(obj.status.as_ref().expect("provisioning").phase, flotilla_resources::VesselPhase::Provisioning);
    assert_eq!(obj.status.as_ref().expect("provisioning").message, None);
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
