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

// #3428: finished crews do not owe another turn while branch discovery is
// pending. Discovery and a live Vessel owe evidence before the Convoy controller
// owes Landed; removal of a completed Vessel discharges its checkout expectation.
#[tokio::test]
async fn landing_discovery_window_has_an_owed_row_then_settles() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let mut wake = supervision_wake(&backend);
    let now = Utc::now();
    let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(now));
    wake.subscriptions = wake.subscriptions.with_clock(clock.clone());
    create_convoy(
        &backend,
        "discovering",
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
            work: BTreeMap::from([(
                "work".into(),
                WorkState::builder()
                    .phase(WorkPhase::Complete)
                    .placement(flotilla_resources::PlacementStatus {
                        fields: BTreeMap::from([("checkout_refs".into(), serde_json::json!({"repo": "checkout"}))]),
                    })
                    .build(),
            )]),
            crew_work: BTreeMap::from([(
                "work".into(),
                BTreeMap::from_iter(
                    ["governor", "aide"]
                        .map(|role| (role.into(), flotilla_resources::CrewWorkState::builder().phase(CrewWorkPhase::Done).build())),
                ),
            )]),
            ..Default::default()
        },
    )
    .await;
    let convoys = backend.using::<Convoy>("flotilla");
    let vessels = backend.using::<Vessel>("flotilla");
    let vessel = vessels
        .create(
            &InputMeta::builder().name("producer".into()).labels(BTreeMap::from([(CONVOY_LABEL.into(), "discovering".into())])).build(),
            &flotilla_resources::VesselSpec {
                convoy_ref: "discovering".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    vessels
        .update_status(
            "producer",
            &vessel.metadata.resource_version,
            &flotilla_resources::VesselStatus {
                checkout_refs: BTreeMap::from([(RepositoryKey("repo".into()), "checkout".into())]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    enum Setup {
        PendingScan,
        RecordScan,
        RemoveVessel,
    }
    let stages = [
        (Setup::PendingScan, ("branch_subject_scan", "forge")),
        (Setup::RecordScan, ("Vessel", "producer")),
        (Setup::RemoveVessel, ("Convoy", "discovering")),
    ];
    for (setup, expected_maker) in stages {
        match setup {
            Setup::PendingScan => {}
            Setup::RecordScan => {
                flotilla_store::apply_status_patch(
                    &convoys,
                    "discovering",
                    &flotilla_resources::ConvoyStatusPatch::RecordBranchSubjectScan { at: now },
                )
                .await
                .unwrap();
            }
            Setup::RemoveVessel => {
                vessels.delete("producer").await.unwrap();
            }
        }
        let objects = HashMap::from([("discovering".into(), convoys.get("discovering").await.unwrap())]);
        wake.sync_rows("flotilla", &objects).await.unwrap();
        wake.judge_stalls_at("flotilla", &objects, now).await.unwrap();
        let status = convoys.get("discovering").await.unwrap().status.unwrap();
        assert!(status.stalled.is_none(), "pending discovery must not be a no-row stall: {:?}", status.stalled);
        let rows = wake.subscriptions.rows().await;
        assert_eq!(rows.len(), 1);
        let actual_maker = match &rows[0].maker {
            LeafMaker::Controller { resource_kind, name: Some(name), .. } => (resource_kind.as_str(), name.as_str()),
            LeafMaker::Observed { refresher, external_party } => (refresher.as_str(), external_party.as_str()),
            maker => panic!("unexpected settlement producer: {maker:?}"),
        };
        assert_eq!(actual_maker, expected_maker);
        if matches!(setup, Setup::RecordScan) {
            // Producer identity, rather than synthetic retry timestamps, keeps
            // the Vessel subscription stable as rederivation advances time.
            let original = &rows[0];
            for seconds in [1, 31, 181] {
                clock.set(now + chrono::Duration::seconds(seconds));
                wake.sync_rows("flotilla", &objects).await.expect("rederive with advancing clock");
                let rederived = wake.subscriptions.rows().await;
                assert_eq!(rederived.len(), 1);
                assert_eq!(rederived[0].id, original.id, "rederivation must retain the standing subscription");
                assert_eq!(rederived[0].created_at, original.created_at);
            }
        }
    }
}

// #3428: a Landing convoy with no exit and no evidence producer still has
// no route forward. Keep the empty-row stall and its diagnostic evidence.
#[tokio::test]
async fn landing_with_nothing_owed_stalls_with_evidence() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let wake = supervision_wake(&backend);
    create_convoy(&backend, "dead-end", ConvoyStatus { phase: ConvoyPhase::Landing, ..Default::default() }).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let objects = HashMap::from([("dead-end".into(), convoys.get("dead-end").await.unwrap())]);
    wake.sync_rows("flotilla", &objects).await.unwrap();
    assert!(wake.subscriptions.rows().await.is_empty());
    wake.judge_stalls_at("flotilla", &objects, Utc::now()).await.unwrap();
    let stall = convoys.get("dead-end").await.unwrap().status.unwrap().stalled.expect("genuine dead end stalls");
    assert_eq!(stall.source, flotilla_resources::StallEvidenceSource::LeafEngine);
    assert_eq!(stall.evidence, "no armed row with an able maker");
}

// #3428: Git owes missing/stale digest evidence while the checkout exists.
// Fresh matching evidence transfers the obligation to the Convoy controller;
// a fresh mismatch or an absent producer remains a genuine dead end.
#[hegel::test]
fn landing_digest_evidence_obligations_follow_observations(tc: hegel::TestCase) {
    // Duplicate refreshes and arbitrary state changes cross the exact TTL;
    // fixed boundary cases guarantee that every obligation transition runs.
    let qualified_branch = tc.draw(hegel::generators::booleans());
    let count = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(5));
    let mut steps = vec![0, 1, 2, 3];
    steps.extend((0..count).map(|_| tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(3))));
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let mut wake = supervision_wake(&backend);
        let now = Utc::now();
        wake.subscriptions = wake.subscriptions.with_clock(Arc::new(flotilla_store_testkit::VirtualClock::new(now)));
        create_convoy(
            &backend,
            "digest",
            ConvoyStatus {
                phase: ConvoyPhase::Landing,
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([(
                        "coder".into(),
                        CrewWorkState::builder()
                            .phase(CrewWorkPhase::Done)
                            .claim_evidence(flotilla_resources::SettlementClaimEvidence {
                                refs: flotilla_resources::ReviewRefPair { base: "main".into(), head: "refs/heads/branch".into() },
                                bundle_url: "https://example.test/bundle".into(),
                                claimed_head_digest: "expected".into(),
                            })
                            .build(),
                    )]),
                )]),
                ..Default::default()
            },
        )
        .await;
        let checkouts = backend.using::<Checkout>("flotilla");
        checkouts
            .create(
                &InputMeta::builder()
                    .name("digest-checkout".into())
                    .labels(BTreeMap::from([(CONVOY_LABEL.into(), "digest".into())]))
                    .build(),
                &CheckoutSpec::Observed(
                    ObservedCheckoutSpec::builder()
                        .r#ref(if qualified_branch { "refs/heads/branch" } else { "branch" }.into())
                        .path("/unused".into())
                        .repo_ref(RepositoryKey("repo".into()))
                        .host_ref("host".into())
                        .is_main(false)
                        .build(),
                ),
            )
            .await
            .unwrap();
        let convoys = backend.using::<Convoy>("flotilla");
        for step in steps {
            let checkout = checkouts.get("digest-checkout").await.unwrap();
            let mut status = flotilla_resources::CheckoutStatus::default();
            if step != 0 {
                status.integration.remote_refs.insert(
                    "refs/heads/branch".into(),
                    flotilla_resources::RemoteRefObservation {
                        digest: if step == 3 { "different" } else { "expected" }.into(),
                        observed_at: (if step == 1 {
                            now - chrono::Duration::from_std(flotilla_resources::LANDING_EVIDENCE_TTL).unwrap()
                        } else {
                            now
                        })
                        .to_rfc3339(),
                    },
                );
            }
            checkouts.update_status("digest-checkout", &checkout.metadata.resource_version, &status).await.unwrap();
            let objects = HashMap::from([("digest".into(), convoys.get("digest").await.unwrap())]);
            wake.sync_rows("flotilla", &objects).await.unwrap();
            wake.judge_stalls_at("flotilla", &objects, now).await.unwrap();
            assert_eq!(convoys.get("digest").await.unwrap().status.unwrap().stalled.is_some(), step == 3);
            let rows = wake.subscriptions.rows().await;
            if step < 2 {
                assert!(rows
                    .iter()
                    .any(|row| matches!(&row.maker, LeafMaker::Observed { refresher, .. } if refresher == "checkout_integration")));
            } else if step == 2 {
                assert!(rows
                    .iter()
                    .any(|row| matches!(&row.maker, LeafMaker::Controller { resource_kind, .. } if resource_kind == "Convoy")));
            } else {
                assert!(rows.is_empty());
            }
        }
        checkouts.delete("digest-checkout").await.unwrap();
        let objects = HashMap::from([("digest".into(), convoys.get("digest").await.unwrap())]);
        wake.sync_rows("flotilla", &objects).await.unwrap();
        wake.judge_stalls_at("flotilla", &objects, now).await.unwrap();
        assert!(wake.subscriptions.rows().await.is_empty());
        assert!(convoys.get("digest").await.unwrap().status.unwrap().stalled.is_some());
        checkouts
            .create(
                &InputMeta::builder()
                    .name("unrelated-checkout".into())
                    .labels(BTreeMap::from([(CONVOY_LABEL.into(), "digest".into())]))
                    .build(),
                &CheckoutSpec::Observed(
                    ObservedCheckoutSpec::builder()
                        .r#ref("other-branch".into())
                        .path("/unused".into())
                        .repo_ref(RepositoryKey("repo".into()))
                        .host_ref("host".into())
                        .is_main(false)
                        .build(),
                ),
            )
            .await
            .unwrap();
        let objects = HashMap::from([("digest".into(), convoys.get("digest").await.unwrap())]);
        wake.sync_rows("flotilla", &objects).await.unwrap();
        wake.judge_stalls_at("flotilla", &objects, now).await.unwrap();
        assert!(wake.subscriptions.rows().await.is_empty(), "an unrelated branch cannot produce the missing ref");
        assert!(convoys.get("digest").await.unwrap().status.unwrap().stalled.is_some());
    });
}

// #3595: a cached actor row can outlive its session during Landing teardown.
// Deletion, a stopped session, and an already removed record are not dead actors;
// Active session loss must still be supervised, including without a project.
#[hegel::test]
fn landing_session_teardown_does_not_stall(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Repeat fresh idle observations across the debounce and idle grace, with
    // generated extra residence after teardown. No sleeps or process doubles.
    let residence = tc.draw(gs::integers::<i64>().min_value(1).max_value(600));
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for (phase, complete) in
            [(ConvoyPhase::Active, false), (ConvoyPhase::Active, true), (ConvoyPhase::Landing, false), (ConvoyPhase::Landing, true)]
        {
            let teardown = phase == ConvoyPhase::Landing && complete;
            for ending in ["stopped", "absent", "deleting"] {
                let (backend, wake, _) = actor_nudge_scenario(&[]).await;
                let convoys = backend.using::<Convoy>("flotilla");
                let convoy = convoys.get("stalled-work").await.unwrap();
                let mut spec = convoy.spec.clone();
                spec.project_ref = None;
                convoys.update(&InputMeta::from(&convoy.metadata), &convoy.metadata.resource_version, &spec).await.unwrap();
                let start = Utc::now();
                observe_actor(&backend, &wake, TerminalAttentionState::Working, start).await;
                update_convoy(&backend, "stalled-work", |status| {
                    status.phase = phase;
                    if complete {
                        status.work.get_mut("work").unwrap().phase = WorkPhase::Complete;
                    }
                })
                .await;
                let sessions = backend.using::<TerminalSession>("flotilla");
                let session = sessions.get("resumed-coder").await.unwrap();
                if ending == "absent" {
                    sessions.delete("resumed-coder").await.unwrap();
                } else if ending == "deleting" {
                    // A finalizer retains the record while teardown ends the runtime.
                    let mut meta = InputMeta::from(&session.metadata);
                    meta.finalizers = vec!["flotilla.work/terminal-teardown".into()];
                    sessions.update(&meta, &session.metadata.resource_version, &session.spec).await.unwrap();
                    sessions.delete("resumed-coder").await.unwrap();
                    assert!(sessions.get("resumed-coder").await.unwrap().metadata.deletion_timestamp.is_some());
                } else {
                    let mut status = session.status.unwrap();
                    flotilla_resources::TerminalSessionStatusPatch::MarkStopped {
                        stopped_at: start + chrono::Duration::seconds(1),
                        inner_command_status: None,
                        inner_exit_code: None,
                        message: None,
                    }
                    .apply(&mut status);
                    sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.unwrap();
                }
                let mut observed_stall = false;
                for seconds in [1, 10, 60, 120, 180, 240, 300, 300 + residence] {
                    let now = start + chrono::Duration::seconds(seconds);
                    if ending == "deleting" {
                        observe_actor(&backend, &wake, TerminalAttentionState::Idle, now).await;
                    } else {
                        let convoy = convoys.get("stalled-work").await.unwrap();
                        wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), now).await.unwrap();
                    }
                    let status = convoys.get("stalled-work").await.unwrap().status.unwrap();
                    if teardown {
                        assert!(status.stalled.is_none(), "{phase:?}/{ending} at {seconds}s: {:?}", status.stalled);
                    } else if let Some(stall) = status.stalled {
                        observed_stall = true;
                        // Missing-governor diagnostics remain useful on a real stall.
                        assert!(
                            stall.evidence.contains("cannot find governor: convoy has no project_ref") || stall.rung == StallRung::Nudge
                        );
                    }
                }
                if !teardown {
                    assert!(observed_stall, "{ending} must still stall outside completed Landing work");
                }
                if teardown {
                    // Retiring a stale actor must not hide a real controller failure.
                    let id = uuid::Uuid::new_v4();
                    let mut row = wake.subscriptions.rows().await.into_iter().next().unwrap();
                    row.id = id;
                    row.maker = LeafMaker::Controller {
                        resource_kind: "CredentialDelivery".into(),
                        name: Some("env".into()),
                        retry: ControllerRetry::terminal(None, start, "credential controller refused"),
                        ceiling: RetryCeiling::default(),
                    };
                    wake.subscriptions.inner.rows.lock().await.insert(id, row);
                    let convoy = convoys.get("stalled-work").await.unwrap();
                    wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), start + chrono::Duration::hours(1))
                        .await
                        .unwrap();
                    let stall = convoys.get("stalled-work").await.unwrap().status.unwrap().stalled.unwrap();
                    assert!(stall.evidence.contains("credential controller refused"), "{}", stall.evidence);
                }
            }
        }
    });
}
