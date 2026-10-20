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
        review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
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

// Final state/readiness and empty claim exits do not age; unchanged final
// evidence is evaluated once. Mutable checks must still expire at the exact
// TTL, and changing the convoy's exit must invalidate a cached positive proof.
#[hegel::test]
fn final_settlement_cache_preserves_freshness_and_input_changes(tc: hegel::TestCase) {
    let field = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(3));
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for field in [0, 1, 2, 3, field] {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let mut wake = supervision_wake(&backend);
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
            let created = convoys.create(&InputMeta::builder().name("cached".into()).build(), &spec).await.unwrap();
            let exit = if field == 3 {
                ExitDeclaration::standard_table()
            } else {
                ExitDeclaration::Table(indexmap::IndexMap::from([(
                    "shipped".into(),
                    flotilla_resources::parse_leaf_template(match field {
                        0 => "$cr.state == merged",
                        1 => "$cr.ready == true",
                        _ => "$cr.checks == pass",
                    })
                    .unwrap(),
                )]))
            };
            let status = ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(exit.clone()),
                    turn_delivery: Default::default(),
                    vessels: vec![],
                }),
                ..Default::default()
            };
            convoys.update_status("cached", &created.metadata.resource_version, &status).await.unwrap();
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
            assert_eq!(calls(), before + usize::from(field == 2));
            clock.advance(chrono::Duration::seconds(181));
            assert_eq!(sync_snapshot(&backend, &wake).await, field != 2);
            assert_eq!(calls(), before + if field == 2 { 2 } else { 0 });
            let current = convoys.get("cached").await.unwrap();
            let mut changed = current.status.unwrap();
            changed.message = Some("revision changed".into());
            changed.workflow_snapshot.as_mut().unwrap().exit = Some(ExitDeclaration::Table(indexmap::IndexMap::from([(
                "rejected".into(),
                flotilla_resources::parse_leaf_template("$cr.state == closed").unwrap(),
            )])));
            // Keep a concrete subject when testing a changed exit; an empty
            // standard table remains a claim by design, so test its revision
            // invalidation without imposing an external obligation.
            if field == 3 {
                changed.workflow_snapshot.as_mut().unwrap().exit = Some(exit);
            }
            convoys.update_status("cached", &current.metadata.resource_version, &changed).await.unwrap();
            assert_eq!(sync_snapshot(&backend, &wake).await, field == 3);
            assert_eq!(calls(), before + if field == 2 { 3 } else { 1 });
        }
    });
}

// One stale episode consults the observer schedule only once. Lag evidence is
// stable across ticks, so unchanged lag does not write the convoy every second;
// a fresh observation clears it and allows a new episode to read a schedule.
#[tokio::test]
async fn stale_episode_reads_one_schedule_and_does_not_rewrite_lag() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let wake = supervision_wake(&backend);
    create_convoy(&backend, "lag", ConvoyStatus { phase: ConvoyPhase::Landing, ..Default::default() }).await;
    let start = Utc::now();
    let id = uuid::Uuid::new_v4();
    wake.subscriptions.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec!["cr/github.com/team/one/1 .state == merged".parse().unwrap()],
            watcher: LeafWatcher::ReconcilerWake { convoy: "lag".into() },
            maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
            freshness_demand: Some(start),
            created_at: start,
            episode_key: Default::default(),
        },
    );
    let convoys = backend.using::<Convoy>("flotilla");
    let mut stalled_version = None;
    for age in [0, 19, 21, 22, 23, 24] {
        let objects = HashMap::from([("lag".into(), convoys.get("lag").await.unwrap())]);
        wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
        assert_eq!(wake.subscriptions.inner.change_requests.grace_schedule_reads(), 1);
        let current = convoys.get("lag").await.unwrap();
        assert_eq!(current.status.unwrap().stalled.is_some(), age >= 20);
        if age >= 21 {
            if let Some(version) = &stalled_version {
                assert_eq!(&current.metadata.resource_version, version);
            }
            stalled_version = Some(current.metadata.resource_version);
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
    let objects = HashMap::from([("lag".into(), convoys.get("lag").await.unwrap())]);
    wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(24)).await.unwrap();
    assert!(convoys.get("lag").await.unwrap().status.unwrap().stalled.is_none());
    wake.subscriptions.inner.rows.lock().await.get_mut(&id).unwrap().freshness_demand = Some(start + chrono::Duration::seconds(25));
    let objects = HashMap::from([("lag".into(), convoys.get("lag").await.unwrap())]);
    wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(25)).await.unwrap();
    assert_eq!(wake.subscriptions.inner.change_requests.grace_schedule_reads(), 2);
}

// A remote event must not materialize the replica inventory in the leaf engine.
// 320 bodies carry a 30 KB workflow/brief-sized payload; setup is outside timing.
#[tokio::test]
async fn replica_event_work_measurement() {
    use flotilla_store_testkit::ReadCountsBackendExt;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let remote = ResourceBackend::new(InMemoryBackend::default());
    for index in 0..320 {
        create_convoy(
            &remote,
            &format!("replica-{index}"),
            ConvoyStatus { phase: ConvoyPhase::Active, message: None, ..Default::default() },
        )
        .await;
        let records = remote.using::<Convoy>("flotilla");
        let object = records.get(&format!("replica-{index}")).await.unwrap();
        let mut spec = object.spec;
        spec.instruction = Some("x".repeat(30_000));
        records.update(&InputMeta::builder().name(object.metadata.name).build(), &object.metadata.resource_version, &spec).await.unwrap();
    }
    let records = remote.using::<Convoy>("flotilla");
    let writer = backend.replica_writer::<Convoy>(flotilla_protocol::NodeId::new("remote"), "flotilla");
    writer.replace(&records.list().await.unwrap(), Utc::now()).await.unwrap();
    let counted = backend.with_read_counts();
    let wake = supervision_wake(&counted);
    wake.local_convoys("flotilla").await.unwrap();
    let cell = wake.views("flotilla").await.unwrap();
    let view = &cell.get().unwrap().convoys;
    let before = view.materialized_objects();
    let reads = counted.read_snapshot();
    let started = std::time::Instant::now();
    for index in 0..20 {
        let mut changed = records.get("replica-0").await.unwrap();
        changed.status.as_mut().unwrap().message = Some(format!("event-{index}"));
        changed.metadata.resource_version = format!("{}", 1000 + index);
        writer.apply(flotilla_resources::WatchEvent::Modified(changed), Utc::now()).await.unwrap();
        assert!(wake.local_convoys("flotilla").await.unwrap().is_empty());
    }
    let copied = view.materialized_objects() - before;
    eprintln!("replica leaf: bodies/event={}, elapsed/event={:?}", copied / 20, started.elapsed() / 20);
    counted.read_snapshot().since(&reads).assert_zero("replica leaf event");
    assert_eq!(copied, 0, "replicas must never materialize owned inventory bodies");
}

// A keyed change derives only that Convoy's standing rows. An empty selection
// does nothing, and a deleted key prunes without re-evaluating other parents.
#[tokio::test]
async fn row_sync_evaluates_only_selected_convoys() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let wake = supervision_wake(&backend);
    for index in 0..32 {
        create_convoy(&backend, &format!("local-{index}"), ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
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
    let selected = HashSet::from(["local-0".to_string()]);
    wake.sync_rows_selected("flotilla", &objects, Some(&selected)).await.unwrap();
    assert_eq!(calls(), before + 1);
    wake.sync_rows_selected("flotilla", &objects, Some(&HashSet::new())).await.unwrap();
    assert_eq!(calls(), before + 1);
    objects.remove("local-0");
    wake.sync_rows_selected("flotilla", &objects, Some(&selected)).await.unwrap();
    assert_eq!(calls(), before + 1);
}
