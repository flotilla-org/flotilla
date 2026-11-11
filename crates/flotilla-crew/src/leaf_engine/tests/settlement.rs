use super::*;

/// A spec whose one repository works on `branch`, with change request `bound` bound when given.
fn repository_spec(branch: &str, bound: Option<(&str, &str)>) -> ConvoySpec {
    let repo_ref = RepositoryKey("repo".to_string());
    let mut spec = ConvoySpec::builder()
        .workflow_ref("workflow".to_string())
        .repositories(vec![ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".to_string())
            .repo_ref(repo_ref.clone())
            .source_ref(branch.to_string())
            .target_ref("main".to_string())
            .workspace_slug("flotilla".to_string())
            .subpaths(Vec::new())
            .build()])
        .build();
    spec.change_request =
        bound.map(|(id, title)| BoundChangeRequest::builder().id(id.to_string()).repository_ref(repo_ref).title(title.to_string()).build());
    spec
}

/// Admits `name` with `spec`, its coder owing a decision ledger under `exit`, and keeps the ledger:
/// the launch is landing. Returns the launch.
async fn landing_launch(backend: &ResourceBackend, home: &EngineHome, name: &str, spec: ConvoySpec, exit: ExitDeclaration) -> String {
    let mut status = ledger_owed();
    status.workflow_snapshot.as_mut().expect("workflow").exit = Some(exit);
    let launch = home.admit(name, spec, status).await;
    home.vessel(&launch, "work", "env");
    home.keep(backend, &launch, "work", "coder").await;
    assert_eq!(home.view(backend, &launch).await.status.expect("status").phase, ConvoyPhase::Landing);
    launch
}

/// The convoy reconciler over `home`'s engine, woken by `table`'s rows, reading change requests
/// stale after `stale_after`.
fn convoy_controller(
    backend: &ResourceBackend,
    home: &EngineHome,
    table: &LeafSubscriptionTable,
    stale_after: Duration,
) -> tokio::task::JoinHandle<impl Send + 'static> {
    tokio::spawn(
        ControllerLoop {
            primary: backend.using::<Convoy>("flotilla"),
            secondaries: vec![table.reconciler_wake_watch()],
            reconciler: ConvoyReconciler::new()
                .with_lifecycle_reads(home.reads())
                .with_engine(home.path())
                .with_change_requests(backend.including_replicas::<ChangeRequest>("flotilla"), stale_after),
            backend: backend.clone(),
        }
        .run(),
    )
}

/// Launch `launch`'s status once it has landed.
async fn landed(backend: &ResourceBackend, launch: &str, expect: &str) -> ConvoyStatus {
    let convoys = backend.using::<Convoy>("flotilla");
    let landed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = convoys.get(launch).await.expect("convoy").status.expect("status");
            if status.phase == ConvoyPhase::Landed {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    match landed {
        Ok(status) => status,
        Err(_) => {
            let status = convoys.get(launch).await.expect("convoy").status.expect("status");
            let requests = backend
                .using::<ChangeRequest>("flotilla")
                .list()
                .await
                .expect("change requests")
                .items
                .into_iter()
                .map(|record| (record.metadata.name, record.status.map(|status| status.state)))
                .collect::<Vec<_>>();
            panic!("{expect}: {:?} promises {:?} change requests {requests:?}", status.phase, status.promises)
        }
    }
}

#[tokio::test]
async fn reconciler_wake_rederives_at_boot_and_lands_without_resync() {
    let (backend, home) = engine_backend();
    let (event_tx, _) = broadcast::channel(16);
    let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_secs(3600),
        checks_pending: Duration::from_millis(20),
        freshness_demanded: Duration::from_millis(20),
        stale_after: Duration::from_secs(60),
    };
    let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source.clone(), cadence);
    let table = engine_table(backend.clone(), &home, broadcast_test_sink(event_tx), refresher);
    let spec = repository_spec("feature/reconciler-wake", Some(("1364", "wake")));
    // The launch is landing before the watcher boots.
    let launch = landing_launch(&backend, &home, "wake", spec, ExitDeclaration::standard_table()).await;

    let controller = convoy_controller(&backend, &home, &table, cadence.stale_after);

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
    let convoys = backend.using::<Convoy>("flotilla");
    assert_eq!(convoys.get(&launch).await.expect("open convoy").status.expect("status").phase, ConvoyPhase::Landing);
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
    landed(&backend, &launch, "leaf fire must enqueue reconcile without waiting for hourly resync").await;
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
    let (backend, home) = engine_backend();
    let (event_tx, _) = broadcast::channel(16);
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default();
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".to_string(),
        Arc::new(UnavailableChangeRequests),
        cadence,
    );
    let table = engine_table(backend.clone(), &home, broadcast_test_sink(event_tx), refresher);
    let launch = landing_launch(&backend, &home, "no-cr", convoy_spec(), ExitDeclaration::standard_table()).await;
    let controller = convoy_controller(&backend, &home, &table, cadence.stale_after);

    let status = landed(&backend, &launch, "zero-subject convoy should claim-exit").await;
    assert_eq!(status.disposition.as_deref(), Some("claim"));
    // The wake follows the landing in its own task: once it has, no leaf stays armed.
    tokio::time::timeout(Duration::from_secs(2), async {
        while !table.rows().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("claim exit should not arm leaves");
    controller.abort();
}

// A launch's spec is fixed at admission, so a change request is bound late by an operator
// linking it once the crew's promises are kept.
#[tokio::test]
async fn change_request_bound_after_claims_instantiates_and_settles() {
    let (backend, home) = engine_backend();
    let (event_tx, _) = broadcast::channel(16);
    let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_millis(20),
        checks_pending: Duration::from_millis(20),
        freshness_demanded: Duration::from_millis(20),
        stale_after: Duration::from_secs(60),
    };
    // This daemon, a host with a heartbeat, observes the change requests no other declares an
    // observer for.
    use flotilla_resources::{Host, HostSpec, HostStatus};
    let hosts = backend.using::<Host>("flotilla");
    let host = hosts.create(&InputMeta::builder().name("this-host".into()).build(), &HostSpec::default()).await.expect("host");
    let ready = HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() };
    hosts.update_status("this-host", &host.metadata.resource_version, &ready).await.expect("host heartbeat");
    let authority = backend.local_root().expect("local root").to_string();
    let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), authority, source.clone(), cadence);
    let table = engine_table(backend.clone(), &home, broadcast_test_sink(event_tx), refresher);
    let launch =
        landing_launch(&backend, &home, "adopt-late", repository_spec("feature/adopt-late", None), ExitDeclaration::standard_table()).await;
    let adopted = flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
        id: "1391".into(),
    };
    let convoy = home.view(&backend, &launch).await;
    home.engine().link(&convoy, &adopted, SubjectDiscoverySource::Operator, "operator").await.expect("bind CR after claims");

    // The link reopens the launch with the PR its crew now owes: the launch's subjects, not
    // landing settlement, demand that PR's observation.
    let demands = tokio::spawn({
        let table = table.clone();
        async move { table.watch_change_request_convoy_demands("flotilla").await }
    });
    let controller = convoy_controller(&backend, &home, &table, cadence.stale_after);
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
    let status = landed(&backend, &launch, "late-bound CR should settle").await;
    assert_eq!(status.disposition.as_deref(), Some("merged"));
    controller.abort();
    demands.abort();
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

// #3651 operator recovery: a retracted submission does not restore settlement
// demand after explicit unlink, including after restart. Another launch's live
// subject still demands the very same PR.
#[hegel::test]
fn retracted_unlinked_submission_does_not_recreate_observation(tc: hegel::TestCase) {
    use flotilla_resources::convoy::engine::Actor;
    use flotilla_resources::promises::{PromiseKind, PromiseOperation, PromiseSource, PromiseState, Submission};
    use flotilla_resources::{ChangeRequestSpec, ChangeRequestSubjectHistory};
    let other_holder = tc.draw(hegel::generators::booleans());
    let reconciliations = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("runtime");
    runtime.block_on(async {
        let (backend, home) = engine_backend();
        let convoys = backend.using::<Convoy>("flotilla");
        let address = flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/rigging", 70, &[]).unwrap();
        let subject = flotilla_resources::address::subject_for(&address).unwrap();
        let spec = ConvoySpec::builder().workflow_ref("single-agent".into()).role("coder".into()).build();
        // The coder owes only a decision ledger: its PR promise is the one it submits.
        let launch = home.admit("mistaken-submission", spec, ledger_owed()).await;
        let vessel = home.vessel(&launch, "work", "env");
        home.seat(&backend, &launch, (&vessel, "work"), ("coder", "mistaken-coder"), CrewWorkPhase::Working).await;
        // The coder submits the wrong PR; an operator retracts it, keeping its audit history, and
        // unlinks the subject.
        let convoy = home.view(&backend, &launch).await;
        let submission = PromiseOperation::Submit {
            id: "mistaken-pr".into(),
            kind: PromiseKind::Pr,
            source: PromiseSource::Crew,
            submission: Submission {
                reference: "cr/github.com/flotilla-org/rigging/70".into(),
                metadata: BTreeMap::new(),
                submitted_at: Utc::now(),
                verdict: None,
            },
        };
        home.engine().promise(&convoy, "work", "coder", &submission, Actor::Crew).await.expect("the mistaken submission");
        let convoy = home.view(&backend, &launch).await;
        home.engine().verdict(&convoy, "mistaken-pr", false, "operator", "incorrect PR number").await.expect("retract the submission");
        let convoy = home.view(&backend, &launch).await;
        home.engine().link(&convoy, &subject, SubjectDiscoverySource::Claim, "crew").await.expect("the claimed subject");
        let convoy = home.view(&backend, &launch).await;
        home.engine().unlink(&convoy, &subject, "operator").await.expect("unlink the subject");
        let convoy = home.view(&backend, &launch).await;
        let status = convoy.status.as_ref().unwrap();
        let retracted =
            flotilla_resources::promises::owned(status, "work", "coder").iter().find(|promise| promise.id == "mistaken-pr").unwrap();
        assert_eq!(retracted.state, PromiseState::Retracted);
        assert_eq!(retracted.submissions.len(), 1, "the retraction keeps the mistaken reference");
        assert!(flotilla_resources::expected_change_request_leaves(&convoy, &[]).unwrap().is_empty());
        assert!(!flotilla_resources::promises::needs_observation(status, PromiseKind::Pr));
        assert!(!flotilla_resources::promises::pending(status, "work", "coder").contains(&"mistaken-pr".to_string()));

        let mut history = vec![ChangeRequestSubjectHistory::builder()
            .namespace("flotilla".into())
            .convoy(launch.clone())
            .origin("fleet".into())
            .relationship(flotilla_protocol::Relationship::Produces)
            .role("coder".into())
            .last_seen(Utc::now())
            .build()];
        if other_holder {
            let other = landing_other_holder(&backend, &home).await;
            let convoy = home.view(&backend, &other).await;
            home.engine().link(&convoy, &subject, SubjectDiscoverySource::Operator, "operator").await.expect("the other launch's subject");
            let mut entry = history[0].clone();
            entry.convoy = other;
            history.push(entry);
        }
        let name = cr_name("github.com", "flotilla-org/rigging", 70);
        let requests = backend.using::<ChangeRequest>("flotilla");
        requests
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("flotilla-org/rigging".into())
                    .number(70)
                    .observing_authority("authority".into())
                    .subject_of(history.clone())
                    .build(),
            )
            .await
            .unwrap();
        let wake = supervision_wake(&backend, &home);
        let refresher = &wake.subscriptions.inner.change_requests;
        refresher.garbage_collect_orphans().await.unwrap();
        for _ in 0..reconciliations {
            let snapshot = convoys.list().await.unwrap().items.into_iter().map(|c| (c.metadata.name.clone(), c)).collect::<HashMap<_, _>>();
            wake.sync_rows("flotilla", &snapshot).await.unwrap();
            for row in wake.subscriptions.rows().await {
                if matches!(&row.watcher, LeafWatcher::ReconcilerWake { convoy } if *convoy == launch) {
                    assert!(!row.leaves.iter().any(|leaf| matches!(leaf.address, LeafAddress::ChangeRequest { number: 70, .. })));
                }
            }
            assert_eq!(refresher.active_demands().await > 0, other_holder);
            tokio::time::advance(Duration::from_secs(16 * 60)).await;
            assert_eq!(requests.get(&name).await.is_ok(), other_holder, "unheld record must stay absent after polling");
            if other_holder {
                // The other launch's live demand records its own history beside the one written
                // here; only the unlinked launch's is released.
                let record = requests.get(&name).await.unwrap();
                assert!(!record.spec.subject_of.is_empty());
                assert!(record.spec.subject_of.iter().all(|entry| entry.convoy == history[1].convoy), "{:?}", record.spec.subject_of);
            }
        }
        for row in wake.subscriptions.rows().await {
            wake.subscriptions.finish(row.id).await;
        }
    });
}

/// Another launch, landing, that will hold the same PR.
async fn landing_other_holder(backend: &ResourceBackend, home: &EngineHome) -> String {
    landing_launch(backend, home, "other-holder", convoy_spec(), ExitDeclaration::standard_table()).await
}
