use super::*;
use flotilla_store_memory::InMemoryBackend;

#[tokio::test]
async fn reconciler_wake_rederives_at_boot_and_lands_without_resync() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
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
    let record_name = cr_name("github.com", "flotilla-org/flotilla", 1364);
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
        merged_unclaimed_scenario(2, unknown_head, false, false).await;
    }
}

// Repeated observations and subscription reconstruction must retain exactly
// one terminal delivery across known/unknown heads, interrupted/stalled crews,
// and open/terminal ledgers.
// Each case drives real in-memory stores, informers and turn delivery. Hegel
// charges that whole scenario to generation time; CPU scheduling is not a
// generation-cost oracle. Keep the bounded generators and other health checks.
#[hegel::test(suppress_health_check = [hegel::HealthCheck::TooSlow])]
fn generated_merged_unclaimed_delivery_is_idempotent(tc: hegel::TestCase) {
    let repeats = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(5));
    let unknown_head = tc.draw(hegel::generators::booleans());
    let stalled = tc.draw(hegel::generators::booleans());
    let open_ledger = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(merged_unclaimed_scenario(
        repeats,
        unknown_head,
        stalled,
        open_ledger,
    ));
}

#[tokio::test]
async fn active_produced_pr_checks_settle_to_pass_or_fail_once_per_head() {
    for role in ["coder", "reviewer"] {
        for pass in [false, true] {
            active_checks_scenario(&[pass, !pass, pass, !pass], true, role).await;
        }
    }
}

// As above, Hegel charges this bounded store scenario's execution to its
// generation budget; retain coverage and the other health checks.
#[hegel::test(suppress_health_check = [hegel::HealthCheck::TooSlow])]
fn generated_active_checks_settlement_deduplicates_heads(tc: hegel::TestCase) {
    // Generate pass/fail sequences past the episode limit (3), including
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
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default();
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
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
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

// #3140: a stalled crew must resume once per merged PR even with another
// promise open; the brief identifies the remaining ledger by its promise ID.
#[tokio::test]
async fn stalled_merged_unclaimed_crew_resumes_with_open_promises() {
    for open_ledger in [false, true] {
        for unknown_head in [false, true] {
            merged_unclaimed_scenario(2, unknown_head, true, open_ledger).await;
        }
    }
}

// #3651 operator recovery: an old retracted submission and refusal do not
// restore settlement demand after explicit unlink, including after restart.
// Another convoy's live subject still demands the very same PR.
#[hegel::test]
fn retracted_unlinked_submission_does_not_recreate_observation(tc: hegel::TestCase) {
    use flotilla_resources::promises::{Promise, PromiseKind, PromiseSource, PromiseState, Submission};
    use flotilla_resources::{ChangeRequestSpec, ChangeRequestSubjectHistory, SubjectDiscoverySource};
    let other_holder = tc.draw(hegel::generators::booleans());
    let reconciliations = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("runtime");
    runtime.block_on(async {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let convoys = backend.using::<Convoy>("flotilla");
        let address = flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/rigging", 70, &[]).unwrap();
        let subject = flotilla_resources::address::subject_for(&address).unwrap();
        let created = convoys.create(&InputMeta::builder().name("mistaken-submission".into()).build(),
            &ConvoySpec::builder().workflow_ref("single-agent".into()).role("coder".into()).build()).await.unwrap();
        let workflow = flotilla_resources::single_agent_workflow_spec();
        let mut status = ConvoyStatus {
            phase: ConvoyPhase::Landing,
            workflow_snapshot: Some(WorkflowSnapshot {
                cascade: None, stall_nudges: workflow.stall_nudges, supervision: workflow.supervision,
                exit: workflow.exit, turn_delivery: workflow.turn_delivery, vessels: workflow.vessels,
            }),
            crew_work: BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(),
                CrewWorkState::builder().phase(CrewWorkPhase::Stalled).build())]))]),
            ..Default::default()
        };
        // Previous-generation data allowed a submitted promise to be retracted.
        // Retain its audit history rather than erasing the mistaken reference.
        status.promises.insert("work".into(), BTreeMap::from([("coder".into(), vec![Promise {
            id: "mistaken-pr".into(), vessel: "work".into(), role: "coder".into(), source: PromiseSource::Crew,
            kind: PromiseKind::Pr, state: PromiseState::Retracted, retraction_reason: Some("incorrect PR number".into()),
            submissions: vec![Submission { reference: "cr/github.com/flotilla-org/rigging/70".into(), metadata: BTreeMap::new(),
                submitted_at: Utc::now(), verdict: None }],
        }]) ]));
        status.discover_subject(subject.clone(), flotilla_protocol::Relationship::Produces, SubjectDiscoverySource::Claim, Utc::now());
        status.unlink_subject(&subject);
        // Decode the old refusal shape, which the current writer no longer emits.
        let mut stored = serde_json::to_value(&status).unwrap();
        stored["crew_work"]["work"]["coder"]["completion_refusal"] = serde_json::json!({
            "expectation": "cr/github.com/flotilla-org/rigging/70", "consecutive_count": 2,
            "causes": [{"kind": "missing_change_request_observation", "service": "github.com", "scope": "flotilla-org/rigging", "number": 70}]
        });
        let restored: ConvoyStatus = serde_json::from_value(stored).unwrap();
        assert!(restored.crew_work["work"]["coder"].completion_refusal.is_some());
        let convoy = convoys.update_status(&created.metadata.name, &created.metadata.resource_version, &restored).await.unwrap();
        assert!(flotilla_resources::expected_change_request_leaves(&convoy, &[]).unwrap().is_empty());
        assert!(!flotilla_resources::promises::needs_observation(&restored, PromiseKind::Pr));
        assert!(flotilla_resources::promises::pending(&restored, "work", "coder").is_empty());

        let mut history = vec![ChangeRequestSubjectHistory::builder().namespace("flotilla".into()).convoy(created.metadata.name.clone())
            .origin("fleet".into()).relationship(flotilla_protocol::Relationship::Produces).role("coder".into()).last_seen(Utc::now()).build()];
        if other_holder {
            let other = convoys.create(&InputMeta::builder().name("other-holder".into()).build(), &created.spec).await.unwrap();
            let mut status = ConvoyStatus { phase: ConvoyPhase::Landing, workflow_snapshot: restored.workflow_snapshot.clone(), ..Default::default() };
            status.discover_subject(subject.clone(), flotilla_protocol::Relationship::Produces, SubjectDiscoverySource::Operator, Utc::now());
            convoys.update_status(&other.metadata.name, &other.metadata.resource_version, &status).await.unwrap();
            let mut entry = history[0].clone(); entry.convoy = other.metadata.name; history.push(entry);
        }
        let name = cr_name("github.com", "flotilla-org/rigging", 70);
        let requests = backend.using::<ChangeRequest>("flotilla");
        requests.create(&InputMeta::builder().name(name.clone()).build(), &ChangeRequestSpec::builder()
            .service("github.com".into()).scope("flotilla-org/rigging".into()).number(70).observing_authority("authority".into())
            .subject_of(history).build()).await.unwrap();
        let wake = supervision_wake(&backend);
        let refresher = &wake.subscriptions.inner.change_requests;
        refresher.garbage_collect_orphans().await.unwrap();
        for _ in 0..reconciliations {
            let mut snapshot = convoys.list().await.unwrap().items.into_iter().map(|c| (c.metadata.name.clone(), c)).collect::<HashMap<_, _>>();
            // Exercise the old decoded refusal: the current writer omits it.
            snapshot.get_mut("mistaken-submission").unwrap().status = Some(restored.clone());
            wake.sync_rows("flotilla", &snapshot).await.unwrap();
            for row in wake.subscriptions.rows().await {
                if matches!(&row.watcher, LeafWatcher::ReconcilerWake { convoy } if convoy == "mistaken-submission") {
                    assert!(!row.leaves.iter().any(|leaf| matches!(leaf.address, LeafAddress::ChangeRequest { number: 70, .. })));
                }
            }
            assert_eq!(refresher.active_demands().await > 0, other_holder);
            tokio::time::advance(Duration::from_secs(16 * 60)).await;
            assert_eq!(requests.get(&name).await.is_ok(), other_holder, "unheld record must stay absent after polling");
            if other_holder {
                let record = requests.get(&name).await.unwrap();
                assert_eq!(record.spec.subject_of.len(), 1);
                assert_eq!(record.spec.subject_of[0].convoy, "other-holder");
            }
        }
        for row in wake.subscriptions.rows().await { wake.subscriptions.finish(row.id).await; }
    });
}
