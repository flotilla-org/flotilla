use super::*;

#[tokio::test]
async fn reconciler_wake_rederives_at_boot_and_lands_without_resync() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
    let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_secs(3600),
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
            .source_ref("feature/reconciler-wake".to_string())
            .target_ref("main".to_string())
            .workspace_slug("flotilla".to_string())
            .subpaths(Vec::new())
            .build()])
        .change_request(BoundChangeRequest::builder().id("1364".to_string()).repository_ref(repo_ref).title("wake".to_string()).build())
        .build();
    let mut meta = InputMeta::builder().name("wake".to_string()).finalizers(vec!["flotilla.work/convoy-teardown".to_string()]).build();
    meta.set_lifecycle_authority(LifecycleAuthority::Managed);
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let created = convoys.create(&meta, &spec).await.expect("create Landing convoy before watcher boot");
    let status = ConvoyStatus {
        phase: ConvoyPhase::Landing,
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            stall_nudges: Default::default(),
            supervision: None,
            exit: Some(ExitDeclaration::standard_table()),
            turn_delivery: Default::default(),
            vessels: Vec::new(),
        }),
        observed_workflow_ref: Some("workflow".to_string()),
        work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
        ..Default::default()
    };
    convoys.update_status("wake", &created.metadata.resource_version, &status).await.expect("mark Landing");

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
            if table.rows().await.iter().any(|row| matches!(row.watcher, LeafWatcher::ReconcilerWake { .. })) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("boot must rederive a ReconcilerWake row");
    assert!(
        table.rows().await.iter().any(|row| row.freshness_demand.is_some()),
        "Landing settlement must demand fresh targeted observations"
    );
    assert_eq!(convoys.get("wake").await.expect("open convoy").status.expect("status").phase, ConvoyPhase::Landing);
    let change_requests = backend.using::<ChangeRequest>("flotilla");
    let record_name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1364);
    let change_request = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(record) = change_requests.get(&record_name).await {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("home authority must publish the demanded change request record");
    assert_eq!(change_request.spec.observing_authority, "authority");

    source.merged.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if convoys.get("wake").await.expect("convoy").status.expect("status").phase == ConvoyPhase::Landed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("leaf fire must enqueue reconcile without waiting for hourly resync");
    controller.abort();
}

#[tokio::test]
async fn merged_unclaimed_crew_delivers_once() {
    for unknown_head in [false, true] {
        merged_unclaimed_scenario(2, unknown_head).await;
    }
}

// Repeated observations and subscription reconstruction must retain exactly
// one terminal delivery, whether the PR head is known or unknown.
#[hegel::test]
fn generated_merged_unclaimed_delivery_is_idempotent(tc: hegel::TestCase) {
    let repeats = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(5));
    let unknown_head = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(merged_unclaimed_scenario(repeats, unknown_head));
}

#[tokio::test]
async fn active_produced_pr_checks_settle_to_pass_or_fail_once_per_head() {
    for role in ["coder", "reviewer"] {
        for pass in [false, true] {
            active_checks_scenario(&[pass, !pass, pass, !pass], true, role).await;
        }
    }
}

#[hegel::test]
fn generated_active_checks_settlement_deduplicates_heads(tc: hegel::TestCase) {
    // Generate pass/fail sequences across the delivery ceiling (3), including
    // coder/reviewer roles, new heads, unknown and pending observations, and duplicate settlement.
    let role = if tc.draw(hegel::generators::booleans()) { "reviewer" } else { "coder" };
    let count = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(12));
    let outcomes = (0..count).map(|_| tc.draw(hegel::generators::booleans())).collect::<Vec<_>>();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(active_checks_scenario(&outcomes, false, role));
}

#[tokio::test]
async fn zero_subject_landing_settles_as_claim_exit_through_engine() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let cadence = crate::change_request_observer::ChangeRequestRefreshCadence::default();
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".to_string(),
        Arc::new(UnavailableChangeRequests),
        cadence,
    );
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("no-cr".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("create zero-subject convoy");
    convoys
        .update_status(
            "no-cr",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(ExitDeclaration::standard_table()),
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                observed_workflow_ref: Some("workflow".to_string()),
                work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
                ..Default::default()
            },
        )
        .await
        .expect("mark zero-subject convoy Landing");
    let controller = tokio::spawn(
        ControllerLoop {
            primary: convoys.clone(),
            secondaries: vec![table.reconciler_wake_watch()],
            reconciler: ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla")),
            resync_interval: Duration::from_secs(3600),
            backend,
        }
        .run(),
    );

    let status = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = convoys.get("no-cr").await.expect("convoy").status.expect("status");
            if status.phase == ConvoyPhase::Landed {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("zero-subject convoy should claim-exit");
    assert_eq!(status.disposition.as_deref(), Some("claim"));
    assert!(table.rows().await.is_empty(), "claim exit should not arm leaves");
    controller.abort();
}

#[tokio::test]
async fn change_request_bound_after_claims_instantiates_and_settles() {
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
    let mut spec = ConvoySpec::builder()
        .workflow_ref("workflow".to_string())
        .repositories(vec![ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".to_string())
            .repo_ref(repo_ref.clone())
            .source_ref("feature/adopt-late".to_string())
            .target_ref("main".to_string())
            .workspace_slug("flotilla".to_string())
            .subpaths(Vec::new())
            .build()])
        .build();
    let meta = InputMeta::builder().name("adopt-late".to_string()).build();
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let created = convoys.create(&meta, &spec).await.expect("create unbound convoy");
    let landing = convoys
        .update_status(
            "adopt-late",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(ExitDeclaration::standard_table()),
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                observed_workflow_ref: Some("workflow".to_string()),
                work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
                ..Default::default()
            },
        )
        .await
        .expect("claims enter Landing before binding");
    spec.change_request =
        Some(BoundChangeRequest::builder().id("1391".to_string()).repository_ref(repo_ref).title("adopted late".to_string()).build());
    convoys.update(&meta, &landing.metadata.resource_version, &spec).await.expect("bind CR after claims");

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
    .expect("late binding should instantiate exit leaves");
    source.merged.store(true, Ordering::SeqCst);
    let status = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = convoys.get("adopt-late").await.expect("convoy").status.expect("status");
            if status.phase == ConvoyPhase::Landed {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late-bound CR should settle");
    assert_eq!(status.disposition.as_deref(), Some("merged"));
    controller.abort();
}
