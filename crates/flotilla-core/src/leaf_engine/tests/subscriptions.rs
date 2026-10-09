use super::*;

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
    use futures::FutureExt;
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let table = supervision_wake(&backend).subscriptions;
    create_convoy(&backend, "busy", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    let id = uuid::Uuid::new_v4();
    let row = LeafSubscriptionRow {
        id,
        namespace: "flotilla".into(),
        leaves: vec![leaf(LeafAddress::Convoy { name: "busy".into() }, ".status.phase", "Failed")],
        watcher: LeafWatcher::WaitCaller { connection_id: uuid::Uuid::new_v4() },
        maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
        freshness_demand: None,
        created_at: Utc::now(),
        episode_key: EpisodeKeyFields::default(),
    };
    table.inner.rows.lock().await.insert(id, row.clone());
    let mut watching = Box::pin(table.watch_row(row));
    // Poll through watch registration and snapshot loading, then deliberately
    // stop polling while writes outrun the bounded ring.
    assert!(watching.as_mut().now_or_never().is_none());
    let convoys = backend.using::<Convoy>("flotilla");
    let mut object = convoys.get("busy").await.expect("convoy");
    for index in 0..600 {
        let spec = ConvoySpec::builder().workflow_ref(format!("workflow-{index}")).build();
        object = convoys.update(&InputMeta::from(&object.metadata), &object.metadata.resource_version, &spec).await.expect("update");
    }
    convoys
        .update_status("busy", &object.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() })
        .await
        .expect("fail convoy");
    tokio::time::timeout(Duration::from_secs(2), watching).await.expect("wait recovers").expect("watch succeeds");
    assert!(!table.rows().await.iter().any(|row| row.id == id), "recovered wait fires and releases its row");
    assert!(table.inner.last_firings.lock().await.is_empty(), "one-shot wait releases firing state");
}

// Permanent overload must bound addressed snapshot work, retain identity
// and accounting during sleeps, then recover from the latest state.
#[tokio::test(start_paused = true)]
async fn leaf_sustained_overload_bounds_snapshot_work_and_recovers() {
    use futures::FutureExt;
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let table = supervision_wake(&backend).subscriptions;
    create_convoy(&backend, "busy", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    let row = overload_row(uuid::Uuid::new_v4());
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
    // 30,000 writes in two simulated seconds, with the leaf allowed
    // to run between bursts. Count addressed snapshots, not RSS.
    for _ in 0..100 {
        overload_convoy(&backend).await;
        for _ in 0..2 {
            assert!(watching.as_mut().now_or_never().is_none());
        }
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    let snapshots = table.inner.snapshot_loads.load(Ordering::SeqCst);
    eprintln!("overload: 30000 writes/2s, {snapshots} addressed snapshots");
    let resyncs = table.inner.routing.lock().await.resyncs;
    assert!(resyncs <= 10, "repeated expiry must throttle shared resyncs: {resyncs}");
    assert_eq!(table.inner.store_reads.load(Ordering::SeqCst), snapshots, "recovery reads only the addressed convoy");
    assert!(table.rows().await.iter().any(|row| row.id == id));
    assert_eq!(table.inner.change_requests.active_demands().await, 1, "backoff retains demand");
    assert_eq!(table.inner.last_firings.lock().await.len(), 1, "backoff retains firing history");
    assert!(table.inner.unable_since.lock().await.contains_key(&id), "backoff retains episode accounting");
    assert!(table.inner.stale_attention_reported.lock().await.contains(&id));
    let convoys = backend.using::<Convoy>("flotilla");
    let object = convoys.get("busy").await.expect("convoy");
    convoys
        .update_status("busy", &object.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() })
        .await
        .expect("fail convoy");
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
    use futures::FutureExt;
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let table = supervision_wake(&backend).subscriptions;
    create_convoy(&backend, "busy", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    let connection_id = uuid::Uuid::new_v4();
    let row = overload_row(connection_id);
    let id = row.id;
    overload_demand(&table, id).await;
    table.inner.rows.lock().await.insert(id, row.clone());
    let watching_table = table.clone();
    let mut watching = Box::pin(async move { watching_table.watch_row(row).await });
    assert!(watching.as_mut().now_or_never().is_none());
    overload_convoy(&backend).await;
    assert!(watching.as_mut().now_or_never().is_none());
    assert!(watching.as_mut().now_or_never().is_none());
    tokio::task::yield_now().await;
    assert!(watching.as_mut().now_or_never().is_none());
    let snapshots = table.inner.snapshot_loads.load(Ordering::SeqCst);
    overload_convoy(&backend).await;
    assert!(watching.as_mut().now_or_never().is_none());
    tokio::task::yield_now().await;
    assert!(watching.as_mut().now_or_never().is_none());
    let snapshots = snapshots.max(table.inner.snapshot_loads.load(Ordering::SeqCst));
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
    let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let convoys = backend.using::<Convoy>("flotilla");
    flotilla_resources::apply_status_patch(
        &convoys,
        "stalled-work",
        &flotilla_resources::external_patches::resume_crew_work(
            "work".into(),
            "coder".into(),
            Utc::now() - chrono::Duration::minutes(3),
            "Continue with the operator's guidance".into(),
            Some("lost-resume-brief".into()),
        ),
    )
    .await
    .expect("resume declared stall");
    let row_id = *wake.subscriptions.inner.rows.lock().await.keys().next().expect("actor row");
    wake.subscriptions
        .inner
        .unable_since
        .lock()
        .await
        .insert(row_id, (UnableEvidenceKey::Absent, Utc::now() - chrono::Duration::minutes(3)));
    let source = convoys.get("stalled-work").await.expect("resumed convoy");
    let governor = convoys.get("governor").await.expect("governor");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source), ("governor".into(), governor)]))
        .await
        .expect("judge missing session");
    let status = convoys.get("stalled-work").await.expect("source").status.expect("status");
    let requests = delivery.requests.lock().expect("supervisor requests");
    assert!(!requests.is_empty(), "missing session should be supervised: {:?}", status.stalled);
    assert_eq!(requests[0].convoy, "governor");
    // Escalations retain the fully-qualified source crew, including its convoy.
    assert_eq!(requests[0].sender, "wheelhouse/stalled-work/work/coder");
    assert_eq!(requests[0].expectation, flotilla_resources::MessageExpectation::Reply);
}

// Deliberate operator-only or empty policies consume the ladder. Routine
// reconciles must not rewrite the condition or repeatedly retry that decision.
#[tokio::test]
async fn operator_only_supervision_is_consumed_once() {
    for policy in [Vec::new(), vec![SupervisionTarget::Operator]] {
        let (backend, wake, delivery) = project_supervision_case(&[]).await;
        let convoys = backend.using::<Convoy>("flotilla");
        let source = convoys.get("stalled-work").await.expect("source");
        let mut status = source.status.expect("status");
        status.workflow_snapshot.as_mut().expect("snapshot").supervision = Some(policy);
        convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("operator policy");
        let mut previous_version = None;
        for _ in 0..3 {
            let objects =
                convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
            wake.sync_rows("flotilla", &objects).await.expect("rows");
            wake.judge_stalls("flotilla", &objects).await.expect("judge operator policy");
            let source = convoys.get("stalled-work").await.expect("source");
            let stalled = source.status.expect("status").stalled.expect("stall");
            assert!(stalled.supervision_exhausted);
            assert!(stalled.supervision_index.is_some());
            assert!(stalled.supervisor.is_none());
            assert_eq!(stalled.rung, StallRung::Operator);
            if let Some(version) = previous_version {
                assert_eq!(source.metadata.resource_version, version);
            }
            previous_version = Some(source.metadata.resource_version);
            assert!(delivery.requests.lock().expect("deliveries").is_empty());
        }
    }
}

#[tokio::test]
async fn in_memory_leaf_subscription_contract() {
    assert_leaf_subscription_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn sqlite_leaf_subscription_contract() {
    assert_leaf_subscription_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("open sqlite"))).await;
}

#[tokio::test]
async fn diagnostics_retain_the_last_firing_for_an_armed_reconciler_row() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(4);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".to_string(),
        Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) }),
        crate::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = LeafSubscriptionTable::new(backend, broadcast_test_sink(event_tx), refresher);
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
    let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_millis(20),
        checks_pending: Duration::from_millis(20),
        freshness_demanded: Duration::from_millis(20),
        stale_after: Duration::from_secs(60),
    };
    let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source.clone(), cadence);
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
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
    let mut meta = InputMeta::builder().name("custom".to_string()).finalizers(vec!["flotilla.work/convoy-teardown".to_string()]).build();
    meta.set_lifecycle_authority(LifecycleAuthority::Managed);
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let created = convoys.create(&meta, &spec).await.expect("create custom-exit convoy");
    let status = ConvoyStatus {
        phase: ConvoyPhase::Landing,
        observed_workflow_ref: Some("workflow".to_string()),
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            stall_nudges: Default::default(),
            supervision: None,
            exit: Some(ExitDeclaration::Table(indexmap::IndexMap::from([(
                "shipped".to_string(),
                "$cr.state == merged".parse().expect("custom leaf template"),
            )]))),
            turn_delivery: Default::default(),
            vessels: Vec::new(),
        }),
        work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
        ..Default::default()
    };
    convoys.update_status("custom", &created.metadata.resource_version, &status).await.expect("mark custom convoy Landing");

    let controller = tokio::spawn(
        ControllerLoop {
            primary: convoys.clone(),
            secondaries: vec![table.reconciler_wake_watch()],
            reconciler: ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla"))
                .with_change_requests(backend.including_replicas::<ChangeRequest>("flotilla"), cadence.stale_after),
            resync_interval: Duration::from_secs(3600),
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
            let convoy = convoys.get("custom").await.expect("convoy");
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
#[hegel::test]
fn leaf_object_routing_counts_only_addressed_rows(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let changes: Vec<usize> = (0..steps).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(63))).collect();
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        routing_count_contract(ResourceBackend::InMemory(InMemoryBackend::default()), &changes).await;
    });
}

// Run the same counted routing contract against the real SQLite adapter.
#[tokio::test]
async fn sqlite_leaf_object_routing_counts_only_addressed_rows() {
    routing_count_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite")), &[63, 0, 1, 1, 32]).await;
}

async fn routing_count_contract(backend: ResourceBackend, changes: &[usize]) {
    use futures::FutureExt;
    for index in 0..64 {
        create_convoy(&backend, &format!("object-{index}"), ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    }
    let (events, mut fires) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".into(),
        backend.clone(),
        "authority".into(),
        Arc::new(UnavailableChangeRequests),
        Default::default(),
    );
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(events), refresher);
    let mut watchers = Vec::new();
    let mut ids = Vec::new();
    for index in [0, 1, 2, 0] {
        let mut row = overload_row(uuid::Uuid::new_v4());
        row.leaves = vec![leaf(LeafAddress::Convoy { name: format!("object-{index}") }, ".status.phase", "Failed")];
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
        let previous_events = table.inner.routing.lock().await.events_processed;
        let before_evaluations = table.inner.evaluations.load(Ordering::SeqCst);
        let before_reads = table.inner.store_reads.load(Ordering::SeqCst);
        let convoys = backend.using::<Convoy>("flotilla");
        let name = format!("object-{index}");
        let object = convoys.get(&name).await.expect("convoy");
        convoys
            .update(
                &InputMeta::from(&object.metadata),
                &object.metadata.resource_version,
                &ConvoySpec::builder().workflow_ref(format!("changed-{step}")).build(),
            )
            .await
            .expect("event");
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
    }
    // A matching name in another namespace is not an addressed object.
    let other = backend.using::<Convoy>("other");
    other
        .create(&InputMeta::builder().name("object-0".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .expect("other namespace");
    tokio::task::yield_now().await;
    let before = table.inner.evaluations.load(Ordering::SeqCst);
    for watching in &mut watchers {
        assert!(watching.as_mut().now_or_never().is_none());
    }
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), before);
    // Exactly one wait fires per addressed object, retaining subscription identity.
    for (position, watching) in watchers.iter_mut().enumerate() {
        let index = [0, 1, 2, 0][position];
        let convoys = backend.using::<Convoy>("flotilla");
        let name = format!("object-{index}");
        let object = convoys.get(&name).await.expect("convoy");
        convoys
            .update_status(&name, &object.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() })
            .await
            .expect("fail");
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

// Absent records remain Unknown until creation; Work leaves route via their
// parent convoy, and re-arming after restart evaluates current evidence once.
#[tokio::test]
async fn leaf_routing_missing_work_and_restart() {
    use futures::FutureExt;
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let table = supervision_wake(&backend).subscriptions;
    let mut row = overload_row(uuid::Uuid::new_v4());
    row.leaves = vec![leaf(LeafAddress::Work { convoy: "missing".into(), work: "work".into() }, ".status.phase", "Complete")];
    table.inner.rows.lock().await.insert(row.id, row.clone());
    let mut watching = Box::pin(table.watch_row(row.clone()));
    assert!(watching.as_mut().now_or_never().is_none(), "absent convoy is Unknown");
    create_convoy(&backend, "missing", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    router_processed(&table, 0).await;
    assert!(watching.as_mut().now_or_never().is_none(), "absent work is Unknown");
    let convoys = backend.using::<Convoy>("flotilla");
    let object = convoys.get("missing").await.expect("convoy");
    convoys
        .update_status(
            "missing",
            &object.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Complete).build())]),
                ..Default::default()
            },
        )
        .await
        .expect("complete work");
    tokio::time::timeout(Duration::from_secs(2), watching).await.expect("parent convoy event arrives").expect("Work leaf fires");
    assert!(table.rows().await.is_empty());
    let restarted = supervision_wake(&backend).subscriptions;
    row.id = uuid::Uuid::new_v4();
    restarted.inner.rows.lock().await.insert(row.id, row.clone());
    restarted.watch_row(row).await.expect("restart fires from current level");
    assert_eq!(restarted.inner.evaluations.load(Ordering::SeqCst), 1);
    assert_eq!(restarted.inner.store_reads.load(Ordering::SeqCst), 1);
    assert!(restarted.rows().await.is_empty());
}

// Watch loss must resync every live row exactly once, even when the overflow
// comes from an unrelated object. Subsequent unrelated events stay unrouted.
#[tokio::test]
async fn leaf_resync_evaluates_each_row_once_then_routes_again() {
    use futures::FutureExt;
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    for name in ["busy", "object-0", "object-1"] {
        create_convoy(&backend, name, ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    }
    let table = supervision_wake(&backend).subscriptions;
    let mut watchers = Vec::new();
    let mut connections = Vec::new();
    for index in 0..2 {
        let connection = uuid::Uuid::new_v4();
        connections.push(connection);
        let mut row = overload_row(connection);
        row.leaves = vec![leaf(LeafAddress::Convoy { name: format!("object-{index}") }, ".status.phase", "Failed")];
        table.inner.rows.lock().await.insert(row.id, row.clone());
        let mut watching = Box::pin(table.watch_row(row));
        assert!(watching.as_mut().now_or_never().is_none());
        watchers.push(watching);
    }
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), 2);
    // No task can consume the store's bounded watch ring during this burst.
    overload_convoy(&backend).await;
    overload_convoy(&backend).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while table.inner.routing.lock().await.resyncs == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shared router recovers");
    for watching in &mut watchers {
        assert!(watching.as_mut().now_or_never().is_none());
    }
    assert_eq!(table.inner.evaluations.load(Ordering::SeqCst), 4, "one resync evaluation per row");
    assert_eq!(table.inner.store_reads.load(Ordering::SeqCst), 4, "one addressed read per evaluation");
    let previous = table.inner.routing.lock().await.events_processed;
    let convoys = backend.using::<Convoy>("flotilla");
    let object = convoys.get("busy").await.expect("busy");
    convoys
        .update(
            &InputMeta::from(&object.metadata),
            &object.metadata.resource_version,
            &ConvoySpec::builder().workflow_ref("after-resync".into()).build(),
        )
        .await
        .expect("unrelated event");
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
