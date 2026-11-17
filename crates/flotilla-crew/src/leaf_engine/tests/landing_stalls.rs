use super::*;
use flotilla_store::controller::WorkQueueSender;
use flotilla_store_testkit::VirtualClock;

/// Launch `launch` as its view reads, keyed for a judge, once `wake`'s views have heard it.
async fn heard(
    backend: &ResourceBackend,
    home: &EngineHome,
    wake: &ReconcilerWake,
    launch: &str,
) -> HashMap<String, ResourceObject<Convoy>> {
    let convoy = home.view(backend, launch).await;
    caught_up(wake, &convoy).await;
    HashMap::from([(launch.to_owned(), convoy)])
}

/// Launch `launch`'s status as its view reads.
async fn status_of(backend: &ResourceBackend, home: &EngineHome, launch: &str) -> ConvoyStatus {
    home.view(backend, launch).await.status.expect("status")
}

// #3412: satisfied settlement waits on the Convoy controller, including the
// zero-subject claim exit. An idle controller is able regardless of row age.
#[tokio::test]
async fn satisfied_landing_has_controller_dependency() {
    let (backend, home) = engine_backend().await;
    let mut wake = supervision_wake(&backend, &home);
    let now = Utc::now() + chrono::Duration::days(365);
    wake.subscriptions = wake.subscriptions.with_clock(Arc::new(VirtualClock::new(now)));
    let (queue, _receiver) = WorkQueueSender::channel();
    wake.subscriptions.inner.controller_queues.lock().await.insert("flotilla".into(), queue);
    let launch = landing_launch(&backend, &home, "settled", convoy_spec(), ExitDeclaration::standard_table()).await;
    let objects = heard(&backend, &home, &wake, &launch).await;
    wake.sync_rows("flotilla", &objects).await.unwrap();
    assert!(wake.subscriptions.rows().await.iter().any(|row| matches!(&row.maker,
        LeafMaker::Controller { resource_kind, name: Some(name), .. } if resource_kind == "Convoy" && *name == launch)));
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
    assert!(status_of(&backend, &home, &launch).await.stalled.is_none());
}

// #3412: landing must clear a pre-existing stall in the transition write;
// waiting for another judge pass can leave terminal history stalled forever.
#[tokio::test]
async fn landing_transition_clears_stall_without_another_judge() {
    let case = project_supervision_case(&[]).await;
    case.judge(&case.wake, Utc::now()).await;
    let prior = case.status().await;
    assert!(prior.stalled.is_some(), "precondition: judge raised a stall");
    case.home.keep(&case.backend, &case.work, "work", "coder").await;
    case.home.end(&case.backend, &case.work, ConvoyPhase::Landed).await;
    let status = case.status().await;
    assert_eq!(status.phase, ConvoyPhase::Landed);
    assert!(status.stalled.is_none(), "terminal transition must clear the stall atomically");
    // All terminal phase rollups share the same invariant, including duplicate
    // patches and a stale attempt to reapply the old stall after the transition.
    for phase in [ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Abandoned] {
        let mut status = prior.clone();
        flotilla_resources::ConvoyStatusPatch::RollUpPhase { phase, started_at: None, finished_at: Some(Utc::now()) }.apply(&mut status);
        assert!(status.stalled.is_none());
        flotilla_resources::ConvoyStatusPatch::SetStall { condition: prior.stalled.clone().expect("the prior stall") }.apply(&mut status);
        assert!(status.stalled.is_none());
    }
}

// #3412: a stopped controller stalls with its identity, whereas an idle
// controller and one draining a backlog remain able. Queue progress, not
// total Landing residence, bounds the wait.
#[tokio::test]
async fn satisfied_landing_names_stuck_controller_and_recovers() {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let (queue, _receiver) = WorkQueueSender::channel();
    wake.subscriptions.inner.controller_queues.lock().await.insert("flotilla".into(), queue.clone());
    let launch = landing_launch(&backend, &home, "settled", convoy_spec(), ExitDeclaration::standard_table()).await;
    queue.send(launch.clone()).await.unwrap();
    let start = Utc::now();
    for age in [0, 59, 61] {
        let objects = heard(&backend, &home, &wake, &launch).await;
        wake.sync_rows("flotilla", &objects).await.unwrap();
        wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
        let status = status_of(&backend, &home, &launch).await;
        assert_eq!(status.stalled.is_some(), age >= 60);
        if let Some(stall) = status.stalled {
            assert!(stall.evidence.contains(&format!("Convoy controller for {launch}")), "{}", stall.evidence);
            assert!(stall.evidence.contains("no progress"));
        }
    }
    let (restarted, _receiver) = WorkQueueSender::channel();
    wake.subscriptions.inner.controller_queues.lock().await.insert("flotilla".into(), restarted);
    let objects = heard(&backend, &home, &wake, &launch).await;
    wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(62)).await.unwrap();
    assert!(status_of(&backend, &home, &launch).await.stalled.is_none());
}

// #3412: freshness demands are unknown through the observer's grace and
// become observer lag afterward. Fresh evidence clears the episode; Merged
// evidence stays final even when older than both demand and age thresholds.
const FORGE_FIELDS: [&str; 4] = [".state", ".ready", ".checks", ".review.actionable-at-head"];

// The grace boundary (19, 20, 21 seconds), absent values and Merged evidence
// older than the 180s stale threshold, for each field.
#[tokio::test]
async fn forge_grace_boundaries_and_merged_finality() {
    for field in FORGE_FIELDS {
        for (age, missing, merged) in [(0, false, false), (19, false, false), (20, false, false), (21, true, false), (400, false, true)] {
            forge_grace_case(field, age, missing, merged).await;
        }
    }
}

#[hegel::test]
fn forge_grace_and_merged_finality(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Ages cross the 20s grace boundary, with absent values and Merged evidence.
    let field = tc.draw(gs::sampled_from(FORGE_FIELDS.to_vec()));
    let age = tc.draw(gs::integers::<i64>().min_value(0).max_value(40));
    let missing = tc.draw(gs::booleans());
    let merged = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(forge_grace_case(field, age, missing, merged));
}

async fn forge_grace_case(field: &str, age: i64, missing: bool, merged: bool) {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let launch = landing_launch(&backend, &home, "forge", convoy_spec(), ExitDeclaration::standard_table()).await;
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
        checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, start - chrono::Duration::seconds(300)),
        review: flotilla_resources::ChangeRequestReviewObservation {
            actionable_at_head: flotilla_resources::Observation::known(false, start - chrono::Duration::seconds(300)),
        },
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
                    ".review.actionable-at-head" => "false",
                    _ => "merged",
                }
            )
            .parse()
            .unwrap()],
            watcher: LeafWatcher::ReconcilerWake { convoy: launch.clone() },
            maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
            freshness_demand: Some(start),
            created_at: start,
            episode_key: Default::default(),
        },
    );
    for _ in 0..2 {
        let objects = heard(&backend, &home, &wake, &launch).await;
        wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
        let stall = status_of(&backend, &home, &launch).await.stalled;
        assert_eq!(stall.is_some(), age >= 20 && !(merged && !missing));
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
        row.leaves[0].address = LeafAddress::ChangeRequest { service: "unconfigured.invalid".into(), scope: "team/one".into(), number: 1 };
        original
    };
    let objects = heard(&backend, &home, &wake, &launch).await;
    wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
    assert!(wake.subscriptions.inner.observer_grace.lock().await.keys().all(|(row, _)| *row != id));
    wake.subscriptions.inner.rows.lock().await.get_mut(&id).unwrap().leaves[0] = original_leaf;
    observation.state = flotilla_resources::Observation::known(
        flotilla_resources::ObservedChangeRequestState::Open,
        start + chrono::Duration::seconds(age),
    );
    observation.checks =
        flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, start + chrono::Duration::seconds(age));
    observation.review.actionable_at_head = flotilla_resources::Observation::known(false, start + chrono::Duration::seconds(age));
    let current = records.get(&name).await.unwrap();
    records.update_status(&name, &current.metadata.resource_version, &observation).await.unwrap();
    let objects = heard(&backend, &home, &wake, &launch).await;
    wake.judge_stalls_at("flotilla", &objects, start + chrono::Duration::seconds(age)).await.unwrap();
    assert!(status_of(&backend, &home, &launch).await.stalled.is_none());
}

// #3428: finished crews do not owe another turn while branch discovery is
// pending. Discovery and a live Vessel owe evidence before the Convoy controller
// owes Landed; removal of a completed Vessel discharges its checkout expectation.
#[tokio::test]
#[ignore = "waits for held-checkout expectations (engine-path-2)"]
async fn landing_discovery_window_has_an_owed_row_then_settles() {
    let (backend, home) = engine_backend().await;
    let mut wake = supervision_wake(&backend, &home);
    let now = Utc::now();
    let clock = Arc::new(VirtualClock::new(now));
    wake.subscriptions = wake.subscriptions.with_clock(clock.clone());
    let mut status = ledger_owed();
    status.workflow_snapshot.as_mut().expect("workflow").exit = Some(ExitDeclaration::standard_table());
    let launch = home.admit("discovering", convoy_spec(), status).await;
    let vessel = home.vessel(&launch, "work", "env");
    home.seat(&backend, &launch, (&vessel, "work"), ("coder", "discovering-coder"), CrewWorkPhase::Done).await;
    assert_eq!(status_of(&backend, &home, &launch).await.phase, ConvoyPhase::Landing);
    // The checkout the vessel's work was placed on, held for the launch.
    home.engine.checkouts.put_ready(
        "checkout",
        &CheckoutSpec::Observed(
            ObservedCheckoutSpec::builder()
                .r#ref("branch".into())
                .path("/unused".into())
                .repo_ref(RepositoryKey("repo".into()))
                .host_ref("test-host".into())
                .is_main(false)
                .build(),
        ),
        Some(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(launch.clone())),
    );
    enum Setup {
        PendingScan,
        RecordScan,
        RemoveVessel,
    }
    let stages = [
        (Setup::PendingScan, ("branch_subject_scan".to_owned(), "forge".to_owned())),
        (Setup::RecordScan, ("Vessel".to_owned(), vessel.clone())),
        (Setup::RemoveVessel, ("Convoy".to_owned(), launch.clone())),
    ];
    for (setup, expected_maker) in stages {
        match setup {
            Setup::PendingScan => {}
            Setup::RecordScan => {
                home.note(&backend, &launch, |status| status.branch_subject_scan_at = Some(now)).await;
            }
            Setup::RemoveVessel => home.engine.vessels.step(&vessel, VesselStep::Released),
        }
        let objects = heard(&backend, &home, &wake, &launch).await;
        wake.sync_rows("flotilla", &objects).await.unwrap();
        wake.judge_stalls_at("flotilla", &objects, now).await.unwrap();
        let status = status_of(&backend, &home, &launch).await;
        assert!(status.stalled.is_none(), "pending discovery must not be a no-row stall: {:?}", status.stalled);
        let rows = wake.subscriptions.rows().await;
        assert_eq!(rows.len(), 1);
        let actual_maker = match &rows[0].maker {
            LeafMaker::Controller { resource_kind, name: Some(name), .. } => (resource_kind.clone(), name.clone()),
            LeafMaker::Observed { refresher, external_party } => (refresher.clone(), external_party.clone()),
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
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let mut status = ledger_owed();
    status.workflow_snapshot.as_mut().expect("workflow").exit = None;
    let launch = launch_at(&backend, &home, "dead-end", status, ConvoyPhase::Landing).await;
    let objects = heard(&backend, &home, &wake, &launch).await;
    wake.sync_rows("flotilla", &objects).await.unwrap();
    assert!(wake.subscriptions.rows().await.is_empty());
    wake.judge_stalls_at("flotilla", &objects, Utc::now()).await.unwrap();
    let stall = status_of(&backend, &home, &launch).await.stalled.expect("genuine dead end stalls");
    assert_eq!(stall.source, flotilla_resources::StallEvidenceSource::LeafEngine);
    assert_eq!(stall.evidence, NO_EXIT_STALL_EVIDENCE);
}

// #4278: a deliverer submits its PR, the forge merges it, and its promises
// settle before Landing, so the crew is Done by promises and Landing records
// no claims. With an exit declared, some row names who owes settlement
// evidence: the forge observer of the bound PR, the branch scan, the
// producing Vessel or the Convoy controller.
// A workflow that omits `exit` is a Standing Convoy (CONTEXT.md): nothing can
// settle it, so it is a genuine dead end whose stall names the missing exit.
#[hegel::test]
fn merged_submission_entering_landing_owes_settlement_or_names_no_exit(tc: hegel::TestCase) {
    use flotilla_resources::convoy::engine::Actor;
    use flotilla_resources::promises::{PromiseKind, PromiseOperation, PromiseSource, Submission, SubmissionVerdict};
    use hegel::generators as gs;
    // Covers each settlement producer (scan pending, checkout absent or
    // present) and merged evidence from fresh to far older than the stale
    // threshold: merged evidence stays final.
    let declared_exit = tc.draw(gs::booleans());
    let scanned = tc.draw(gs::booleans());
    let checkout_present = tc.draw(gs::booleans());
    let merged_age = tc.draw(gs::integers::<i64>().min_value(0).max_value(3600));
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let (backend, home) = engine_backend().await;
        let mut wake = supervision_wake(&backend, &home);
        let now = Utc::now();
        wake.subscriptions = wake.subscriptions.with_clock(Arc::new(VirtualClock::new(now)));
        let workflow = flotilla_resources::single_agent_workflow_spec();
        let status = ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                cascade: None,
                stall_nudges: Default::default(),
                supervision: None,
                exit: declared_exit.then_some(workflow.exit).flatten(),
                turn_delivery: Default::default(),
                vessels: workflow.vessels,
            }),
            ..Default::default()
        };
        let launch = home.admit("merged", ConvoySpec::builder().workflow_ref("single-agent".into()).build(), status).await;
        let vessel = home.vessel(&launch, "work", "env");
        home.seat(&backend, &launch, (&vessel, "work"), ("coder", "merged-coder"), CrewWorkPhase::Working).await;
        if scanned {
            home.note(&backend, &launch, |status| status.branch_subject_scan_at = Some(now)).await;
        }
        // The coder declares the PR it delivers, submits it and its ledger, and the forge's
        // merge accepts both: its promises settle, so its seat is Done and the launch Landing.
        let merged_at = now - chrono::Duration::seconds(merged_age);
        let engine = home.engine();
        let convoy = home.view(&backend, &launch).await;
        let pr = PromiseOperation::Declare { id: "template/pr".into(), kind: PromiseKind::Pr, source: PromiseSource::Template };
        engine.promise(&convoy, "work", "coder", &pr, Actor::Crew).await.expect("the coder declares its PR");
        for (kind, reference) in [(PromiseKind::Pr, "cr/github.com/team/one/22"), (PromiseKind::DecisionLedger, "artifact/ledger")] {
            let convoy = home.view(&backend, &launch).await;
            let promise = flotilla_resources::promises::owned(convoy.status.as_ref().expect("status"), "work", "coder")
                .iter()
                .find(|promise| promise.kind == kind)
                .expect("the promise the coder owes")
                .clone();
            let submitted_at = merged_at - chrono::Duration::seconds(10);
            let submission = Submission { reference: reference.into(), metadata: BTreeMap::new(), submitted_at, verdict: None };
            let submit = PromiseOperation::Submit { id: promise.id.clone(), kind, source: promise.source, submission };
            engine.promise(&convoy, "work", "coder", &submit, Actor::Crew).await.expect("the coder submits");
            let convoy = home.view(&backend, &launch).await;
            let verdict = SubmissionVerdict { accepted: true, who: "forge".into(), at: merged_at, why: "PR merged".into() };
            let verdict = PromiseOperation::Verdict { id: promise.id, reference: reference.into(), submitted_at, verdict };
            engine.promise(&convoy, "work", "coder", &verdict, Actor::Observer).await.expect("the forge accepts");
        }
        let status = status_of(&backend, &home, &launch).await;
        assert_eq!(status.phase, ConvoyPhase::Landing);
        assert_eq!(status.landing_entry.as_ref().map(|entry| entry.claims.len()).unwrap_or_default(), 0);

        let records = backend.using::<ChangeRequest>("flotilla");
        let name = cr_name("github.com", "team/one", 22);
        let created = records
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("team/one".into())
                    .number(22)
                    .observing_authority("owner".into())
                    .build(),
            )
            .await
            .unwrap();
        let observation = flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, merged_at),
            head_sha: Default::default(),
            checks: Default::default(),
            review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
            mergeable: Default::default(),
        };
        records.update_status(&name, &created.metadata.resource_version, &observation).await.unwrap();
        if checkout_present {
            home.engine.checkouts.put_ready(
                "checkout",
                &CheckoutSpec::Observed(
                    ObservedCheckoutSpec::builder()
                        .r#ref("branch".into())
                        .path("/unused".into())
                        .repo_ref(RepositoryKey("repo".into()))
                        .host_ref("test-host".into())
                        .is_main(false)
                        .build(),
                ),
                Some(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(launch.clone())),
            );
        }

        let objects = heard(&backend, &home, &wake, &launch).await;
        wake.sync_rows("flotilla", &objects).await.unwrap();
        wake.judge_stalls_at("flotilla", &objects, now).await.unwrap();
        let stalled = status_of(&backend, &home, &launch).await.stalled;
        let rows = wake.subscriptions.rows().await;
        if declared_exit {
            assert!(stalled.is_none(), "a merged submission entering Landing must not stall: {stalled:?}");
            assert!(!rows.is_empty(), "some row must name who owes settlement evidence");
        } else {
            assert!(rows.is_empty());
            let stalled = stalled.expect("a standing convoy in Landing is a dead end");
            assert_eq!((stalled.maker, stalled.leaves), (None, Vec::new()));
            assert_eq!(stalled.evidence, NO_EXIT_STALL_EVIDENCE, "the stall must name the missing exit");
        }
    });
}

// #3595: a cached actor row can outlive its session during Landing teardown.
// A session that exited or was released is not a dead actor. (Outside
// completed work, a seat whose session ended is Interrupted, which
// interrupted_stalls covers.)
#[hegel::test]
fn landing_session_teardown_does_not_stall(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Repeat judgements across the debounce and idle grace, with generated
    // extra residence after teardown. No sleeps or process doubles.
    let residence = tc.draw(gs::integers::<i64>().min_value(1).max_value(600));
    let ending = tc.draw(gs::sampled_from(vec!["exited", "released"]));
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let case = actor_nudge_scenario(&[]).await;
        let start = Utc::now();
        observe_actor(&case, &case.wake, TerminalAttentionState::Working, start).await;
        // The coder's work is complete: the launch is Landing.
        case.home.keep(&case.backend, &case.work, "work", "coder").await;
        assert_eq!(case.status().await.phase, ConvoyPhase::Landing);
        let session = case.launch("resumed-coder");
        match ending {
            "exited" => case.home.engine.terminals.exited(&session, "the agent exited"),
            _ => case.home.engine.terminals.release(&session),
        }
        for seconds in [1, 10, 60, 120, 180, 240, 300, 300 + residence] {
            case.caught_up(&case.wake).await;
            case.wake.sync_rows("flotilla", &case.objects().await).await.expect("derive the Landing launch's rows");
            case.judge(&case.wake, start + chrono::Duration::seconds(seconds)).await;
            let status = case.status().await;
            assert!(status.stalled.is_none(), "Landing/{ending} at {seconds}s: {:?}", status.stalled);
        }
        // Retiring a stale actor must not hide a real controller failure.
        let id = uuid::Uuid::new_v4();
        let mut row = case.wake.subscriptions.rows().await.into_iter().next().unwrap();
        row.id = id;
        row.maker = LeafMaker::Controller {
            resource_kind: "CredentialDelivery".into(),
            name: Some("env".into()),
            retry: ControllerRetry::terminal(None, start, "credential controller refused"),
            ceiling: RetryCeiling::default(),
        };
        case.wake.subscriptions.inner.rows.lock().await.insert(id, row);
        case.judge(&case.wake, start + chrono::Duration::hours(1)).await;
        let stall = case.status().await.stalled.unwrap();
        assert!(stall.evidence.contains("credential controller refused"), "{}", stall.evidence);
    });
}

// #4038: a Pending convoy waiting for host capacity holds a stall that only the
// Convoy controller owns. The judge keeps it, so it is neither cleared nor
// restarted every few seconds.
fn pending_stall(cause: Option<flotilla_resources::StallCause>) -> flotilla_resources::StalledCondition {
    flotilla_resources::StalledCondition {
        leaves: Vec::new(),
        maker: None,
        evidence: "capacity for fulfilment `docker-crew-image` on host `feta` is unavailable (no free vessel slots)".into(),
        source: StallEvidenceSource::LeafEngine,
        cause,
        began_at: Utc::now() - chrono::Duration::minutes(5),
        rung: StallRung::Operator,
        supervisor: None,
        supervision_message: None,
        supervision_index: None,
        supervision_exhausted: false,
        reason: None,
        proposed_disposition: None,
        nudge_history: Vec::new(),
    }
}

/// A launch admitted with nothing started, its home noting `stall` on it: one waiting for capacity.
async fn waiting_launch(backend: &ResourceBackend, home: &EngineHome, stall: flotilla_resources::StalledCondition) -> String {
    let launch = create_convoy(home, "waiting", single_agent("coder")).await;
    home.note(backend, &launch, |status| status.stalls.set(stall)).await;
    launch
}

#[tokio::test]
async fn judge_keeps_the_convoy_controllers_capacity_stall() {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let stall = pending_stall(Some(flotilla_resources::StallCause::Capacity));
    let launch = waiting_launch(&backend, &home, stall.clone()).await;
    let objects = heard(&backend, &home, &wake, &launch).await;
    wake.sync_rows("flotilla", &objects).await.unwrap();
    wake.judge_stalls("flotilla", &objects).await.unwrap();
    assert_eq!(status_of(&backend, &home, &launch).await.stalled, Some(stall));
}

// #4038: the capacity exemption is only for the controller's stall; any other
// stall on a convoy with nothing actionable is still cleared.
#[tokio::test]
async fn judge_still_clears_other_stalls_on_a_convoy_with_nothing_actionable() {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let launch = waiting_launch(&backend, &home, pending_stall(None)).await;
    let objects = heard(&backend, &home, &wake, &launch).await;
    wake.sync_rows("flotilla", &objects).await.unwrap();
    wake.judge_stalls("flotilla", &objects).await.unwrap();
    assert!(status_of(&backend, &home, &launch).await.stalled.is_none());
}
