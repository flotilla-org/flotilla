use super::*;

fn observed_request(state: flotilla_resources::ObservedChangeRequestState, at: DateTime<Utc>) -> flotilla_resources::ChangeRequestStatus {
    flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: flotilla_resources::Observation::known(state, at),
        head_sha: Default::default(),
        checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, at),
        review: flotilla_resources::ChangeRequestReviewObservation {
            actionable_at_head: flotilla_resources::Observation::known(false, at),
        },
        mergeable: Default::default(),
    }
}

async fn sync_snapshot(backend: &ResourceBackend, wake: &ReconcilerWake) -> bool {
    let objects = backend
        .using::<Convoy>("flotilla")
        .list()
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|object| (object.metadata.name.clone(), object))
        .collect();
    wake.sync_rows("flotilla", &objects).await.unwrap();
    wake.subscriptions
        .rows()
        .await
        .iter()
        .any(|row| matches!(&row.maker, LeafMaker::Controller { resource_kind, .. } if resource_kind == "Convoy"))
}

/// Change request `number` of github.com/team/one, as a launch links it.
fn team_one_request(number: u64) -> flotilla_protocol::Subject {
    flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "team/one".into() },
        id: number.to_string(),
    }
}

// #3710: every merged field and empty claim exits remain final. Unchanged
// evidence is evaluated once; a change of the launch invalidates its cached proof.
#[hegel::test]
fn final_settlement_cache_preserves_freshness_and_input_changes(tc: hegel::TestCase) {
    let field = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(4));
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for field in [0, 1, 2, 3, 4, field] {
            let (backend, home) = engine_backend().await;
            let mut wake = supervision_wake(&backend, &home);
            let now = Utc::now();
            let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(now));
            wake.subscriptions = wake.subscriptions.with_clock(clock.clone());
            let repo = RepositoryKey("repo".into());
            let mut spec = convoy_spec();
            if field != 3 {
                spec.repositories = vec![ConvoyRepositorySpec::builder()
                    .url("https://github.com/team/one".into())
                    .repo_ref(repo.clone())
                    .source_ref("branch".into())
                    .target_ref("main".into())
                    .workspace_slug("repo".into())
                    .subpaths(vec![])
                    .build()];
                spec.change_request = Some(BoundChangeRequest::builder().id("1".into()).repository_ref(repo).title("test".into()).build());
            }
            let convoys = backend.using::<Convoy>("flotilla");
            let exit = if field == 3 {
                ExitDeclaration::standard_table()
            } else {
                ExitDeclaration::Table(indexmap::IndexMap::from([(
                    "shipped".into(),
                    flotilla_resources::parse_leaf_template(match field {
                        0 => "$cr.state == merged",
                        1 => "$cr.ready == true",
                        2 => "$cr.checks == pass",
                        _ => "$cr.review.actionable-at-head == false",
                    })
                    .unwrap(),
                )]))
            };
            let cached = landing_launch(&backend, &home, "cached", spec, exit).await;
            if field != 3 {
                let records = backend.using::<ChangeRequest>("flotilla");
                let name = cr_name("github.com", "team/one", 1);
                let created = records
                    .create(
                        &InputMeta::builder().name(name.clone()).build(),
                        &flotilla_resources::ChangeRequestSpec::builder()
                            .service("github.com".into())
                            .scope("team/one".into())
                            .number(1)
                            .observing_authority("test-host".into())
                            .build(),
                    )
                    .await
                    .unwrap();
                records
                    .update_status(
                        &name,
                        &created.metadata.resource_version,
                        &observed_request(flotilla_resources::ObservedChangeRequestState::Merged, now),
                    )
                    .await
                    .unwrap();
            }
            assert!(sync_snapshot(&backend, &wake).await);
            let cell = wake.views("flotilla").await.unwrap();
            let calls = || cell.get().unwrap().settlement_evaluations.load(Ordering::Relaxed);
            assert_eq!(calls(), 1);
            // Arming demand records the convoy link in the CR spec. Observe
            // that legitimate revision change before checking steady state.
            assert!(sync_snapshot(&backend, &wake).await);
            let before = calls();
            assert!(sync_snapshot(&backend, &wake).await);
            assert_eq!(calls(), before);
            clock.advance(chrono::Duration::seconds(181));
            assert!(sync_snapshot(&backend, &wake).await);
            assert_eq!(calls(), before);
            // The launch changes: an operator links another change request and unlinks it. Its
            // exit is fixed at admission, so the settlement stays final, evaluated once again.
            let other = team_one_request(2);
            let current = convoys.get(&cached).await.unwrap();
            home.engine().link(&current, &other, SubjectDiscoverySource::Operator, "operator").await.unwrap();
            let current = convoys.get(&cached).await.unwrap();
            home.engine().unlink(&current, &other, "operator").await.unwrap();
            assert!(sync_snapshot(&backend, &wake).await);
            assert_eq!(calls(), before + 1);
        }
    });
}

// One stale episode consults the observer schedule only once. Lag evidence is
// stable across ticks, so unchanged lag does not write the convoy every second;
// a fresh observation clears it and allows a new episode to read a schedule.
#[tokio::test]
async fn stale_episode_reads_one_schedule_and_does_not_rewrite_lag() {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let lag = landing_launch(&backend, &home, "lag", convoy_spec(), ExitDeclaration::standard_table()).await;
    let start = Utc::now();
    let id = uuid::Uuid::new_v4();
    wake.subscriptions.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec!["cr/github.com/team/one/1 .state == merged".parse().unwrap()],
            watcher: LeafWatcher::ReconcilerWake { convoy: lag.clone() },
            maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
            freshness_demand: Some(start),
            created_at: start,
            episode_key: Default::default(),
        },
    );
    let convoys = backend.using::<Convoy>("flotilla");
    let mut notes = home.engine().memory().subscribe();
    // Each judgement waits for the views to hear the launch as it now reads, or it skips it.
    let current = || async {
        let convoy = convoys.get(&lag).await.unwrap();
        caught_up(&wake, &convoy).await;
        HashMap::from([(lag.clone(), convoy)])
    };
    for age in [0, 19, 21, 22, 23, 24] {
        let objects = current().await;
        wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
        assert_eq!(wake.subscriptions.inner.change_requests.grace_schedule_reads(), 1);
        let current = convoys.get(&lag).await.unwrap();
        assert_eq!(current.status.unwrap().stalled.is_some(), age >= 20);
        // The stall is a note on the launch: once written, unchanged lag writes nothing.
        let noted = notes_changed(&mut notes);
        if age >= 22 {
            assert!(!noted, "unchanged lag rewrote the launch at {age}s");
        }
    }
    let records = backend.using::<ChangeRequest>("flotilla");
    let name = cr_name("github.com", "team/one", 1);
    let created = records
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("team/one".into())
                .number(1)
                .observing_authority("test-host".into())
                .build(),
        )
        .await
        .unwrap();
    records
        .update_status(
            &name,
            &created.metadata.resource_version,
            &observed_request(flotilla_resources::ObservedChangeRequestState::Open, start + chrono::Duration::seconds(24)),
        )
        .await
        .unwrap();
    let objects = current().await;
    wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(24)).await.unwrap();
    assert!(convoys.get(&lag).await.unwrap().status.unwrap().stalled.is_none());
    wake.subscriptions.inner.rows.lock().await.get_mut(&id).unwrap().freshness_demand = Some(start + chrono::Duration::seconds(25));
    let objects = current().await;
    wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(25)).await.unwrap();
    assert_eq!(wake.subscriptions.inner.change_requests.grace_schedule_reads(), 2);
}

// A remote event must not materialize the replica inventory in the leaf engine.
// 320 bodies carry a 30 KB workflow/brief-sized payload; setup is outside timing.
#[tokio::test]
async fn replica_event_work_measurement() {
    use flotilla_resources::convoy::engine::Actor;
    use flotilla_resources::promises::{PromiseKind, PromiseOperation, PromiseSource};
    use flotilla_store_testkit::ReadCountsBackendExt;
    let (backend, home) = engine_backend().await;
    // The launches are another daemon's, heard here as its log arrives.
    let remote_backend = ResourceBackend::new(InMemoryBackend::default());
    let remote = EngineHome::install(&remote_backend, "flotilla", "remote");
    let mut spec = convoy_spec();
    spec.instruction = Some("x".repeat(30_000));
    let mut launches = Vec::new();
    for index in 0..320 {
        let launch = remote.admit(&format!("replica-{index}"), spec.clone(), ledger_owed()).await;
        remote.vessel(&launch, "work", "env");
        launches.push(launch);
    }
    flotilla_provisioning_testkit::replicate_home(remote.service(), "remote", home.service());
    let replicas = backend.including_replicas::<Convoy>("flotilla");
    assert_eq!(replicas.list().await.unwrap().items.len(), 320, "the remote's launches are heard here");
    let counted = backend.with_read_counts();
    let wake = supervision_wake(&counted, &home);
    wake.local_convoys("flotilla").await.unwrap();
    let cell = wake.views("flotilla").await.unwrap();
    let view = &cell.get().unwrap().convoys;
    let before = view.materialized_objects();
    let reads = counted.read_snapshot();
    let started = std::time::Instant::now();
    let other = team_one_request(1);
    for index in 0..20 {
        // Each event is an edge on the remote's first launch: its coder declares a promise, or an
        // operator links a change request, or unlinks it. It reaches here as the remote's next
        // log entries.
        let changed = remote.view(&remote_backend, &launches[0]).await;
        match index % 4 {
            0 | 2 => {
                let operation = PromiseOperation::Declare {
                    id: format!("event-{index}"),
                    kind: PromiseKind::DecisionLedger,
                    source: PromiseSource::Crew,
                };
                remote.engine().promise(&changed, "work", "coder", &operation, Actor::Crew).await.expect("the remote coder's promise");
            }
            1 => remote.engine().link(&changed, &other, SubjectDiscoverySource::Operator, "operator").await.unwrap(),
            _ => remote.engine().unlink(&changed, &other, "operator").await.unwrap(),
        }
        flotilla_provisioning_testkit::replicate_home_tail(remote.service(), "remote", home.service());
        // The leaf's view hears the event, as the launch now reads here.
        let heard = replicas.get(&launches[0]).await.expect("the replica").object.metadata.resource_version;
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut changes = view.subscribe();
            while !view.shared_named(&launches[0]).await.unwrap().iter().any(|read| read.object.metadata.resource_version == heard) {
                changes.changed().await.expect("the leaf's view runs");
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the leaf's view hears the remote's event {index} at {heard}"));
        assert!(wake.local_convoys("flotilla").await.unwrap().is_empty());
    }
    let copied = view.materialized_objects() - before;
    eprintln!("replica leaf: bodies/event={}, elapsed/event={:?}", copied / 20, started.elapsed() / 20);
    counted.read_snapshot().since(&reads).assert_zero("replica leaf event");
    assert_eq!(copied, 0, "replicas must never materialize owned inventory bodies");
    let listed = replicas.list().await.unwrap().items;
    let heard = listed.into_iter().find(|source| source.object.metadata.name == launches[0]).expect("the first launch").object;
    assert!(heard.status.unwrap().unlinked_subjects.contains(&other), "the last event is heard here");
}

// A keyed change derives only that Convoy's standing rows. An empty selection
// does nothing, and a deleted key prunes without re-evaluating other parents.
#[tokio::test]
async fn row_sync_evaluates_only_selected_convoys() {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let mut launches = Vec::new();
    for index in 0..32 {
        launches.push(launch_at(&backend, &home, &format!("local-{index}"), ledger_owed(), ConvoyPhase::Active).await);
    }
    let mut objects = backend
        .using::<Convoy>("flotilla")
        .list()
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|item| (item.metadata.name.clone(), item))
        .collect::<HashMap<_, _>>();
    wake.sync_rows("flotilla", &objects).await.unwrap();
    let cell = wake.views("flotilla").await.unwrap();
    let calls = || cell.get().unwrap().row_evaluations.load(Ordering::Relaxed);
    let before = calls();
    let selected = HashSet::from([launches[0].clone()]);
    wake.sync_rows_selected("flotilla", &objects, Some(&selected)).await.unwrap();
    assert_eq!(calls(), before + 1);
    wake.sync_rows_selected("flotilla", &objects, Some(&HashSet::new())).await.unwrap();
    assert_eq!(calls(), before + 1);
    objects.remove(&launches[0]);
    wake.sync_rows_selected("flotilla", &objects, Some(&selected)).await.unwrap();
    assert_eq!(calls(), before + 1);
}
