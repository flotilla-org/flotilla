use super::*;
use flotilla_store_memory::InMemoryBackend;

// #3412: satisfied settlement waits on the Convoy controller, including the
// zero-subject claim exit. An idle controller is able regardless of row age.
#[tokio::test]
async fn satisfied_landing_has_controller_dependency() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let mut wake = supervision_wake(&backend);
    let now = Utc::now() + chrono::Duration::days(365);
    wake.subscriptions = wake.subscriptions.with_clock(Arc::new(flotilla_store_testkit::VirtualClock::new(now)));
    let (queue, _receiver) = flotilla_store::controller::WorkQueueSender::channel();
    wake.subscriptions.inner.controller_queues.lock().await.insert("flotilla".into(), queue);
    create_convoy(
        &backend,
        "settled",
        ConvoyStatus {
            phase: ConvoyPhase::Landing,
            workflow_snapshot: Some(WorkflowSnapshot {
                cascade: None,
                stall_nudges: Default::default(),
                supervision: None,
                exit: Some(ExitDeclaration::standard_table()),
                turn_delivery: Default::default(),
                vessels: vec![],
            }),
            ..Default::default()
        },
    )
    .await;
    let convoy = backend.using::<Convoy>("flotilla").get("settled").await.unwrap();
    let objects = HashMap::from([("settled".into(), convoy)]);
    wake.sync_rows("flotilla", &objects).await.unwrap();
    assert!(wake.subscriptions.rows().await.iter().any(|row| matches!(&row.maker,
        LeafMaker::Controller { resource_kind, name: Some(name), .. } if resource_kind == "Convoy" && name == "settled")));
    // #3419: synthetic dependency timestamps follow the injected judge clock.
    let row = wake
        .subscriptions
        .rows()
        .await
        .into_iter()
        .find(|row| {
            matches!(&row.maker,
        LeafMaker::Controller { resource_kind, .. } if resource_kind == "Convoy")
        })
        .unwrap();
    assert_eq!(row.created_at, now);
    let LeafMaker::Controller { retry, .. } = row.maker else { unreachable!() };
    assert_eq!(retry.first_failure_at, now);
    assert_eq!(retry.next_attempt_at(), Some(now));
    wake.judge_stalls_at("flotilla", &objects, Utc::now() + chrono::Duration::hours(2)).await.unwrap();
    assert!(backend.using::<Convoy>("flotilla").get("settled").await.unwrap().status.unwrap().stalled.is_none());
}

// #3412: landing must clear a pre-existing stall in the transition write;
// waiting for another judge pass can leave terminal history stalled forever.
#[tokio::test]
async fn landing_transition_clears_stall_without_another_judge() {
    let (backend, wake, _) = project_supervision_case(&[]).await;
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.unwrap();
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), convoy)])).await.unwrap();
    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.get("stalled-work").await.unwrap();
    assert!(convoy.status.as_ref().unwrap().stalled.is_some(), "precondition: judge raised a stall");
    flotilla_store::apply_status_patch(
        &convoys,
        "stalled-work",
        &flotilla_resources::ConvoyStatusPatch::Settle {
            disposition: "claim".into(),
            target_mismatches: vec![],
            finished_at: Utc::now(),
            evidence: None,
        },
    )
    .await
    .unwrap();
    let status = convoys.get("stalled-work").await.unwrap().status.unwrap();
    assert_eq!(status.phase, ConvoyPhase::Landed);
    assert!(status.stalled.is_none(), "terminal transition must clear the stall atomically");
    // All terminal phase rollups share the same invariant, including duplicate
    // patches and a stale attempt to reapply the old stall after the transition.
    for phase in [ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Abandoned] {
        let prior = convoy.status.as_ref().unwrap();
        let mut status = prior.clone();
        flotilla_resources::ConvoyStatusPatch::RollUpPhase { phase, started_at: None, finished_at: Some(Utc::now()) }.apply(&mut status);
        assert!(status.stalled.is_none());
        flotilla_resources::ConvoyStatusPatch::SetStalled { condition: prior.stalled.clone() }.apply(&mut status);
        assert!(status.stalled.is_none());
    }
}

// #3412: a stopped controller stalls with its identity, whereas an idle
// controller and one draining a backlog remain able. Queue progress, not
// total Landing residence, bounds the wait.
#[tokio::test]
async fn satisfied_landing_names_stuck_controller_and_recovers() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let wake = supervision_wake(&backend);
    let (queue, _receiver) = flotilla_store::controller::WorkQueueSender::channel();
    wake.subscriptions.inner.controller_queues.lock().await.insert("flotilla".into(), queue.clone());
    let status = ConvoyStatus {
        phase: ConvoyPhase::Landing,
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            stall_nudges: Default::default(),
            supervision: None,
            exit: Some(ExitDeclaration::standard_table()),
            turn_delivery: Default::default(),
            vessels: vec![],
        }),
        ..Default::default()
    };
    create_convoy(&backend, "settled", status).await;
    let convoys = backend.using::<Convoy>("flotilla");
    queue.send("settled".into()).await.unwrap();
    let start = Utc::now();
    for age in [0, 59, 61] {
        let objects = HashMap::from([("settled".into(), convoys.get("settled").await.unwrap())]);
        wake.sync_rows("flotilla", &objects).await.unwrap();
        wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
        let status = convoys.get("settled").await.unwrap().status.unwrap();
        assert_eq!(status.stalled.is_some(), age >= 60);
        if let Some(stall) = status.stalled {
            assert!(stall.evidence.contains("Convoy controller for settled"), "{}", stall.evidence);
            assert!(stall.evidence.contains("no progress"));
        }
    }
    let (restarted, _receiver) = flotilla_store::controller::WorkQueueSender::channel();
    wake.subscriptions.inner.controller_queues.lock().await.insert("flotilla".into(), restarted);
    let objects = HashMap::from([("settled".into(), convoys.get("settled").await.unwrap())]);
    wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(62)).await.unwrap();
    assert!(convoys.get("settled").await.unwrap().status.unwrap().stalled.is_none());
}

// #3412: freshness demands are unknown through the observer's grace and
// become observer lag afterward. Fresh evidence clears the episode; Merged
// evidence stays final even when older than both demand and age thresholds.
#[hegel::test]
fn forge_grace_and_merged_finality(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Ages cross the 20s grace boundary; include absent values and Merged
    // evidence older than the 180s stale threshold.
    let age = tc.draw(gs::integers::<i64>().min_value(0).max_value(40));
    let missing = tc.draw(gs::booleans());
    let merged = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for field in [".state", ".ready", ".checks"] {
            for (age, missing, merged) in
                [(0, false, false), (19, false, false), (20, false, false), (21, true, false), (400, false, true), (age, missing, merged)]
            {
                let backend = ResourceBackend::new(InMemoryBackend::default());
                let wake = supervision_wake(&backend);
                create_convoy(&backend, "forge", ConvoyStatus { phase: ConvoyPhase::Landing, ..Default::default() }).await;
                let start = Utc::now();
                let records = backend.using::<ChangeRequest>("flotilla");
                let name = cr_name("github.com", "team/one", 1);
                let created = records
                    .create(
                        &InputMeta::builder().name(name.clone()).build(),
                        &flotilla_resources::ChangeRequestSpec::builder()
                            .service("github.com".into())
                            .scope("team/one".into())
                            .number(1)
                            .observing_authority("owner".into())
                            .build(),
                    )
                    .await
                    .unwrap();
                let mut observation = flotilla_resources::ChangeRequestStatus {
                    title: Default::default(),
                    author: Default::default(),
                    review_decision: Default::default(),
                    review_requested_from_owner: Default::default(),
                    state: flotilla_resources::Observation::known(
                        if merged {
                            flotilla_resources::ObservedChangeRequestState::Merged
                        } else {
                            flotilla_resources::ObservedChangeRequestState::Open
                        },
                        start - chrono::Duration::seconds(300),
                    ),
                    head_sha: Default::default(),
                    checks: flotilla_resources::Observation::known(
                        flotilla_resources::ObservedChecks::Pass,
                        start - chrono::Duration::seconds(300),
                    ),
                    review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
                    mergeable: Default::default(),
                };
                if !missing {
                    records.update_status(&name, &created.metadata.resource_version, &observation).await.unwrap();
                }
                let id = uuid::Uuid::new_v4();
                wake.subscriptions.inner.rows.lock().await.insert(
                    id,
                    LeafSubscriptionRow {
                        id,
                        namespace: "flotilla".into(),
                        leaves: vec![format!(
                            "cr/github.com/team/one/1 {field} == {}",
                            match field {
                                ".ready" => "true",
                                ".checks" => "pass",
                                _ => "merged",
                            }
                        )
                        .parse()
                        .unwrap()],
                        watcher: LeafWatcher::ReconcilerWake { convoy: "forge".into() },
                        maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
                        freshness_demand: Some(start),
                        created_at: start,
                        episode_key: Default::default(),
                    },
                );
                let convoys = backend.using::<Convoy>("flotilla");
                for _ in 0..2 {
                    let objects = HashMap::from([("forge".into(), convoys.get("forge").await.unwrap())]);
                    wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
                    let stall = convoys.get("forge").await.unwrap().status.unwrap().stalled;
                    assert_eq!(stall.is_some(), age >= 20 && !(merged && !missing && field != ".checks"));
                    if let Some(stall) = stall {
                        assert!(stall.evidence.contains("change_request observer"));
                        assert!(stall.evidence.contains("observer behind scheduled refresh grace"));
                    }
                }
                // #3419: losing address resolution clears a pinned grace entry,
                // even if the standing row has not yet been unsubscribed.
                let original_leaf = {
                    let mut rows = wake.subscriptions.inner.rows.lock().await;
                    let row = rows.get_mut(&id).unwrap();
                    let original = row.leaves[0].clone();
                    row.leaves[0].address =
                        LeafAddress::ChangeRequest { service: "unconfigured.invalid".into(), scope: "team/one".into(), number: 1 };
                    original
                };
                let objects = HashMap::from([("forge".into(), convoys.get("forge").await.unwrap())]);
                wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
                assert!(wake.subscriptions.inner.observer_grace.lock().await.keys().all(|(row, _)| *row != id));
                wake.subscriptions.inner.rows.lock().await.get_mut(&id).unwrap().leaves[0] = original_leaf;
                observation.state = flotilla_resources::Observation::known(
                    flotilla_resources::ObservedChangeRequestState::Open,
                    start + chrono::Duration::seconds(age),
                );
                observation.checks = flotilla_resources::Observation::known(
                    flotilla_resources::ObservedChecks::Pass,
                    start + chrono::Duration::seconds(age),
                );
                let current = records.get(&name).await.unwrap();
                records.update_status(&name, &current.metadata.resource_version, &observation).await.unwrap();
                let objects = HashMap::from([("forge".into(), convoys.get("forge").await.unwrap())]);
                wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
                assert!(convoys.get("forge").await.unwrap().status.unwrap().stalled.is_none());
            }
        }
    });
}
