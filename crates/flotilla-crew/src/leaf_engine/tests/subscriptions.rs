use super::*;
use flotilla_resources::ResumeIntent;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;

/// A caller's wait on launch `launch` failing.
fn overload_row(connection_id: uuid::Uuid, launch: &str) -> LeafSubscriptionRow {
    LeafSubscriptionRow {
        id: uuid::Uuid::new_v4(),
        namespace: "flotilla".into(),
        leaves: vec![leaf(LeafAddress::Convoy { name: launch.into() }, ".status.phase", "Failed")],
        watcher: LeafWatcher::WaitCaller { connection_id },
        maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
        freshness_demand: None,
        created_at: Utc::now(),
        episode_key: EpisodeKeyFields::default(),
    }
}

async fn overload_demand(table: &LeafSubscriptionTable, id: uuid::Uuid) {
    table
        .inner
        .change_requests
        .demand(
            id,
            ChangeRequestKey::new(
                "flotilla",
                flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 1, &[]).expect("address"),
            )
            .expect("key"),
            None,
        )
        .await
        .expect("refresher demand");
    assert_eq!(table.inner.change_requests.active_demands().await, 1);
}

/// 300 writes to one change request record, outrunning the store's bounded watch ring. A launch's
/// view is fed by snapshots, which never overrun, so the leaf's watches overflow on the
/// change requests it also follows.
async fn overload_store(backend: &ResourceBackend) {
    let records = backend.using::<ChangeRequest>("flotilla");
    let name = cr_name("github.com", "org/load", 1);
    let spec = |authority: String| {
        flotilla_resources::ChangeRequestSpec::builder()
            .service("github.com".into())
            .scope("org/load".into())
            .number(1)
            .observing_authority(authority)
            .build()
    };
    let mut object = match records.get(&name).await {
        Ok(object) => object,
        Err(_) => records.create(&InputMeta::builder().name(name.clone()).build(), &spec("load".into())).await.expect("load record"),
    };
    for index in 0..300 {
        object = records
            .update(&InputMeta::from(&object.metadata), &object.metadata.resource_version, &spec(format!("load-{index}")))
            .await
            .expect("overload write");
    }
}

/// Polls `watching` until the table has evaluated `count` rows, none firing. A launch is read
/// from the engine on a blocking thread, so a row's first evaluation takes more than one poll.
async fn evaluated<F: std::future::Future>(table: &LeafSubscriptionTable, watching: &mut std::pin::Pin<Box<F>>, count: usize) {
    use futures::FutureExt;
    tokio::time::timeout(Duration::from_secs(2), async {
        while table.inner.evaluations.load(Ordering::SeqCst) < count {
            assert!(watching.as_mut().now_or_never().is_none(), "the wait does not fire yet");
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the rows are evaluated");
}

/// Yields until the table's router has lost `count` watches.
async fn lost(table: &LeafSubscriptionTable, count: usize) {
    for _ in 0..100_000 {
        if table.inner.routing.lock().await.watch_losses >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the router never lost watch {count}");
}

/// Yields until the table's router has resynced `count` times.
async fn resynced(table: &LeafSubscriptionTable, count: usize) {
    for _ in 0..100_000 {
        if table.inner.routing.lock().await.resyncs >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the router never resynced {count} times");
}

/// One event on launch `launch`: its home notes a branch scan at `step` seconds.
async fn touch(home: &EngineHome, backend: &ResourceBackend, launch: &str, step: i64) {
    home.note(backend, launch, |status| {
        status.branch_subject_scan_at = Some(DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(step))
    })
    .await;
}

#[test]
fn reconciler_row_identity_ignores_regenerated_freshness_instant() {
    let row = |freshness_demand| LeafSubscriptionRow {
        id: uuid::Uuid::nil(),
        namespace: "flotilla".to_string(),
        leaves: vec!["cr/github.com/flotilla-org/flotilla/1699 .state == merged".parse().expect("leaf")],
        watcher: LeafWatcher::ReconcilerWake { convoy: "landing".to_string() },
        maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
        freshness_demand: Some(freshness_demand),
        created_at: freshness_demand,
        episode_key: EpisodeKeyFields::default(),
    };

    assert!(same_standing_row(
        &row("2026-08-23T14:00:00Z".parse().expect("first instant")),
        &row("2026-08-23T14:01:00Z".parse().expect("second instant")),
    ));
}

// A live wait must survive queue overflow and evaluate the current state
// after relisting, with its original subscription identity intact.
#[tokio::test]
async fn leaf_wait_recovers_after_watch_overflow() {
    let (backend, home) = engine_backend();
    let table = supervision_wake(&backend, &home).subscriptions;
    let busy = launch_at(&backend, &home, "busy", ledger_owed(), ConvoyPhase::Active).await;
    let row = overload_row(uuid::Uuid::new_v4(), &busy);
    let id = row.id;
    table.inner.rows.lock().await.insert(id, row.clone());
    let mut watching = Box::pin(table.watch_row(row));
    // Poll through watch registration and snapshot loading, then deliberately
    // stop polling while writes outrun the bounded ring.
    evaluated(&table, &mut watching, 1).await;
    overload_store(&backend).await;
    overload_store(&backend).await;
    home.end(&backend, &busy, ConvoyPhase::Failed).await;
    tokio::time::timeout(Duration::from_secs(2), watching).await.expect("wait recovers").expect("watch succeeds");
    assert!(!table.rows().await.iter().any(|row| row.id == id), "recovered wait fires and releases its row");
    assert!(table.inner.last_firings.lock().await.is_empty(), "one-shot wait releases firing state");
}

// Permanent overload must bound addressed snapshot work, retain identity
// and accounting during sleeps, then recover from the latest state.
#[tokio::test(start_paused = true)]
async fn leaf_sustained_overload_bounds_snapshot_work_and_recovers() {
    use futures::FutureExt;
    let (backend, home) = engine_backend();
    let table = supervision_wake(&backend, &home).subscriptions;
    let busy = launch_at(&backend, &home, "busy", ledger_owed(), ConvoyPhase::Active).await;
    let row = overload_row(uuid::Uuid::new_v4(), &busy);
    let id = row.id;
    table.inner.last_firings.lock().await.insert(
        (id, row.leaves[0].clone()),
        LeafFiringRecord { leaf: row.leaves[0].clone(), value: "Active".into(), fired_at: Utc::now() },
    );
    table.inner.unable_since.lock().await.insert(id, (UnableEvidenceKey::Absent, Utc::now()));
    table.inner.stale_attention_reported.lock().await.insert(id);
    overload_demand(&table, id).await;
    table.inner.rows.lock().await.insert(id, row.clone());
    let mut watching = Box::pin(table.watch_row(row));
    assert!(watching.as_mut().now_or_never().is_none());
    // Two simulated seconds in 20ms ticks, overrunning every watch the router
    // opens with a 300-write burst at the next tick, and letting the leaf run
    // between bursts. A router sleeping in backoff holds no watch, so writes then
    // could not reach it. Count addressed snapshots, not RSS.
    for _ in 0..100 {
        let routing = table.inner.routing.lock().await;
        let watching_live = routing.watch_losses == routing.resyncs;
        drop(routing);
        if watching_live {
            overload_store(&backend).await;
        }
        for _ in 0..2 {
            assert!(watching.as_mut().now_or_never().is_none());
        }
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    let snapshots = table.inner.snapshot_loads.load(Ordering::SeqCst);
    let resyncs = table.inner.routing.lock().await.resyncs;
    // Backoff from 25ms reaches its one-second cap after seven resyncs, within the two seconds.
    assert!(resyncs >= 6, "sustained overload keeps expiring reopened watches: {resyncs}");
    assert!(resyncs <= 10, "repeated expiry must throttle shared resyncs: {resyncs}");
    assert_eq!(table.inner.store_reads.load(Ordering::SeqCst), snapshots, "recovery reads only the addressed convoy");
    assert!(table.rows().await.iter().any(|row| row.id == id));
    assert_eq!(table.inner.change_requests.active_demands().await, 1, "backoff retains demand");
    assert_eq!(table.inner.last_firings.lock().await.len(), 1, "backoff retains firing history");
    assert!(table.inner.unable_since.lock().await.contains_key(&id), "backoff retains episode accounting");
    assert!(table.inner.stale_attention_reported.lock().await.contains(&id));
    home.end(&backend, &busy, ConvoyPhase::Failed).await;
    tokio::time::timeout(Duration::from_secs(2), watching).await.expect("bounded recovery latency").expect("watch succeeds");
    assert!(table.rows().await.is_empty());
    assert_eq!(table.inner.change_requests.active_demands().await, 0, "completion releases demand");
    assert!(table.inner.last_firings.lock().await.is_empty());
    assert!(table.inner.unable_since.lock().await.is_empty());
    assert!(table.inner.stale_attention_reported.lock().await.is_empty());
}

// Connection cancellation must abort a sleeping recovery task and release
// row/firing state immediately rather than waiting for the recovery timer.
#[tokio::test(start_paused = true)]
async fn leaf_overload_backoff_is_cancelled_by_disconnect() {
    let (backend, home) = engine_backend();
    let table = supervision_wake(&backend, &home).subscriptions;
    let busy = launch_at(&backend, &home, "busy", ledger_owed(), ConvoyPhase::Active).await;
    let connection_id = uuid::Uuid::new_v4();
    let row = overload_row(connection_id, &busy);
    let id = row.id;
    overload_demand(&table, id).await;
    table.inner.rows.lock().await.insert(id, row.clone());
    let watching_table = table.clone();
    let mut watching = Box::pin(async move { watching_table.watch_row(row).await });
    evaluated(&table, &mut watching, 1).await;
    // Disable cooperative yielding so the burst deterministically overruns the ring.
    tokio::task::unconstrained(overload_store(&backend)).await;
    resynced(&table, 1).await;
    assert_eq!(table.inner.routing.lock().await.watch_losses, 1);
    assert_eq!(table.inner.routing.lock().await.resyncs, 1, "first expiry resyncs without advancing time");
    evaluated(&table, &mut watching, 2).await;
    // Disable cooperative yielding so the burst deterministically overruns the ring.
    tokio::task::unconstrained(overload_store(&backend)).await;
    lost(&table, 2).await;
    assert_eq!(table.inner.routing.lock().await.watch_losses, 2);
    assert_eq!(table.inner.routing.lock().await.resyncs, 1, "second expiry enters backoff");
    tokio::time::advance(Duration::from_millis(24)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(table.inner.routing.lock().await.resyncs, 1, "25ms backoff has not elapsed");
    tokio::time::advance(Duration::from_millis(1)).await;
    resynced(&table, 2).await;
    assert_eq!(table.inner.routing.lock().await.resyncs, 2, "second expiry resyncs at 25ms");
    evaluated(&table, &mut watching, 3).await;
    // Disable cooperative yielding so the burst deterministically overruns the ring.
    tokio::task::unconstrained(overload_store(&backend)).await;
    lost(&table, 3).await;
    assert_eq!(table.inner.routing.lock().await.watch_losses, 3);
    assert_eq!(table.inner.routing.lock().await.resyncs, 2, "third expiry sleeps until cancellation");
    let snapshots = table.inner.snapshot_loads.load(Ordering::SeqCst);
    let task = tokio::spawn(async move {
        watching.await.expect("watch succeeds");
    });
    let abort = task.abort_handle();
    table.inner.tasks.lock().await.insert(id, task);
    table.unsubscribe_connection(connection_id).await;
    tokio::task::yield_now().await;
    assert!(abort.is_finished(), "disconnect cancels the pending sleep");
    assert!(table.rows().await.is_empty());
    assert_eq!(table.inner.change_requests.active_demands().await, 0, "completion releases demand");
    assert!(table.inner.last_firings.lock().await.is_empty());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(table.inner.snapshot_loads.load(Ordering::SeqCst), snapshots, "cancelled leaf does not reload");
}

// Generated healthy/overloaded intervals cross the reset boundary; retry
// delay never exceeds one second and a healthy interval restores immediacy.
#[hegel::test]
fn leaf_recovery_budget_resets_after_healthy_watch(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(30));
    let mut recovery = LeafWatchRecovery::default();
    for _ in 0..steps {
        let healthy_ms = tc.draw(gs::integers::<u64>().min_value(0).max_value(6000));
        let delay = recovery.expired(Duration::from_millis(healthy_ms));
        assert!(delay <= Duration::from_secs(1));
        if healthy_ms >= 5000 {
            assert!(delay.is_zero());
        }
    }
    assert!(recovery.expired(Duration::from_secs(5)).is_zero());
}

#[tokio::test]
async fn missing_resumed_session_does_not_suppress_supervision_forever() {
    let case = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    case.note(|status| {
        flotilla_resources::external_patches::resume_crew_work(
            "work".into(),
            "coder".into(),
            Utc::now() - chrono::Duration::minutes(3),
            "Continue with the operator's guidance".into(),
            Some("lost-resume-brief".into()),
            ResumeIntent::Reopen,
        )
        .apply(status)
    })
    .await;
    let row_id = *case.wake.subscriptions.inner.rows.lock().await.keys().next().expect("actor row");
    case.wake
        .subscriptions
        .inner
        .unable_since
        .lock()
        .await
        .insert(row_id, (UnableEvidenceKey::Absent, Utc::now() - chrono::Duration::minutes(3)));
    let governor = case.governor("governor");
    let objects =
        HashMap::from([(case.work.clone(), case.convoy().await), (governor.clone(), case.home.view(&case.backend, &governor).await)]);
    case.wake.judge_stalls("flotilla", &objects).await.expect("judge missing session");
    let status = case.status().await;
    let requests = case.delivery.requests.lock().expect("supervisor requests");
    assert!(!requests.is_empty(), "missing session should be supervised: {:?}", status.stalled);
    assert_eq!(requests[0].convoy, governor);
    // Escalations retain the fully-qualified source crew, including its convoy.
    assert_eq!(requests[0].sender, format!("wheelhouse/{}/work/coder", case.work));
    assert_eq!(requests[0].expectation, flotilla_resources::MessageExpectation::Reply);
}

// Deliberate operator-only or empty policies consume the ladder. Routine
// reconciles must not rewrite the condition or repeatedly retry that decision.
#[tokio::test]
async fn operator_only_supervision_is_consumed_once() {
    for policy in [Vec::new(), vec![SupervisionTarget::Operator]] {
        let mut workflow = supervised_workflow();
        workflow.supervision = Some(policy);
        let case = supervised_case(workflow, &[]).await;
        let convoys = case.backend.using::<Convoy>("flotilla");
        let mut notes = case.home.engine().memory().subscribe();
        for tick in 0..3 {
            let objects =
                convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
            case.wake.sync_rows("flotilla", &objects).await.expect("rows");
            case.wake.judge_stalls("flotilla", &objects).await.expect("judge operator policy");
            let stalled = case.status().await.stalled.expect("stall");
            assert!(stalled.supervision_exhausted);
            assert!(stalled.supervision_index.is_some());
            assert!(stalled.supervisor.is_none());
            assert_eq!(stalled.rung, StallRung::Operator);
            let changed = notes_changed(&mut notes);
            if tick > 0 {
                assert!(!changed, "a consumed ladder is not rewritten");
            }
            assert!(case.delivery.requests.lock().expect("deliveries").is_empty());
        }
    }
}

#[tokio::test]
async fn in_memory_leaf_subscription_contract() {
    assert_leaf_subscription_contract(ResourceBackend::new(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn sqlite_leaf_subscription_contract() {
    assert_leaf_subscription_contract(ResourceBackend::new(SqliteBackend::open_in_memory().expect("open sqlite"))).await;
}

#[tokio::test]
async fn diagnostics_retain_the_last_firing_for_an_armed_reconciler_row() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(4);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".to_string(),
        Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) }),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = served_table(backend, broadcast_test_sink(event_tx), refresher);
    let id = uuid::Uuid::new_v4();
    let fired_leaf = leaf(LeafAddress::Convoy { name: "held".to_string() }, ".status.phase", "Landed");
    table.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".to_string(),
            leaves: vec![fired_leaf.clone()],
            watcher: LeafWatcher::ReconcilerWake { convoy: "held".to_string() },
            maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        },
    );

    table
        .fire(id, LeafFire { subscription_id: uuid::Uuid::nil(), watcher_id: uuid::Uuid::nil(), leaf: fired_leaf, value: "Landed".into() })
        .await;

    let diagnostics = table.diagnostics().await;
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].1.len(), 1);
    assert_eq!(diagnostics[0].1[0].value, "Landed");
}

// #2654: each subscription key logs its ineligible-to-eligible transition;
// duplicate ticks with different crew phases but the same decision stay quiet.
#[test]
fn unchanged_subscription_decisions_log_once() {
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = captured_subscriber(logs.clone(), tracing::Level::DEBUG);
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tracing scenario").block_on(active_checks_scenario(
            &[true],
            false,
            "coder",
        ));
    });
    let output = String::from_utf8(logs.lock().expect("tracing scenario").clone()).expect("tracing scenario");
    let decisions = output
        .lines()
        .filter(|line| line.contains("turn delivery subscription decision") && line.contains("source=checks-settled"))
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 2, "{output}");
    assert!(decisions[0].contains("skip_ineligible_active_crew_or_subject"));
    assert!(decisions[1].contains("arm_eligible_active_crew_and_subject"));
}

#[tokio::test]
async fn declared_exit_entry_name_is_recorded_when_its_instantiated_leaf_fires() {
    let (backend, home) = engine_backend();
    let (event_tx, _) = broadcast::channel(16);
    let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_millis(20),
        checks_pending: Duration::from_millis(20),
        freshness_demanded: Duration::from_millis(20),
        stale_after: Duration::from_secs(60),
    };
    let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source.clone(), cadence);
    let table = engine_table(backend.clone(), &home, broadcast_test_sink(event_tx), refresher);
    let repo_ref = RepositoryKey("repo".to_string());
    let spec = ConvoySpec::builder()
        .workflow_ref("workflow".to_string())
        .repositories(vec![ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".to_string())
            .repo_ref(repo_ref.clone())
            .source_ref("feature/custom-disposition".to_string())
            .target_ref("main".to_string())
            .workspace_slug("flotilla".to_string())
            .subpaths(Vec::new())
            .build()])
        .change_request(BoundChangeRequest::builder().id("1391".to_string()).repository_ref(repo_ref).title("custom".to_string()).build())
        .build();
    // The coder kept the ledger it owed under the custom exit: the launch is landing.
    let mut status = ledger_owed();
    status.workflow_snapshot.as_mut().expect("workflow").exit = Some(ExitDeclaration::Table(indexmap::IndexMap::from([(
        "shipped".to_string(),
        flotilla_resources::parse_leaf_template("$cr.state == merged").expect("custom leaf template"),
    )])));
    let launch = home.admit("custom", spec, status).await;
    home.vessel(&launch, "work", "env");
    home.keep(&backend, &launch, "work", "coder").await;
    let convoys = backend.using::<Convoy>("flotilla");
    assert_eq!(convoys.get(&launch).await.expect("convoy").status.expect("status").phase, ConvoyPhase::Landing);

    let controller = tokio::spawn(
        ControllerLoop {
            primary: convoys.clone(),
            secondaries: vec![table.reconciler_wake_watch()],
            reconciler: ConvoyReconciler::new()
                .with_lifecycle_reads(home.reads())
                .with_engine(home.path())
                .with_change_requests(backend.including_replicas::<ChangeRequest>("flotilla"), cadence.stale_after),
            backend: backend.clone(),
        }
        .run(),
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !table.rows().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("custom exit should instantiate at binding");

    source.merged.store(true, Ordering::SeqCst);
    let settled = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let convoy = convoys.get(&launch).await.expect("convoy");
            let status = convoy.status.expect("status");
            if status.phase == ConvoyPhase::Landed {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("custom exit leaf should settle through the engine");
    assert_eq!(settled.disposition.as_deref(), Some("shipped"));
    controller.abort();
}

// Object events must evaluate only addressed rows. Generated operation sequences
// include unrelated objects and duplicate events; named cases cover namespace
// collisions, absent records, cancellation and restart. Count work, not time.
// Each case admits 64 launches on a real engine store; Hegel charges that setup
// to generation time. Keep the bounded generators and other health checks.
#[hegel::test(suppress_health_check = [hegel::HealthCheck::TooSlow])]
fn leaf_object_routing_counts_only_addressed_rows(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let changes: Vec<usize> = (0..steps).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(63))).collect();
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        routing_count_contract(ResourceBackend::new(InMemoryBackend::default()), &changes).await;
    });
}

// Run the same counted routing contract against the real SQLite adapter.
#[tokio::test]
async fn sqlite_leaf_object_routing_counts_only_addressed_rows() {
    routing_count_contract(ResourceBackend::new(SqliteBackend::open_in_memory().expect("sqlite")), &[63, 0, 1, 1, 32]).await;
}

async fn routing_count_contract(backend: ResourceBackend, changes: &[usize]) {
    routing_contract(backend, changes, false).await;
}

// #3141 / #3120: a warm leaf wake must evaluate from resident subjects.
// Today each affected row reloads its addressed objects; keep the zero budget
// running in CI as an expected failure until the resident redesign lands.
#[tokio::test]
#[should_panic(expected = "zero durable-store read budget exceeded")]
async fn leaf_wake_zero_reads_pending_3120() {
    routing_contract(ResourceBackend::new(SqliteBackend::open_in_memory().expect("sqlite")), &[0, 1, 63], true).await;
}

// #3141 / #3120: attaching a wait to an already warm leaf engine should
// use resident subjects; today even an already watched address is re-read.
#[tokio::test]
#[should_panic(expected = "zero durable-store read budget exceeded")]
async fn leaf_warm_subscription_zero_reads_pending_3120() {
    use flotilla_store_testkit::{ReadCountsBackendExt, ZeroReadGuard};
    use futures::FutureExt;
    let writer = ResourceBackend::new(SqliteBackend::open_in_memory().expect("sqlite"));
    let home = EngineHome::install(&writer, "flotilla", "test-host");
    let watched = launch_at(&writer, &home, "watched", ledger_owed(), ConvoyPhase::Active).await;
    let counted = writer.with_read_counts();
    let table = supervision_wake(&counted, &home).subscriptions;
    let mut watches = Vec::new();
    for index in 0..2 {
        let guard = ZeroReadGuard::after_warm_up(&counted);
        let row = overload_row(uuid::Uuid::new_v4(), &watched);
        table.inner.rows.lock().await.insert(row.id, row.clone());
        let mut watching = Box::pin(table.watch_row(row));
        tokio::time::timeout(Duration::from_secs(5), async {
            while table.inner.evaluations.load(Ordering::SeqCst) <= index {
                assert!(watching.as_mut().now_or_never().is_none(), "Active cannot fire a Failed wait");
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("subscription evaluated");
        watches.push(watching);
        // Index 0 is the cold attach; index 1 attaches to an already warm engine.
        if index != 0 {
            guard.assert_zero("leaf subscription after warm-up");
        }
    }
}

async fn routing_contract(backend: ResourceBackend, changes: &[usize], zero_reads: bool) {
    use flotilla_store_testkit::{ReadCountsBackendExt, ZeroReadGuard};
    use futures::FutureExt;
    let home = EngineHome::install(&backend, "flotilla", "test-host");
    let counted = backend.clone().with_read_counts();
    let mut launches = Vec::new();
    for index in 0..64 {
        launches.push(launch_at(&backend, &home, &format!("object-{index}"), ledger_owed(), ConvoyPhase::Active).await);
    }
    let (events, mut fires) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".into(),
        backend.clone(),
        "authority".into(),
        Arc::new(UnavailableChangeRequests),
        Default::default(),
    );
    let table = engine_table(counted.clone(), &home, broadcast_test_sink(events), refresher);
    let mut watchers = Vec::new();
    let mut ids = Vec::new();
    for index in [0, 1, 2, 0] {
        let mut row = overload_row(uuid::Uuid::new_v4(), &launches[index]);
        // Multiple conditions on one object share a single store dependency.
        let mut another = row.leaves[0].clone();
        another.literal = "Landed".into();
        row.leaves.push(another);
        ids.push(row.id);
        table.inner.rows.lock().await.insert(row.id, row.clone());
        let mut watching = Box::pin(table.watch_row(row));
        assert!(watching.as_mut().now_or_never().is_none());
        watchers.push(watching);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while table.inner.evaluations.load(Ordering::SeqCst) < 4 {
            for watching in &mut watchers {
                assert!(watching.as_mut().now_or_never().is_none());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial addressed reads complete");
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), 4);
    assert_eq!(table.inner.store_reads.load(Ordering::SeqCst), 4, "one targeted read per unique address, no full lists");
    for (step, index) in changes.iter().enumerate() {
        let guard = ZeroReadGuard::after_warm_up(&counted);
        let previous_events = table.inner.routing.lock().await.events_processed;
        let before_evaluations = table.inner.evaluations.load(Ordering::SeqCst);
        let before_reads = table.inner.store_reads.load(Ordering::SeqCst);
        touch(&home, &backend, &launches[*index], step as i64).await;
        router_processed(&table, previous_events).await;
        let touched = if *index == 0 { 2 } else { usize::from(*index < 3) };
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                for watching in &mut watchers {
                    assert!(watching.as_mut().now_or_never().is_none());
                }
                if table.inner.evaluations.load(Ordering::SeqCst) >= before_evaluations + touched {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("addressed evaluations complete");
        assert_eq!(table.inner.evaluations.load(Ordering::SeqCst) - before_evaluations, touched);
        assert_eq!(table.inner.store_reads.load(Ordering::SeqCst) - before_reads, touched);
        assert!(fires.try_recv().is_err());
        if zero_reads {
            guard.assert_zero("leaf wake and subscription delivery");
        }
    }
    // Exactly one wait fires per addressed object, retaining subscription identity.
    for (position, watching) in watchers.iter_mut().enumerate() {
        let index = [0, 1, 2, 0][position];
        if position < 3 {
            home.end(&backend, &launches[index], ConvoyPhase::Failed).await;
        }
        tokio::time::timeout(Duration::from_secs(2), watching).await.expect("fires").expect("watch");
        let DaemonEvent::LeafFired(fire) = fires.try_recv().expect("one fire") else { panic!("leaf fire") };
        assert_eq!(fire.subscription_id, ids[position]);
        assert!(fires.try_recv().is_err());
    }
    assert!(table.rows().await.is_empty());
    assert!(table.inner.routing.lock().await.dependencies.is_empty());
}

async fn router_processed(table: &LeafSubscriptionTable, previous: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while table.inner.routing.lock().await.events_processed == previous {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("router consumes event");
}

// Absent records remain Unknown until admission; Work leaves route via their
// parent launch, and re-arming after restart evaluates current evidence once.
#[tokio::test]
async fn leaf_routing_missing_work_and_restart() {
    let (backend, home) = engine_backend();
    let table = supervision_wake(&backend, &home).subscriptions;
    // A wait can name a launch before it is admitted: admission reserves it first.
    let missing = home.reserve("missing").await;
    let mut row = overload_row(uuid::Uuid::new_v4(), &missing);
    row.leaves = vec![leaf(LeafAddress::Work { convoy: missing.clone(), work: "work".into() }, ".status.phase", "Complete")];
    table.inner.rows.lock().await.insert(row.id, row.clone());
    // The wait runs as its own task: a wait left unpolled mid-evaluation blocks later reads of
    // the launch.
    let watching = tokio::spawn({
        let (table, row) = (table.clone(), row.clone());
        async move { table.watch_row(row).await }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while table.inner.evaluations.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the row is evaluated");
    home.admit_reserved(&missing, convoy_spec(), ledger_owed()).await;
    router_processed(&table, 0).await;
    assert!(!watching.is_finished(), "unstarted work is not Complete");
    // The work's vessel runs and its coder keeps what it owes: the work is complete.
    home.vessel(&missing, "work", "env");
    home.keep(&backend, &missing, "work", "coder").await;
    tokio::time::timeout(Duration::from_secs(2), watching)
        .await
        .expect("parent convoy event arrives")
        .expect("the wait's task")
        .expect("Work leaf fires");
    assert!(table.rows().await.is_empty());
    let restarted = supervision_wake(&backend, &home).subscriptions;
    row.id = uuid::Uuid::new_v4();
    restarted.inner.rows.lock().await.insert(row.id, row.clone());
    tokio::time::timeout(Duration::from_secs(2), restarted.watch_row(row))
        .await
        .expect("restart fires from current level")
        .expect("restarted watch succeeds");
    assert_eq!(restarted.inner.evaluations.load(Ordering::SeqCst), 1);
    assert_eq!(restarted.inner.store_reads.load(Ordering::SeqCst), 1);
    assert!(restarted.rows().await.is_empty());
}

// Watch loss must resync every live row exactly once, even when the overflow
// comes from an unrelated object. Subsequent unrelated events stay unrouted.
#[tokio::test]
async fn leaf_resync_evaluates_each_row_once_then_routes_again() {
    use futures::FutureExt;
    let (backend, home) = engine_backend();
    let mut launches = Vec::new();
    for name in ["busy", "object-0", "object-1"] {
        launches.push(launch_at(&backend, &home, name, ledger_owed(), ConvoyPhase::Active).await);
    }
    let table = supervision_wake(&backend, &home).subscriptions;
    let mut watchers = Vec::new();
    let mut connections = Vec::new();
    for index in 0..2 {
        let connection = uuid::Uuid::new_v4();
        connections.push(connection);
        let row = overload_row(connection, &launches[index + 1]);
        table.inner.rows.lock().await.insert(row.id, row.clone());
        let mut watching = Box::pin(table.watch_row(row));
        evaluated(&table, &mut watching, index + 1).await;
        watchers.push(watching);
    }
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), 2);
    // No task can consume the store's bounded watch ring during this burst.
    tokio::task::unconstrained(overload_store(&backend)).await;
    tokio::task::unconstrained(overload_store(&backend)).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while table.inner.routing.lock().await.resyncs == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shared router recovers");
    for (index, watching) in watchers.iter_mut().enumerate() {
        evaluated(&table, watching, 3 + index).await;
    }
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), 4, "one resync evaluation per row");
    assert_eq!(table.inner.store_reads.load(Ordering::SeqCst), 4, "one addressed read per evaluation");
    let previous = table.inner.routing.lock().await.events_processed;
    touch(&home, &backend, &launches[0], 1).await;
    router_processed(&table, previous).await;
    for watching in &mut watchers {
        assert!(watching.as_mut().now_or_never().is_none());
    }
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), 4, "routing resumes after resync");
    assert_eq!(table.inner.store_reads.load(Ordering::SeqCst), 4);
    for connection in connections {
        table.unsubscribe_connection(connection).await;
    }
    for watching in watchers {
        watching.await.expect("disconnected watcher exits");
    }
    assert!(table.rows().await.is_empty());
}

// A namespace awaiting its store opener must not block another namespace's
// events or cancellation. The pending future stands in for stalled store I/O.
#[tokio::test]
async fn slow_leaf_watch_open_does_not_block_other_namespaces() {
    use futures::FutureExt;
    let (backend, home) = engine_backend();
    let busy = launch_at(&backend, &home, "busy", ledger_owed(), ConvoyPhase::Active).await;
    let table = supervision_wake(&backend, &home).subscriptions;
    let connection = uuid::Uuid::new_v4();
    let row = overload_row(connection, &busy);
    table.inner.rows.lock().await.insert(row.id, row.clone());
    let mut watching = Box::pin(table.watch_row(row));
    evaluated(&table, &mut watching, 1).await;
    let mut slow_row = overload_row(uuid::Uuid::new_v4(), &busy);
    slow_row.namespace = "slow-store".into();
    let previous = table.inner.routing.lock().await.events_processed;
    let mut opening = Box::pin(table.register_routing_with(&slow_row, std::future::pending()));
    assert!(opening.as_mut().now_or_never().is_none());
    touch(&home, &backend, &busy, 1).await;
    router_processed(&table, previous).await;
    evaluated(&table, &mut watching, 2).await;
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), 2);
    tokio::time::timeout(Duration::from_secs(2), table.unsubscribe_connection(connection)).await.expect("independent cancellation");
    watching.await.expect("watcher exits");
    drop(opening);
    assert!(table.inner.routing.lock().await.dependencies.is_empty());
}

// Racing openers must join the same live namespace router. Removing a row
// releases only its dependency entries, leaves its sibling live, and is idempotent.
#[tokio::test]
async fn racing_leaf_openers_share_router_and_remove_only_their_addresses() {
    use futures::FutureExt;
    let (backend, home) = engine_backend();
    let table = supervision_wake(&backend, &home).subscriptions;
    // A wait can name a launch before it is admitted: admission reserves it first.
    let busy = home.reserve("busy").await;
    let row = overload_row(uuid::Uuid::new_v4(), &busy);
    let mut sibling = row.clone();
    sibling.id = uuid::Uuid::new_v4();
    let permit = tokio::sync::Notify::new();
    // Store-I/O seam: release an obsolete candidate after another opener won.
    let mut opening = Box::pin(table.register_routing_with(&row, async {
        permit.notified().await;
        Ok(futures::stream::SelectAll::new())
    }));
    assert!(opening.as_mut().now_or_never().is_none());
    let (_, mut sibling_changes) = table.register_routing(&sibling).await.expect("live router");
    permit.notify_one();
    let (_, mut changes) = opening.await.expect("join winning router");
    home.admit_reserved(&busy, convoy_spec(), ledger_owed()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        changes.changed().await.expect("first row receives event");
        sibling_changes.changed().await.expect("sibling receives event");
    })
    .await
    .expect("same router routes to both rows");
    assert_eq!(table.inner.routing.lock().await.resyncs, 0, "obsolete empty candidate never starts");
    table.remove_routing(row.id).await;
    assert!(changes.has_changed().is_err(), "removed row's senders are released");
    assert!(sibling_changes.has_changed().is_ok(), "sibling keeps the router alive");
    table.remove_routing(row.id).await;
    assert_eq!(table.inner.routing.lock().await.removal_visits, 1, "repeat removal does no index work");
    table.remove_routing(sibling.id).await;
    assert!(sibling_changes.has_changed().is_err());
    assert_eq!(table.inner.routing.lock().await.removal_visits, 2, "cleanup visits only each row's dependencies");
    assert!(table.inner.routing.lock().await.dependencies.is_empty());
}

// Transient addressed-read errors retain the row, demand and firing identity;
// retries back off without new object events and use fresh evidence on success.
#[tokio::test(start_paused = true)]
async fn leaf_targeted_reads_retry_without_losing_wait_identity() {
    use futures::FutureExt;
    let (backend, home) = engine_backend();
    let busy = launch_at(&backend, &home, "busy", ledger_owed(), ConvoyPhase::Failed).await;
    let table = supervision_wake(&backend, &home).subscriptions;
    let row = overload_row(uuid::Uuid::new_v4(), &busy);
    let id = row.id;
    overload_demand(&table, id).await;
    table.inner.rows.lock().await.insert(id, row.clone());
    let (dependencies, changes) = table.register_routing(&row).await.expect("register");
    let attempts = AtomicUsize::new(0);
    // Store-I/O seam: inject two transient read failures, then use the real store.
    let names = dependencies.names.clone();
    let mut watching = Box::pin(table.watch_registered_row(row, &names, changes, || {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
        let table = &table;
        let dependencies = &dependencies;
        async move {
            match attempt {
                0 => Err(ResourceError::other("temporary store read failure")),
                1 => Err(ResourceError::conflict("busy", "temporary replica view conflict")),
                _ => table.load_subjects("flotilla", &dependencies.objects).await,
            }
        }
    }));
    assert!(watching.as_mut().now_or_never().is_none());
    for _ in 0..20 {
        tokio::task::yield_now().await;
        assert!(watching.as_mut().now_or_never().is_none());
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "one immediate retry, then backoff");
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), 0, "failed reads do not evaluate partial evidence");
    assert_eq!(table.rows().await[0].id, id);
    assert_eq!(table.inner.change_requests.active_demands().await, 1);
    tokio::time::advance(Duration::from_millis(24)).await;
    assert!(watching.as_mut().now_or_never().is_none());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_millis(1)).await;
    watching.await.expect("fresh evidence fires original wait");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), 1);
    assert!(table.rows().await.is_empty());
    assert_eq!(table.inner.change_requests.active_demands().await, 0);
}

// Disconnect aborts a row sleeping after transient read errors and releases
// demand immediately; no read retries occur after cancellation.
#[tokio::test(start_paused = true)]
async fn leaf_read_backoff_is_cancelled_by_disconnect() {
    let (backend, home) = engine_backend();
    let table = supervision_wake(&backend, &home).subscriptions;
    let connection = uuid::Uuid::new_v4();
    let row = overload_row(connection, "busy");
    let id = row.id;
    overload_demand(&table, id).await;
    table.inner.rows.lock().await.insert(id, row.clone());
    let attempts = Arc::new(AtomicUsize::new(0));
    let watching_table = table.clone();
    let reading_attempts = attempts.clone();
    let task = tokio::spawn(async move {
        let (dependencies, changes) = watching_table.register_routing(&row).await.expect("register");
        // Store-I/O seam: hold the store in a transient failure state.
        watching_table
            .watch_registered_row(row, &dependencies.names, changes, || {
                reading_attempts.fetch_add(1, Ordering::SeqCst);
                async { Err(ResourceError::other("store temporarily unavailable")) }
            })
            .await
            .expect("watch");
    });
    let abort = task.abort_handle();
    table.inner.tasks.lock().await.insert(id, task);
    // Registration opens the engine's watches on blocking threads before the first read.
    for _ in 0..100_000 {
        if attempts.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "read retry is sleeping in backoff");
    table.unsubscribe_connection(connection).await;
    tokio::task::yield_now().await;
    assert!(abort.is_finished());
    assert!(table.rows().await.is_empty());
    assert_eq!(table.inner.change_requests.active_demands().await, 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}
