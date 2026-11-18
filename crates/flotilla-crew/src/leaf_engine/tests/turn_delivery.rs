use super::*;
use flotilla_resources::queued_turn_evidence;
use flotilla_resources::{Observation, ObservedChangeRequestState, ObservedMergeability};
use flotilla_store_memory::InMemoryBackend;
use tracing::instrument::WithSubscriber;

use crate::leaf_engine::turn_delivery::{DeliveryError, FailureWriter, PendingFailureMap};

// #2684: queue acceptance remains queued for every attention state. Only the
// queued Message's delivery confirms it; age raises advisory attention.
// The ages at the bound, for every rung and attention state, so shrinking cannot hide them.
#[tokio::test]
async fn queued_turn_bound_boundaries() {
    for rung in [TurnDeliveryRung::WarmSession, TurnDeliveryRung::FreshAgent] {
        for state in 0..=3 {
            for age in [-1, 0, 299, 300, 301] {
                queued_turn_case(rung, state, age).await;
            }
        }
    }
}

#[hegel::test]
fn queued_turn_waits_for_receipt_and_exposes_bound(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // All attention states, missing sessions and ages across the bound.
    let rung = if tc.draw(gs::booleans()) { TurnDeliveryRung::WarmSession } else { TurnDeliveryRung::FreshAgent };
    let state = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(601));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(queued_turn_case(rung, state, age));
}

async fn queued_turn_case(rung: TurnDeliveryRung, state: usize, age: i64) {
    let case = idle_nudge_scenario().await;
    let start = Utc::now();
    let now = start + chrono::Duration::seconds(age);
    // The original firing need not remain eligible: health still follows receipts.
    interrupt(&case);
    let mut status = case.status().await;
    assert_eq!(status.phase, ConvoyPhase::Interrupted);
    status.attention = None;
    status.stalls.clear();
    status.turn_deliveries.insert("review".into(), queued_turn(rung, start, start, "turn"));
    // Episodes, including their original age, survive the status's wire form.
    let status: ConvoyStatus = serde_json::from_value(serde_json::to_value(status).unwrap()).unwrap();
    case.note(|noted| *noted = status).await;
    message_to_coder(&case, "turn", "review wake").await;
    let attention = TerminalAttention {
        state: [
            TerminalAttentionState::Unobservable,
            TerminalAttentionState::Working,
            TerminalAttentionState::Idle,
            TerminalAttentionState::NeedsInput,
        ][state],
        source: TerminalAttentionSource::Screen,
        as_of: now,
    };
    observe_session(&case.home, "resumed-coder", Some(attention));
    let mut notes = case.home.engine().memory().subscribe();
    for repeat in 0..2 {
        notes_changed(&mut notes);
        case.wake.judge_stalls_at("flotilla", &case.objects().await, now).await.unwrap();
        if repeat == 1 {
            assert!(!notes_changed(&mut notes), "unchanged queue evidence must not write every tick");
        }
        let status = case.status().await;
        assert!(matches!(status.turn_deliveries["review"].episodes[0].outcome, TurnDeliveryOutcome::Queued { .. }), "repeat {repeat}");
        assert_eq!(
            status.attention.as_ref().map(|attention| attention.source.as_str()),
            (age > 300).then_some(ConvoyAttention::QUEUED_TURN_SOURCE)
        );
        let TurnDeliveryOutcome::Queued { queued_at, blocking_reason, .. } = &status.turn_deliveries["review"].episodes[0].outcome else {
            panic!("queued")
        };
        assert_eq!(*queued_at, start);
        assert!(blocking_reason.contains("waiting for its consumer"), "{blocking_reason}");
    }
    // Terminal settlement can race a submission receipt. It must neither
    // strand the episode nor retain its queued-turn advisory.
    let terminal_phase = [ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Abandoned][state % 3];
    if terminal_phase == ConvoyPhase::Landed {
        // A launch is delivered once what it owes is kept.
        case.home.keep(&case.backend, &case.work, "work", "coder").await;
    }
    case.home.end(&case.backend, &case.work, terminal_phase).await;
    deliver_to_coder(&case, "turn", now).await;
    case.caught_up(&case.wake).await;
    case.wake.judge_stalls_at("flotilla", &case.objects().await, now).await.unwrap();
    let status = case.status().await;
    assert!(matches!(status.turn_deliveries["review"].episodes[0].outcome, TurnDeliveryOutcome::Delivered {
                rung: delivered_rung, ..
            } if delivered_rung == rung));
    assert!(status.attention.is_none());
    assert_eq!(status.phase, terminal_phase, "receipt observations must preserve the terminal outcome");
    assert_eq!(status.turn_deliveries["review"].episodes.len(), 1);
    assert_eq!(queued_turn_evidence(None, None), (false, "terminal session unavailable".into()));
}

// A health tick applies all queued observations atomically, with one resource
// watch event even when several blockers and the advisory attention change.
#[tokio::test]
async fn queued_turn_observations_write_once_per_convoy() {
    use futures::FutureExt;
    let case = idle_nudge_scenario().await;
    let now = Utc::now();
    let convoys = case.backend.using::<Convoy>("flotilla");
    interrupt(&case);
    let convoy = case
        .note(|status| {
            status.stalls.clear();
            status.attention = None;
            for source in ["review", "checks"] {
                let queued = queued_turn(TurnDeliveryRung::WarmSession, now, now - chrono::Duration::seconds(301), source);
                status.turn_deliveries.insert(source.into(), queued);
            }
        })
        .await;
    assert_eq!(convoy.status.as_ref().unwrap().phase, ConvoyPhase::Interrupted);
    let mut watch = convoys.watch(WatchStart::Now).await.unwrap();
    case.wake.observe_queued_turns("flotilla", &convoy, &BTreeMap::new(), now).await.unwrap();
    assert!(matches!(watch.next().await.unwrap().unwrap(), WatchEvent::Modified(_)));
    assert!(watch.next().now_or_never().is_none(), "one tick must emit only one status update");
    let observed = case.convoy().await;
    let attention = observed.status.as_ref().unwrap().attention.as_ref().unwrap();
    assert!(attention.reason.contains("review"));
    assert!(attention.reason.contains("checks"));
    for delivery in observed.status.as_ref().unwrap().turn_deliveries.values() {
        let TurnDeliveryOutcome::Queued { blocking_reason, .. } = &delivery.episodes[0].outcome else { panic!("queued") };
        assert_eq!(blocking_reason, "terminal session unavailable");
    }
    case.wake.observe_queued_turns("flotilla", &observed, &BTreeMap::new(), now).await.unwrap();
    assert!(watch.next().now_or_never().is_none(), "unchanged batches must not write");
}

/// A turn to the coder, Message `message_id`, queued at `queued_at` on `rung`.
fn queued_turn(
    rung: TurnDeliveryRung,
    evidence_at: DateTime<Utc>,
    queued_at: DateTime<Utc>,
    message_id: &str,
) -> flotilla_resources::TurnDeliveryStatus {
    flotilla_resources::TurnDeliveryStatus {
        episodes: vec![TurnDeliveryEpisode {
            subject_revision: "head".into(),
            evidence_at,
            judged_claim_at: evidence_at,
            outcome: TurnDeliveryOutcome::Queued {
                rung,
                queued_at,
                vessel: "work".into(),
                role: "coder".into(),
                message_id: message_id.into(),
                blocking_reason: "initial".into(),
            },
        }],
        ..Default::default()
    }
}

/// The work's vessel loses its environment: the work, and so the launch, read Interrupted.
fn interrupt(case: &Case) {
    case.home.engine.vessels.step(&case.vessel, VesselStep::Lost { reason: "environment lost".into() });
}

// #2560: fresh screen redraws cannot keep a silent turn able forever.
// The injected clock crosses the inactivity boundary without sleeps or a live crew.
#[tokio::test]
async fn silent_working_turn_reaches_supervision() {
    let case = actor_nudge_scenario(&[("governor", 1, ConvoyPhase::Active)]).await;
    let (wake, delivery) = (&case.wake, &case.delivery);
    let start = Utc::now();
    // The governor's own session is at work.
    let busy = TerminalAttention {
        state: TerminalAttentionState::Working,
        source: TerminalAttentionSource::Screen,
        as_of: start + chrono::Duration::seconds(900),
    };
    observe_session(&case.home, "governor-1-governor", Some(busy));
    observe_actor(&case, wake, TerminalAttentionState::Working, start).await;
    observe_actor(&case, wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(899)).await;
    assert!(case.status().await.stalled.is_none());
    observe_actor(&case, wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(900)).await;
    let status = case.status().await;
    let stall = status.stalled.expect("silent working turn must stall");
    assert!(stall.evidence.contains("turn inactivity bound"), "{}", stall.evidence);
    assert_eq!(stall.source, StallEvidenceSource::Session);
    assert_eq!(stall.rung, StallRung::Governor);
    assert_eq!(stall.supervisor.expect("supervisor").convoy, case.governor("governor"));
    assert_eq!(
        delivery.requests.lock().expect("deliveries")[0].convoy,
        case.governor("governor"),
        "a busy TUI needs supervision, not a queued nudge"
    );
    // The real watch loop re-arms the row with its supervisor as maker.
    wake.sync_rows("flotilla", &case.objects().await).await.expect("re-arm supervised rows");
    // Repeated Working redraws must not clear a supervised stall.
    observe_actor(&case, wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(901)).await;
    assert!(case.status().await.stalled.is_some());
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    observe_claude_hook(&case, wake, "pre-tool-use", start + chrono::Duration::seconds(902)).await;
    let recovered = case.status().await;
    assert!(recovered.stalled.is_none(), "tool activity recovers the maker: {:?}", recovered.stalled);
}

async fn stalled_at(case: &Case, wake: &ReconcilerWake, now: DateTime<Utc>) -> Option<flotilla_resources::StalledCondition> {
    wake.judge_stalls_at("flotilla", &case.objects().await, now).await.expect("judge turn");
    case.status().await.stalled
}

// #3108: a long turn that keeps calling tools is never bounded by its length; the bound counts
// from its last tool call.
#[tokio::test]
async fn long_turn_with_tool_activity_is_bounded_only_by_its_silence() {
    let case = idle_nudge_scenario().await;
    let wake = &case.wake;
    let start = Utc::now();
    observe_actor_source(&case, wake, TerminalAttentionState::Working, TerminalAttentionSource::Hook, start).await;
    for minute in (5..=30).step_by(5) {
        agents().progress.tool("resumed-coder", start + chrono::Duration::minutes(minute));
        assert!(stalled_at(&case, wake, start + chrono::Duration::minutes(minute) + chrono::Duration::seconds(1)).await.is_none());
    }
    let last_tool = start + chrono::Duration::minutes(30);
    assert!(stalled_at(&case, wake, last_tool + chrono::Duration::seconds(899)).await.is_none(), "45 minutes in, 14:59 silent");
    let stall = stalled_at(&case, wake, last_tool + chrono::Duration::seconds(900)).await.expect("15 silent minutes stall");
    assert!(stall.evidence.contains("turn inactivity bound"), "{}", stall.evidence);
}

// #3108: what the daemon saw of a turn's progress is not stored, so a restarted daemon counts the
// turn's silence from its own start, not from the turn's.
#[tokio::test]
async fn a_restart_starts_the_turn_clock_at_the_restart() {
    let mut case = idle_nudge_scenario().await;
    let start = Utc::now();
    observe_actor_source(&case, &case.wake, TerminalAttentionState::Working, TerminalAttentionSource::Hook, start).await;
    agents().progress.tool("resumed-coder", start + chrono::Duration::minutes(10));
    let restart = start + chrono::Duration::minutes(20);
    // The restarted daemon keeps the subscriptions, with its own readings.
    let running = std::mem::replace(&mut case.wake, supervision_wake(&case.backend, &case.home));
    let wake = ReconcilerWake::new(running.subscriptions.restarted_with(case.home.restarted_reads(restart)));
    assert!(stalled_at(&case, &wake, restart + chrono::Duration::seconds(899)).await.is_none(), "35 minutes in, 14:59 after the restart");
    let stall = stalled_at(&case, &wake, restart + chrono::Duration::seconds(900)).await.expect("15 minutes after the restart");
    assert!(stall.evidence.contains("turn inactivity bound"), "{}", stall.evidence);
}

// Losing observation is not a new turn boundary: stale/Unobservable
// evidence must not reset a hung turn's bound or masquerade as a real hook.
#[tokio::test]
async fn silent_turn_survives_stale_and_unobservable_evidence() {
    for source in [TerminalAttentionSource::Screen, TerminalAttentionSource::Hook] {
        let case = idle_nudge_scenario().await;
        let wake = &case.wake;
        let start = Utc::now();
        observe_actor_source(&case, wake, TerminalAttentionState::Working, source, start).await;
        case.judge(wake, start + chrono::Duration::seconds(120)).await;
        observe_actor_source(&case, wake, TerminalAttentionState::Unobservable, source, start + chrono::Duration::seconds(121)).await;
        observe_actor(&case, wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(900)).await;
        let status = case.status().await;
        assert!(status.stalled.expect("silent turn still stalls").evidence.contains("turn inactivity bound"));
        observe_claude_hook(&case, wake, "pre-tool-use", start + chrono::Duration::seconds(901)).await;
        let stalled = case.status().await.stalled;
        assert!(stalled.is_none(), "new turn activity restores ability ({source:?}): {stalled:?}");
    }
}

// #2654: permanent failures park once, transient failures retain a retry
// deadline; neither repeated ticks nor a reconstructed subscription retries early.
#[test]
fn delivery_failures_are_durable_and_log_once() {
    // A launch never reads Pending (one with no work is refused), so the Pending kind, 0, is gone.
    for kind in 1..5 {
        // Missing observation, unsupported leaf, timed retry (of a missing crew), and missing crew.
        let permanent = kind == 2;
        let (logging, logs) = flotilla_log_testkit::capture(tracing::Level::WARN);
        tracing::dispatcher::with_default(&logging, || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tracing scenario").block_on(async {
                let (backend, home) = engine_backend().await;
                let limit = if kind == 2 || kind == 3 { 0 } else { 3 };
                let mut wake = supervision_wake_with_limit(&backend, &home, limit);
                let convoys = backend.using::<Convoy>("flotilla");
                // The turn's target, the coder, is absent while a reviewer crews the work; the
                // reviewer hands its work to the coder to bring it back.
                let crewed = if kind == 3 || kind == 4 { "reviewer" } else { "coder" };
                let mut status = single_agent(crewed);
                status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew[0].promises =
                    vec![flotilla_resources::TemplatePromise::decision_ledger(crewed)];
                let launch = home.admit("stalled-work", convoy_spec(), status).await;
                let launch = launch.as_str();
                let vessel = home.vessel(launch, "work", "env");
                home.seat(&backend, launch, (&vessel, "work"), (crewed, "crew-session"), CrewWorkPhase::Done).await;
                let phase = convoys.get(launch).await.expect("tracing scenario").status.expect("tracing scenario").phase;
                assert_eq!(phase, ConvoyPhase::Landing);
                let rule = TurnDeliveryRule::builder()
                    .on(flotilla_resources::parse_leaf_template(if kind == 2 || kind == 3 {
                        "$issue.state == closed"
                    } else {
                        "$cr.checks == fail"
                    })
                    .expect("tracing scenario"))
                    .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".into()).role("coder".into()).build())
                    .brief("continue".into())
                    .hold(HoldAct::State)
                    .build();
                let mut leaf = Leaf {
                    address: LeafAddress::ChangeRequest { service: "github.com".into(), scope: "team/repo".into(), number: 1 },
                    field_path: ".checks".into(),
                    operator: LeafOperator::Equal,
                    literal: "fail".into(),
                };
                if kind == 2 || kind == 3 {
                    let records = backend.using::<Issue>("flotilla");
                    let name = flotilla_resources::address::record_name(
                        &flotilla_resources::address::stored_issue_address("github.com", "team/repo", 1, &[]).expect("issue address"),
                    )
                    .expect("record name");
                    let record = records
                        .create(
                            &InputMeta::builder().name(name.clone()).build(),
                            &flotilla_resources::IssueSpec::builder()
                                .service("github.com".into())
                                .scope("team/repo".into())
                                .number(1)
                                .observing_authority("test-host".into())
                                .build(),
                        )
                        .await
                        .expect("tracing scenario");
                    let now = Utc::now();
                    records
                        .update_status(
                            &name,
                            &record.metadata.resource_version,
                            &flotilla_resources::IssueStatus {
                                state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, now),
                                updated_at: flotilla_resources::Observation::known(now, now),
                                title: Default::default(),
                                assignees: Default::default(),
                                labels: Default::default(),
                            },
                        )
                        .await
                        .expect("tracing scenario");
                    leaf.address = LeafAddress::Issue { service: "github.com".into(), scope: "team/repo".into(), number: 1 };
                    leaf.field_path = if permanent { ".unsupported" } else { ".state" }.into();
                    leaf.literal = "closed".into();
                }
                for tick in 0..10 {
                    if tick == 5 {
                        wake = supervision_wake_with_limit(&backend, &home, limit);
                    }
                    let id = uuid::Uuid::new_v4();
                    wake.subscriptions.inner.rows.lock().await.insert(
                        id,
                        LeafSubscriptionRow {
                            id,
                            namespace: "flotilla".into(),
                            leaves: vec![leaf.clone()],
                            watcher: LeafWatcher::TurnDelivery {
                                convoy: launch.into(),
                                source: "checks".into(),
                                rule: Box::new(rule.clone()),
                            },
                            maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
                            freshness_demand: None,
                            created_at: Utc::now(),
                            episode_key: Default::default(),
                        },
                    );
                    wake.subscriptions
                        .fire(id, LeafFire { subscription_id: id, watcher_id: uuid::Uuid::nil(), leaf: leaf.clone(), value: "fail".into() })
                        .await;
                    let status = convoys.get(launch).await.expect("tracing scenario").status.expect("tracing scenario");
                    let failure = status.turn_deliveries["checks"].failure.as_ref().expect("tracing scenario");
                    assert_eq!(failure.kind == flotilla_resources::TurnDeliveryFailureKind::Permanent, permanent);
                    assert_eq!(failure.attempts, 1);
                    assert!(failure.retry_at > Utc::now());
                }
                let retry_at = |at: DateTime<Utc>| {
                    move |status: &mut ConvoyStatus| {
                        let delivery = status.turn_deliveries.get_mut("checks").expect("delivery retry fixture");
                        delivery.failure.as_mut().expect("delivery retry fixture").retry_at = at;
                    }
                };
                if kind == 3 {
                    // No observation or convoy event after the watch starts:
                    // its own deadline must re-evaluate this level-triggered leaf.
                    home.note(&backend, launch, retry_at(Utc::now() + chrono::Duration::milliseconds(30))).await;
                    let row = wake.subscriptions.rows().await.into_iter().next().expect("row");
                    let _ = tokio::time::timeout(Duration::from_millis(150), wake.subscriptions.watch_row_once(row)).await;
                    let status = convoys.get(launch).await.expect("delivery retry fixture").status.expect("delivery retry fixture");
                    assert_eq!(status.turn_deliveries["checks"].failure.as_ref().expect("delivery retry fixture").attempts, 2);
                    return;
                }
                // Advance the durable deadline without a wall-clock sleep.
                home.note(&backend, launch, retry_at(Utc::now() - chrono::Duration::seconds(1))).await;
                let row = wake
                    .subscriptions
                    .rows()
                    .await
                    .into_iter()
                    .find(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "checks"))
                    .expect("tracing scenario");
                wake.subscriptions
                    .fire(
                        row.id,
                        LeafFire { subscription_id: row.id, watcher_id: uuid::Uuid::nil(), leaf: leaf.clone(), value: "fail".into() },
                    )
                    .await;
                let status = convoys.get(launch).await.expect("tracing scenario").status.expect("tracing scenario");
                let failure = status.turn_deliveries["checks"].failure.as_ref().expect("tracing scenario");
                assert_eq!(failure.attempts, if permanent { 1 } else { 2 });
                if !permanent {
                    assert_eq!(failure.retry_at.signed_duration_since(failure.failed_at), chrono::Duration::seconds(10));
                }
                if kind == 4 {
                    // The reviewer hands its work to the coder: the coder is crewed.
                    let convoy = convoys.get(launch).await.expect("delivery retry fixture");
                    home.engine()
                        .reassign(&convoy, ("work", "reviewer"), ("work", "coder"), &[])
                        .await
                        .expect("hand the work to the coder");
                    let status = home.note(&backend, launch, retry_at(Utc::now())).await.status.expect("delivery retry fixture");
                    assert_eq!(status.phase, ConvoyPhase::Landing);
                    assert!(status.crew_work["work"].contains_key("coder"), "the coder is crewed");
                    wake.subscriptions
                        .fire(row.id, LeafFire { subscription_id: row.id, watcher_id: uuid::Uuid::nil(), leaf, value: "fail".into() })
                        .await;
                    let status = convoys.get(launch).await.expect("delivery retry fixture").status.expect("delivery retry fixture");
                    let failure = status.turn_deliveries["checks"].failure.as_ref().expect("delivery retry fixture");
                    assert_eq!(failure.attempts, 3);
                    assert!(
                        !failure.reason.contains("phase") && !failure.reason.contains("crew is absent"),
                        "recovery must get past the old failure"
                    );
                }
            });
        });
        let text = logs.text();
        assert_eq!(text.matches("turn delivery failed").count(), if kind == 4 { 2 } else { 1 }, "{text}");
        assert!(text.contains("source=checks"));
    }
}

#[tokio::test]
async fn checks_settled_turns_keep_flowing_across_many_new_heads() {
    for role in ["coder", "reviewer"] {
        active_checks_scenario(&[false; 12], false, role).await;
    }
}

#[tokio::test]
async fn actionable_review_turn_delivery_enforces_head_identity_and_keeps_delivering_new_heads() {
    assert_turn_delivery_enforces_head_identity_and_records_rungs(
        "review",
        "$cr.review.actionable-at-head == true",
        ".review.actionable-at-head",
        "true",
        true,
        flotilla_resources::ObservedMergeability::Mergeable,
        "Address the actionable review and push the fix.",
    )
    .await;
}

#[tokio::test]
async fn conflicting_mergeability_delivers_once_per_head_past_the_episode_limit() {
    assert_turn_delivery_enforces_head_identity_and_records_rungs(
        "conflicting",
        "$cr.mergeable == conflicting",
        ".mergeable",
        "conflicting",
        false,
        flotilla_resources::ObservedMergeability::Conflicting,
        "Rebase onto the current base branch, resolve additively, run pinned CI, push the same branch, and continue shepherding the PR through review.",
    )
    .await;
}

#[tokio::test]
async fn issue_turn_delivery_fires_once_for_changed_issue() {
    let (backend, home) = engine_backend().await;
    let (event_tx, _) = broadcast::channel(4);
    let refresher = ChangeRequestRefresher::new(
        backend.clone(),
        "authority".into(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = engine_table(backend.clone(), &home, broadcast_test_sink(event_tx), refresher);
    let actuator = Arc::new(RecordingTurnDelivery::default());
    table.set_turn_delivery_actuator(actuator.clone()).await;
    let rule = TurnDeliveryRule::builder()
        .on(flotilla_resources::parse_leaf_template("$issue.state == closed").expect("issue rule"))
        .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".into()).role("coder".into()).build())
        .brief("Respond to the issue change.".into())
        .hold(HoldAct::State)
        .build();
    let reference = flotilla_protocol::IssueRef {
        source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
        id: "2052".into(),
    };
    let spec = ConvoySpec::builder()
        .project_ref("flotilla".into())
        .workflow_ref("workflow".into())
        .repositories(vec![ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".into())
            .repo_ref(RepositoryKey("repo".into()))
            .source_ref("feature/issue".into())
            .target_ref("main".into())
            .workspace_slug("flotilla".into())
            .subpaths(Vec::new())
            .build()])
        .issues(vec![flotilla_resources::ConvoyIssue {
            reference: reference.clone(),
            repository_ref: None,
            snapshot: flotilla_resources::IssueSnapshot {
                title: "Issue".into(),
                body: None,
                state: flotilla_protocol::IssueState::Open,
                labels: vec![],
                as_of: Utc::now(),
            },
        }])
        .build();
    let convoys = backend.using::<Convoy>("flotilla");
    let claim_at = Utc::now() - chrono::Duration::seconds(2);
    // The coder kept the decision ledger it owed, at https://example.com/ledger: the launch is landing.
    let launch = home.admit("issue-turn", spec, ledger_owed()).await;
    let launch = launch.as_str();
    let vessel = home.vessel(launch, "work", "env");
    home.seat(&backend, launch, (&vessel, "work"), ("coder", "issue-turn-coder"), CrewWorkPhase::Working).await;
    submit_ledger(&home, &backend, launch, "https://example.com/ledger", claim_at).await;
    home.keep(&backend, launch, "work", "coder").await;
    assert_eq!(convoys.get(launch).await.expect("convoy").status.expect("status").phase, ConvoyPhase::Landing);
    let leaf = flotilla_resources::address::issue_reference_address(&reference, &[])
        .and_then(|address| flotilla_resources::address::leaf_for(&address))
        .map(|address| Leaf { address, field_path: ".state".into(), operator: LeafOperator::Equal, literal: "closed".into() })
        .expect("issue address");
    let subscription_id = uuid::Uuid::new_v4();
    table.inner.rows.lock().await.insert(
        subscription_id,
        LeafSubscriptionRow {
            id: subscription_id,
            namespace: "flotilla".into(),
            leaves: vec![leaf.clone()],
            watcher: LeafWatcher::TurnDelivery { convoy: launch.into(), source: "issue".into(), rule: Box::new(rule.clone()) },
            maker: LeafMaker::Observed { refresher: "issue".into(), external_party: "forge".into() },
            freshness_demand: Some(claim_at),
            created_at: claim_at,
            episode_key: EpisodeKeyFields::default(),
        },
    );
    let records = backend.using::<Issue>("flotilla");
    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::stored_issue_address("github.com", "flotilla-org/flotilla", 2052, &[]).expect("issue address"),
    )
    .expect("record name");
    let issue = records
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &flotilla_resources::IssueSpec::builder()
                .service("github.com".into())
                .scope("flotilla-org/flotilla".into())
                .number(2052)
                .observing_authority("authority".into())
                .build(),
        )
        .await
        .expect("issue");
    let watched_row = table.inner.rows.lock().await[&subscription_id].clone();
    let watching_table = table.clone();
    let watch = tokio::spawn(async move { watching_table.watch_row(watched_row).await });
    let changed_at = Utc::now();
    records
        .update_status(
            &name,
            &issue.metadata.resource_version,
            &flotilla_resources::IssueStatus {
                title: Default::default(),
                assignees: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, changed_at),
                labels: flotilla_resources::Observation::known(vec!["done".into()], changed_at),
                updated_at: flotilla_resources::Observation::known(changed_at, changed_at),
            },
        )
        .await
        .expect("observation");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if convoys
                .get(launch)
                .await
                .expect("convoy")
                .status
                .as_ref()
                .is_some_and(|status| status.turn_deliveries.get("issue").is_some_and(|delivery| !delivery.episodes.is_empty()))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observed issue change wakes turn delivery");
    table.deliver_turn(subscription_id, launch, "issue", &rule, &leaf).await.expect("duplicate issue event");
    assert_eq!(actuator.requests.lock().expect("requests").len(), 1);
    let subject = super::turn_subject(&actuator.requests.lock().expect("requests")[0]);
    assert_eq!(subject, (flotilla_address::Kind::Issue, "2052".to_owned(), changed_at.to_rfc3339()));
    let brief = actuator.requests.lock().expect("requests")[0].brief.clone();
    assert!(brief.contains("Target crew: `work/coder`"));
    // The kept ledger promise's artifact record, not the reference the coder submitted.
    let ledger = flotilla_resources::address::artifact_slot_record_name(launch, "coder", "decision-ledger", launch).expect("ledger slot");
    assert!(brief.contains(&format!("Decision ledger: {ledger}")), "{brief}");
    assert!(brief.contains("feature/issue"));
    let status = convoys.get(launch).await.expect("convoy").status.expect("status");
    assert_eq!(status.turn_deliveries["issue"].episodes.len(), 1);
    watch.abort();
}

// ADR 0061: a closed, unmerged submission wakes its active owner once, with
// the promise and rejection reason, through the existing delivery collaborator.
#[hegel::test]
fn rejected_promise_arms_owner_turn(tc: hegel::TestCase) {
    // Cover working and yielded owners; rejection must reach each.
    let phase = [CrewWorkPhase::Working, CrewWorkPhase::Interrupted][tc.draw(hegel::generators::integers::<usize>().max_value(1))];
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        use flotilla_resources::convoy::engine::Actor;
        use flotilla_resources::promises::{PromiseKind, PromiseOperation, PromiseSource, Submission};
        use flotilla_resources::{Observation, ObservedChangeRequestState};
        let (backend, home) = engine_backend().await;
        let wake = supervision_wake(&backend, &home);
        let table = &wake.subscriptions;
        let actuator = Arc::new(RecordingTurnDelivery::default());
        table.set_turn_delivery_actuator(actuator.clone()).await;
        let convoys = backend.using::<Convoy>("flotilla");
        let status = ConvoyStatus { workflow_snapshot: Some(stock_snapshot("single-agent")), ..Default::default() };
        let launch = home
            .admit(
                "rejected-promise",
                ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("single-agent".into()).build(),
                status,
            )
            .await;
        let launch = launch.as_str();
        let vessel = home.vessel(launch, "work", "env");
        home.seat(&backend, launch, (&vessel, "work"), ("coder", "rejected-promise-coder"), phase).await;
        let started_at = Utc::now() - chrono::Duration::seconds(5);
        let address = LeafAddress::ChangeRequest { service: "github.com".into(), scope: "flotilla-org/flotilla".into(), number: 42 };
        let submit = PromiseOperation::Submit {
            id: "implementation".into(),
            kind: PromiseKind::Pr,
            source: PromiseSource::Crew,
            submission: Submission {
                reference: address.to_string(),
                metadata: BTreeMap::new(),
                submitted_at: Some(started_at),
                verdict: None,
            },
        };
        let convoy = convoys.get(launch).await.unwrap();
        home.engine().promise(&convoy, "work", "coder", &submit, Actor::Crew).await.expect("the coder submits its PR");
        assert_eq!(convoys.get(launch).await.unwrap().status.unwrap().crew_work["work"]["coder"].phase, phase);
        let requests = backend.using::<ChangeRequest>("flotilla");
        let name = cr_name("github.com", "flotilla-org/flotilla", 42);
        let cr = requests
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("flotilla-org/flotilla".into())
                    .number(42)
                    .observing_authority("host".into())
                    .build(),
            )
            .await
            .unwrap();
        let observed = flotilla_resources::ChangeRequestStatus {
            state: Observation::known(ObservedChangeRequestState::Closed, Utc::now()),
            head_sha: Observation::known("closed-head".into(), Utc::now()),
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            checks: Default::default(),
            mergeable: Default::default(),
            review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
        };
        let observed = requests.update_status(&name, &cr.metadata.resource_version, &observed).await.unwrap();
        let rejected = observe_promise(&home, &backend, launch, &BTreeMap::from([(name.clone(), observed)]), &BTreeMap::new()).await;
        assert!(rejected, "the closed PR rejects the submission");
        let current = convoys.get(launch).await.unwrap();
        wake.sync_rows("flotilla", &HashMap::from([(launch.to_owned(), current)])).await.unwrap();
        let row = table
            .rows()
            .await
            .into_iter()
            .find(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source.starts_with("promise-rejected/")))
            .expect("rejection row");
        let LeafWatcher::TurnDelivery { source, rule, .. } = &row.watcher else { panic!("turn row") };
        // Let the watcher record its episode before replaying the rejection. Aborting
        // it after actuation can leave the fake's request recorded without an episode.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if convoys
                    .get(launch)
                    .await
                    .unwrap()
                    .status
                    .as_ref()
                    .is_some_and(|status| status.turn_deliveries.get(source).is_some_and(|delivery| !delivery.episodes.is_empty()))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("promise rejection wakes its owner and records delivery");
        table.deliver_turn(row.id, launch, source, rule, &row.leaves[0]).await.unwrap();
        table.deliver_turn(row.id, launch, source, rule, &row.leaves[0]).await.unwrap();
        let deliveries = actuator.requests.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].role, "coder");
        assert!(deliveries[0].brief.contains("implementation"));
        assert!(deliveries[0].brief.contains("closed without merging"));
    });
}

// Empty queued evidence may still repair stale queued-turn attention. Across
// every convoy phase, preserve unrelated attention and avoid no-op status writes.
#[hegel::test]
fn empty_queued_batches_preserve_attention_cleanup(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // A launch never reads Pending: one with no work to start is refused.
    let phases = [
        ConvoyPhase::Active,
        ConvoyPhase::Interrupted,
        ConvoyPhase::Landing,
        ConvoyPhase::Landed,
        ConvoyPhase::Failed,
        ConvoyPhase::Abandoned,
    ];
    let phase = phases[tc.draw(gs::integers::<usize>().min_value(0).max_value(phases.len() - 1))];
    let attention_kind = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let (backend, home) = engine_backend().await;
        let wake = supervision_wake(&backend, &home);
        let now = Utc::now();
        let attention = (attention_kind != 0).then(|| flotilla_resources::ConvoyAttention {
            source: if attention_kind == 1 { flotilla_resources::ConvoyAttention::QUEUED_TURN_SOURCE.into() } else { "operator".into() },
            reason: "attention must be reconciled without queued episodes".into(),
            raised_at: now,
        });
        let status = ConvoyStatus { attention: attention.clone(), ..ledger_owed() };
        let launch = launch_at(&backend, &home, "attention", status, phase).await;
        let resolver = backend.using::<Convoy>("flotilla");
        let before = resolver.get(&launch).await.unwrap();
        let mut notes = home.engine().memory().subscribe();
        wake.observe_queued_turns("flotilla", &before, &BTreeMap::new(), now).await.unwrap();
        let after = resolver.get(&launch).await.unwrap();
        let expected = if attention_kind == 1 { None } else { attention };
        assert_eq!(after.status.as_ref().unwrap().attention, expected);
        assert_eq!(!notes_changed(&mut notes), attention_kind != 1);
    });
}

async fn demand_without_placeholder<T: flotilla_resources::Resource>(
    home: &ResourceBackend,
    name: &str,
    demand: impl std::future::Future<Output = Result<(), String>>,
) {
    demand.await.unwrap();
    // Demand writes no local record: the observation reaches the home only from its observer.
    assert!(
        matches!(home.using::<T>("flotilla").get(name).await, Err(flotilla_resources::ResourceError::NotFound { .. })),
        "demand writes no local placeholder"
    );
}

async fn publish_remote_observation<T: flotilla_resources::Resource>(
    home: &ResourceBackend,
    observer: &ResourceBackend,
    name: &str,
    spec: &T::Spec,
    status: &T::Status,
) -> flotilla_resources::ResourceObject<T> {
    let records = observer.using::<T>("flotilla");
    let remote = records.create(&InputMeta::builder().name(name.into()).build(), spec).await.unwrap();
    let observed = records.update_status(name, &remote.metadata.resource_version, status).await.unwrap();
    home.replica_writer::<T>(NodeId::new("observer"), "flotilla").replace(&records.list().await.unwrap(), Utc::now()).await.unwrap();
    observed
}

// For equal observation times, the larger authority wins over local provenance.
// When authority also ties, local wins, independently of copy iteration order.
fn assert_observation_authority_ties<T: flotilla_resources::Resource + Clone>(
    observed: flotilla_resources::ResourceObject<T>,
    set_authority: impl Fn(&mut T::Spec, &str),
) {
    let mut local = observed.clone();
    let mut replica = observed;
    set_authority(&mut local.spec, "authority-a");
    set_authority(&mut replica.spec, "authority-z");
    for local_first in [true, false] {
        let copies = if local_first { [(&local, true), (&replica, false)] } else { [(&replica, false), (&local, true)] };
        let selected = flotilla_resources::pick_replica_read(copies, |(object, local)| (*object, *local)).unwrap();
        assert!(!selected.1, "larger authority wins even over an equally fresh local copy");
    }
    set_authority(&mut local.spec, "authority-z");
    for local_first in [true, false] {
        let copies = if local_first { [(&local, true), (&replica, false)] } else { [(&replica, false), (&local, true)] };
        let selected = flotilla_resources::pick_replica_read(copies, |(object, local)| (*object, *local)).unwrap();
        assert!(selected.1, "local wins only after observation time and authority tie");
    }
}

struct ReplicaTurnSetup<'a> {
    home: &'a ResourceBackend,
    observer: &'a ResourceBackend,
    name: &'a str,
    id: uuid::Uuid,
    /// When set, the observation is older than the demand yet newer than this instant.
    observed_after: Option<DateTime<Utc>>,
}

impl ReplicaTurnSetup<'_> {
    /// Called once the demand is made, so a midpoint is older than it.
    fn observed_at(&self) -> DateTime<Utc> {
        let now = Utc::now();
        self.observed_after.map_or(now, |after| after + (now - after) / 2)
    }

    async fn observe_issue(&self, issues: &IssueRefresher) -> DateTime<Utc> {
        let home = self.home;
        let observer = self.observer;
        let name = self.name;
        let id = self.id;
        let address = flotilla_resources::address::stored_issue_address("github.com", "flotilla-org/flotilla", 3556, &[]).unwrap();
        let (service, scope) = flotilla_resources::address::stored_service_scope(&address).unwrap();
        let key = flotilla_observation::issue_observer::IssueKey::new("flotilla", address).unwrap();
        let spec = flotilla_resources::IssueSpec::builder()
            .service(service)
            .scope(scope)
            .number(key.number())
            .observing_authority("observer".into())
            .build();
        demand_without_placeholder::<Issue>(home, name, issues.demand(id, key, None)).await;
        let observed_at = self.observed_at();
        let observed = publish_remote_observation::<Issue>(
            home,
            observer,
            name,
            &spec,
            &flotilla_resources::IssueStatus {
                title: Default::default(),
                assignees: Default::default(),
                state: Observation::known(flotilla_resources::ObservedIssueState::Closed, observed_at),
                labels: Observation::known(vec![], observed_at),
                updated_at: Observation::known(observed_at, observed_at),
            },
        )
        .await;
        assert_observation_authority_ties(observed, |spec, authority| spec.observing_authority = authority.into());
        observed_at
    }

    async fn observe_change_request(&self, refresher: &ChangeRequestRefresher, merged: bool, failed_checks: bool) -> DateTime<Utc> {
        let home = self.home;
        let observer = self.observer;
        let name = self.name;
        let id = self.id;

        let key = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 3556, &[]).unwrap(),
        )
        .unwrap();
        let (service, scope) = key.stored_service_scope();
        let spec = flotilla_resources::ChangeRequestSpec::builder()
            .service(service)
            .scope(scope)
            .number(key.number())
            .observing_authority("observer".into())
            .build();
        demand_without_placeholder::<ChangeRequest>(home, name, refresher.demand(id, key, None)).await;
        let observed_at = self.observed_at();
        let observed = publish_remote_observation::<ChangeRequest>(
            home,
            observer,
            name,
            &spec,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: Observation::known(
                    if merged { ObservedChangeRequestState::Merged } else { ObservedChangeRequestState::Open },
                    observed_at,
                ),
                head_sha: Observation::known("head".into(), observed_at),
                checks: Observation::known(if failed_checks { ObservedChecks::Fail } else { ObservedChecks::Pass }, observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Observation::known(true, observed_at) },
                mergeable: Observation::known(ObservedMergeability::Mergeable, observed_at),
            },
        )
        .await;
        assert_observation_authority_ties(observed, |spec, authority| spec.observing_authority = authority.into());
        observed_at
    }
}

struct ReplicaTurnSource {
    name: &'static str,
    field_path: &'static str,
    literal: &'static str,
}

impl ReplicaTurnSource {
    const fn new(name: &'static str, field_path: &'static str, literal: &'static str) -> Self {
        Self { name, field_path, literal }
    }

    fn is_issue(&self) -> bool {
        self.name == "issue"
    }

    fn leaf(&self) -> Leaf {
        let address = if self.is_issue() {
            LeafAddress::Issue { service: "github.com".into(), scope: "flotilla-org/flotilla".into(), number: 3556 }
        } else {
            LeafAddress::ChangeRequest { service: "github.com".into(), scope: "flotilla-org/flotilla".into(), number: 3556 }
        };
        Leaf {
            address,
            field_path: self.field_path.into(),
            operator: if self.name == "checks-settled" { LeafOperator::NotEqual } else { LeafOperator::Equal },
            literal: self.literal.into(),
        }
    }

    fn rule(&self, leaf: &Leaf) -> TurnDeliveryRule {
        let comparison = if leaf.operator == LeafOperator::NotEqual { "!=" } else { "==" };
        TurnDeliveryRule::builder()
            .on(flotilla_resources::parse_leaf_template(&format!(
                "${}{} {comparison} {}",
                if self.is_issue() { "issue" } else { "cr" },
                self.field_path,
                self.literal
            ))
            .unwrap())
            .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".into()).role("coder".into()).build())
            .brief("Handle observed change".into())
            .hold(HoldAct::State)
            .build()
    }

    // A delivered message must identify the observed subject revision.
    fn assert_message_subject(&self, subject: (flotilla_address::Kind, String, String), observed_at: DateTime<Utc>) {
        if self.is_issue() {
            assert_eq!(subject, (flotilla_address::Kind::Issue, "3556".to_owned(), observed_at.to_rfc3339()));
        } else {
            assert_eq!(subject, (flotilla_address::Kind::ChangeRequest, "3556".to_owned(), "head".to_owned()));
        }
    }
}

// A demand on the convoy home writes no record there; another host's observation reaches it
// as a replica, and triggering and delivery must agree, including a stalled merge.
#[tokio::test]
async fn replica_status_turn_delivery() {
    use super::super::routing::{ObjectAddress, ObjectKind};
    // Cross both timestamp orders: observed now, or between the launch's judgement and now.
    for observed_since_judgement in [false, true] {
        // Both terminal check outcomes satisfy the stock checks-settled rule.
        for failed_checks in [false, true] {
            for case in [
                ReplicaTurnSource::new("merged", ".state", "merged"),
                ReplicaTurnSource::new("checks-settled", ".checks", "pending"),
                ReplicaTurnSource::new("review", ".review.actionable-at-head", "true"),
                ReplicaTurnSource::new("issue", ".state", "closed"),
            ] {
                let source = case.name;
                let home = ResourceBackend::new(InMemoryBackend::default());
                let observer = ResourceBackend::new(InMemoryBackend::default());
                let (tx, _) = broadcast::channel(4);
                // External forge boundary: provider reads are unavailable; demand and replication are real.
                let refresher =
                    ChangeRequestRefresher::new(home.clone(), "home".into(), Arc::new(UnavailableChangeRequests), Default::default());
                let issues = IssueRefresher::new(home.clone(), "home".into(), Arc::new(UnavailableIssues), Default::default());
                let engine = EngineHome::install(&home, "flotilla", "home");
                let table = engine_table(home.clone(), &engine, broadcast_test_sink(tx), refresher.clone());
                let actuator = Arc::new(RecordingTurnDelivery::default());
                table.set_turn_delivery_actuator(actuator.clone()).await;
                let base = Utc::now() - chrono::Duration::seconds(10);
                let id = uuid::Uuid::new_v4();
                let is_issue = case.is_issue();
                let leaf = case.leaf();
                let name = flotilla_resources::address::leaf_record_name(&leaf.address, &[]).unwrap();
                let rule = case.rule(&leaf);
                let convoys = home.using::<Convoy>("flotilla");
                let merged = source == "merged";
                // A merge reaches a coder stalled at work; the other observations a coder that is done.
                let phase = if merged { ConvoyPhase::Active } else { ConvoyPhase::Landing };
                let launch = launch_at(&home, &engine, "remote-turn", ledger_owed(), phase).await;
                let launch = launch.as_str();
                if merged {
                    let vessel = engine.engine.vessels.request(launch, "work", "home");
                    engine.session(&vessel, "coder", "remote-turn-coder", "env");
                    engine
                        .note(&home, launch, |status| {
                            external_patches::mark_crew_stalled(
                                launch.into(),
                                "work".into(),
                                "coder".into(),
                                base,
                                flotilla_resources::StallReason::Infra,
                                None,
                                "waiting on PR".into(),
                            )
                            .apply(status)
                        })
                        .await;
                    let status = convoys.get(launch).await.unwrap().status.unwrap();
                    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled);
                }
                // A launch starts, and its coder claims, when admitted: evidence older than that is
                // never a turn, so an observation from between the judgement and now still follows it.
                let status = convoys.get(launch).await.unwrap().status.unwrap();
                let judged = status.crew_work["work"]["coder"].finished_at.or(status.started_at).expect("launch started");
                let setup = ReplicaTurnSetup {
                    home: &home,
                    observer: &observer,
                    name: &name,
                    id,
                    observed_after: observed_since_judgement.then_some(judged),
                };
                let (kind, observed_at) = if is_issue {
                    (ObjectKind::Issue, setup.observe_issue(&issues).await)
                } else {
                    (ObjectKind::ChangeRequest, setup.observe_change_request(&refresher, merged, failed_checks).await)
                };
                let row = LeafSubscriptionRow {
                    id,
                    namespace: "flotilla".into(),
                    leaves: vec![leaf.clone()],
                    watcher: LeafWatcher::TurnDelivery { convoy: launch.into(), source: source.into(), rule: Box::new(rule.clone()) },
                    maker: LeafMaker::Observed { refresher: "test".into(), external_party: "forge".into() },
                    freshness_demand: Some(base),
                    created_at: base,
                    episode_key: Default::default(),
                };
                table.inner.rows.lock().await.insert(id, row.clone());
                let subjects = table
                    .load_subjects("flotilla", &std::collections::HashSet::from([ObjectAddress { kind, name: name.clone() }]))
                    .await
                    .unwrap();
                let firing = super::super::subscriptions::evaluate_row(
                    &row,
                    &subjects.borrowed(),
                    &HashMap::from([(leaf.address.clone(), name)]),
                    super::super::sources::LeafObservationStaleness {
                        change_request: Duration::from_secs(60),
                        issue: Duration::from_secs(60),
                    },
                )
                .unwrap()
                .unwrap_or_else(|| panic!("replica triggers {source} rule"));
                table.fire(id, firing.clone()).await;
                assert_eq!(actuator.requests.lock().unwrap().len(), 1, "{source} must deliver from replica status");
                let subject = super::turn_subject(&actuator.requests.lock().unwrap()[0]);
                case.assert_message_subject(subject, observed_at);
                table.fire(id, firing).await;
                assert_eq!(actuator.requests.lock().unwrap().len(), 1, "duplicate {source} is idempotent");
                let status = convoys.get(launch).await.unwrap().status.unwrap();
                assert!(status.turn_deliveries[source].failure.is_none());
                refresher.release(id).await;
                issues.release(id).await;
            }
        }
    }
}

/// The review rounds: an active stock-workflow launch whose coder produced a PR, with its turn
/// rows armed.
struct Rounds {
    backend: ResourceBackend,
    wake: ReconcilerWake,
    actuator: Arc<RecordingTurnDelivery>,
    /// The PR's record.
    name: String,
    launch: String,
    _home: Arc<EngineHome>,
}

/// [`Rounds`] whose coder reads `phase`, and whose workflow also delivers turns on `custom`, a
/// checks predicate, as `custom-pending`.
async fn review_rounds_fixture(phase: CrewWorkPhase, custom: Option<&str>) -> Rounds {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let actuator = Arc::new(RecordingTurnDelivery::default());
    wake.subscriptions.set_turn_delivery_actuator(actuator.clone()).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let mut snapshot = stock_snapshot("single-agent");
    if let Some(predicate) = custom {
        let mut rule = snapshot.turn_delivery["checks-settled"].clone();
        rule.on = flotilla_resources::parse_leaf_template(predicate).expect("custom predicate");
        snapshot.turn_delivery.insert("custom-pending".into(), rule);
    }
    let status = ConvoyStatus { workflow_snapshot: Some(snapshot), ..Default::default() };
    let launch = home
        .admit("rounds", ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("single-agent".into()).build(), status)
        .await;
    let vessel = home.vessel(&launch, "work", "env");
    let pr = flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
        id: "3557".into(),
    };
    let convoy = home.view(&backend, &launch).await;
    home.engine().link(&convoy, &pr, SubjectDiscoverySource::Claim, "crew").await.expect("the crew claims its PR");
    home.seat(&backend, &launch, (&vessel, "work"), ("coder", "rounds-coder"), phase).await;
    let updated = convoys.get(&launch).await.expect("active crew");
    let name = cr_name("github.com", "flotilla-org/flotilla", 3557);
    backend
        .using::<ChangeRequest>("flotilla")
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("flotilla-org/flotilla".into())
                .number(3557)
                .observing_authority("authority".into())
                .build(),
        )
        .await
        .expect("PR record");
    wake.sync_rows("flotilla", &HashMap::from([(launch.clone(), updated)])).await.expect("turn rows");
    // Drive each firing deterministically rather than through background watches.
    for task in wake.subscriptions.inner.tasks.lock().await.drain().map(|(_, task)| task) {
        task.abort();
    }
    Rounds { backend, wake, actuator, name, launch, _home: home }
}

async fn turn_row(wake: &ReconcilerWake, wanted: &str) -> LeafSubscriptionRow {
    wake.subscriptions
        .rows()
        .await
        .into_iter()
        .find(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == wanted))
        .unwrap_or_else(|| panic!("armed `{wanted}` row"))
}

async fn fire_turn_row(wake: &ReconcilerWake, launch: &str, row: &LeafSubscriptionRow) {
    let LeafWatcher::TurnDelivery { source, rule, .. } = &row.watcher else { unreachable!() };
    wake.subscriptions.deliver_turn(row.id, launch, source, rule, &row.leaves[0]).await.expect("turn firing");
}

// #3565 (PR #3557): a crew pushing review fixes is making progress. Four
// rounds, each a new head with a review and settled checks, all reach the
// crew past the episode limit. A fail-to-pass rerun at the last head must
// deliver green once, while duplicate outcomes stay deduplicated.
#[tokio::test]
async fn four_review_rounds_deliver_the_green_checks_on_the_last_round() {
    let Rounds { backend, wake, actuator, name, launch, .. } = review_rounds_fixture(CrewWorkPhase::Working, None).await;
    let launch = launch.as_str();
    let records = backend.using::<ChangeRequest>("flotilla");
    let checks = turn_row(&wake, "checks-settled").await;
    let review = turn_row(&wake, "actionable-review").await;
    let rounds = [("ae5be374", false), ("51844bae", false), ("765a01ce", false), ("a2bc4085", false), ("a2bc4085", true)];
    for (head, green) in rounds {
        let current = records.get(&name).await.expect("record");
        let observed_at = Utc::now();
        let observed = flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: Observation::known(ObservedChangeRequestState::Open, observed_at),
            head_sha: Observation::known(head.to_string(), observed_at),
            checks: Observation::known(if green { ObservedChecks::Pass } else { ObservedChecks::Fail }, observed_at),
            review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Observation::known(!green, observed_at) },
            mergeable: Observation::known(ObservedMergeability::Mergeable, observed_at),
        };
        records.update_status(&name, &current.metadata.resource_version, &observed).await.expect("observe round");
        if !green {
            fire_turn_row(&wake, launch, &review).await;
        }
        fire_turn_row(&wake, launch, &checks).await;
        // A repeat firing at the same head stays one episode.
        fire_turn_row(&wake, launch, &checks).await;
    }
    let requests = actuator.requests.lock().expect("requests").clone();
    let sources = requests.iter().map(|request| (request.source.as_str(), request.subject_revision.as_str())).collect::<Vec<_>>();
    assert_eq!(
        sources,
        [
            ("actionable-review", "ae5be374"),
            ("checks-settled", "ae5be374@fail"),
            ("actionable-review", "51844bae"),
            ("checks-settled", "51844bae@fail"),
            ("actionable-review", "765a01ce"),
            ("checks-settled", "765a01ce@fail"),
            ("actionable-review", "a2bc4085"),
            ("checks-settled", "a2bc4085@fail"),
            ("checks-settled", "a2bc4085@pass"),
        ]
    );
    let last = &requests.last().expect("green turn").brief;
    assert!(last.contains("- Head SHA: `a2bc4085`") && last.contains("- Checks: pass"), "{last}");
    assert_eq!(actuator.holds.load(Ordering::SeqCst), 0);
    let status = backend.using::<Convoy>("flotilla").get(launch).await.expect("convoy").status.expect("status");
    assert_eq!(status.turn_deliveries["checks-settled"].episodes.len(), 5);
    assert!(status.turn_delivery_holds().is_empty());
    assert!(status.attention.is_none());
}

// Custom checks predicates may request Pending too; outcome-aware identity must
// preserve that delivery and deduplicate repeated observations at the same head.
#[hegel::test]
fn custom_checks_rules_deliver_pending_once(tc: hegel::TestCase) {
    // Both admitted predicate forms can match Pending, for either active crew phase.
    let predicate = if tc.draw(hegel::generators::booleans()) { "$cr.checks == pending" } else { "$cr.checks != pass" };
    let phase = if tc.draw(hegel::generators::booleans()) { CrewWorkPhase::Working } else { CrewWorkPhase::Interrupted };
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let Rounds { backend, wake, actuator, name, launch, .. } = review_rounds_fixture(phase, Some(predicate)).await;
        let row = turn_row(&wake, "custom-pending").await;
        let records = backend.using::<ChangeRequest>("flotilla");
        let record = records.get(&name).await.expect("PR");
        let now = Utc::now();
        let observed = flotilla_resources::ChangeRequestStatus {
            head_sha: Observation::known("same-head".into(), now),
            checks: Observation::known(ObservedChecks::Pending, now),
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: Default::default(),
            review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
            mergeable: Default::default(),
        };
        records.update_status(&name, &record.metadata.resource_version, &observed).await.expect("pending checks");
        fire_turn_row(&wake, &launch, &row).await;
        fire_turn_row(&wake, &launch, &row).await;
        let requests = actuator.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1, "{predicate}, {phase:?}");
        assert_eq!(requests[0].subject_revision, "same-head@pending");
        assert!(requests[0].brief.contains("- Checks: pending"));
    });
}

// #3565: the limit still catches a loop that makes no progress. Review-round
// artifacts republished about one head hold the fourth turn; further rounds at
// that head stay held, and a new head clears the hold and delivers.
#[tokio::test]
async fn artifact_rounds_at_one_head_hold_until_the_head_moves() {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let table = &wake.subscriptions;
    let actuator = Arc::new(RecordingTurnDelivery::default());
    table.set_turn_delivery_actuator(actuator.clone()).await;
    let source = "review-round";
    let rule = TurnDeliveryRule::builder()
        .on(flotilla_resources::parse_leaf_template(
            "$artifact(reviewer,review-round,change-request-head).summary.disposition == changes-requested",
        )
        .expect("artifact leaf"))
        .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".into()).role("coder".into()).build())
        .brief("Address the review round.".into())
        .hold(HoldAct::State)
        .build();
    let repo_ref = RepositoryKey("repo".into());
    let spec = ConvoySpec::builder()
        .project_ref("flotilla".into())
        .workflow_ref("workflow".into())
        .repositories(vec![ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".into())
            .repo_ref(repo_ref.clone())
            .source_ref("feature/rounds".into())
            .target_ref("main".into())
            .workspace_slug("flotilla".into())
            .subpaths(Vec::new())
            .build()])
        .change_request(BoundChangeRequest::builder().id("1392".into()).repository_ref(repo_ref).title("rounds".into()).build())
        .build();
    let convoys = backend.using::<Convoy>("flotilla");
    let base = Utc::now();
    // The coder kept what it owed: the launch is landing.
    let launch = home.admit("rounds", spec, ledger_owed()).await;
    let launch = launch.as_str();
    home.vessel(launch, "work", "env");
    home.keep(&backend, launch, "work", "coder").await;
    let status = convoys.get(launch).await.expect("convoy").status.expect("status");
    assert_eq!((status.phase, status.work["work"].phase), (ConvoyPhase::Landing, WorkPhase::Complete));
    let records = backend.using::<ChangeRequest>("flotilla");
    records
        .create(
            &InputMeta::builder().name(cr_name("github.com", "flotilla-org/flotilla", 1392)).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("flotilla-org/flotilla".into())
                .number(1392)
                .observing_authority("authority".into())
                .build(),
        )
        .await
        .expect("change request");
    let held = |status: &ConvoyStatus| (!status.turn_delivery_holds().is_empty(), status.attention.is_some());
    let delivered = || actuator.requests.lock().expect("requests").len();
    let holds = || actuator.holds.load(Ordering::SeqCst);

    let round = |head, round| publish_review_round(&backend, &wake, &rule, (launch, head), round, base);
    observe_review_head(&backend, "aaa").await;
    for index in 1..=3 {
        round("aaa", index).await;
        assert_eq!((delivered(), holds()), (index as usize, 0), "round {index} at an unchanged head is delivered");
    }

    // Round 4: a fourth consecutive turn without a new head is refused and held.
    round("aaa", 4).await;
    assert_eq!((delivered(), holds()), (3, 1));
    let status = convoys.get(launch).await.expect("convoy").status.expect("status");
    let refusal = status.turn_deliveries[source].episodes.last().expect("refusal");
    let TurnDeliveryOutcome::Refused { reason, refused_at, hold_executed: true } = &refusal.outcome else {
        panic!("fourth round at one head is refused: {refusal:?}");
    };
    assert!(reason.contains("at subject revision `aaa`"), "{reason}");
    // The production actuator records the hold state field beside the refusal.
    let hold = ConvoyAttention { source: source.into(), reason: reason.clone(), raised_at: *refused_at };
    home.note(&backend, launch, |status| {
        flotilla_resources::ConvoyStatusPatch::HoldTurnDelivery { source: source.into(), hold }.apply(status)
    })
    .await;

    // Round 5 at the same head stays held, without a second hold.
    round("aaa", 5).await;
    assert_eq!((delivered(), holds()), (3, 1));
    assert_eq!(held(&convoys.get(launch).await.expect("convoy").status.expect("status")), (true, true));

    // Round 6 at a new head is progress: delivered, and the hold is released.
    observe_review_head(&backend, "bbb").await;
    round("bbb", 6).await;
    assert_eq!((delivered(), holds()), (4, 1));
    assert_eq!(held(&convoys.get(launch).await.expect("convoy").status.expect("status")), (false, false));
    assert_eq!(
        actuator.requests.lock().expect("requests").last().expect("new head turn").subject_revision,
        format!("bbb@{}", flotilla_resources::artifact::blob_digest_of(b"round-6"))
    );
}

async fn observe_review_head(backend: &ResourceBackend, head: &str) {
    let records = backend.using::<ChangeRequest>("flotilla");
    let name = cr_name("github.com", "flotilla-org/flotilla", 1392);
    let current = records.get(&name).await.expect("change request");
    let now = Utc::now();
    let observed = flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: Observation::known(ObservedChangeRequestState::Open, now),
        head_sha: Observation::known(head.to_string(), now),
        checks: Observation::known(ObservedChecks::Fail, now),
        review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, now) },
        mergeable: Observation::known(ObservedMergeability::Mergeable, now),
    };
    records.update_status(&name, &current.metadata.resource_version, &observed).await.expect("observe head");
}

/// After the previous round, the reviewer of launch `launch` republishes its review-round
/// artifact about `head`, and its turn row fires.
async fn publish_review_round(
    backend: &ResourceBackend,
    wake: &ReconcilerWake,
    rule: &TurnDeliveryRule,
    (launch, head): (&str, &str),
    round: i64,
    base: DateTime<Utc>,
) {
    let claim_at = base + chrono::Duration::seconds(round * 2);
    let artifacts = backend.using::<Artifact>("flotilla");
    let record_name = flotilla_resources::address::record_name(
        &flotilla_resources::address::artifact_address(launch, "reviewer", "review-round", head).expect("artifact address"),
    )
    .expect("record name");
    let spec = flotilla_resources::ArtifactSpec::builder()
        .convoy(launch.into())
        .producer("reviewer".into())
        .kind("review-round".into())
        .subject(head.into())
        .summary(BTreeMap::from([("disposition".into(), serde_json::json!("changes-requested"))]))
        .digest(flotilla_resources::artifact::blob_digest_of(format!("round-{round}").as_bytes()))
        .size(1)
        .media_type("text/markdown".into())
        .recorded_at(claim_at + chrono::Duration::seconds(1))
        .expires_at(claim_at + chrono::Duration::days(1))
        .build();
    match artifacts.get(&record_name).await {
        Ok(current) => {
            artifacts.update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec).await.expect("republish")
        }
        Err(_) => artifacts.create(&InputMeta::builder().name(record_name).build(), &spec).await.expect("publish"),
    };
    let leaf = Leaf {
        address: LeafAddress::Artifact {
            convoy: launch.into(),
            producer: "reviewer".into(),
            kind: "review-round".into(),
            subject: head.into(),
        },
        field_path: ".summary.disposition".into(),
        operator: LeafOperator::Equal,
        literal: "changes-requested".into(),
    };
    let id = uuid::Uuid::new_v4();
    wake.subscriptions.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf.clone()],
            watcher: LeafWatcher::TurnDelivery { convoy: launch.into(), source: "review-round".into(), rule: Box::new(rule.clone()) },
            maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
            freshness_demand: None,
            created_at: base,
            episode_key: Default::default(),
        },
    );
    wake.subscriptions.deliver_turn(id, launch, "review-round", rule, &leaf).await.expect("artifact turn");
}

// #3455 part 4: failures that arrive while a convoy's failure write is in
// flight join it. They are recorded by one status write, not one write each,
// so a burst of firings cannot exhaust the status patch retry budget.
#[tokio::test]
async fn concurrent_delivery_failures_for_one_convoy_share_one_status_write() {
    // Scope this test's failure warnings, as the log-counting tests do, so the
    // shared callsite never registers without a subscriber.
    concurrent_failures_share_one_write().with_subscriber(flotilla_log_testkit::capture(tracing::Level::WARN).0).await;
}

async fn concurrent_failures_share_one_write() {
    let case = project_supervision_case(&[]).await;
    let table = &case.wake.subscriptions;
    let convoys = case.backend.using::<Convoy>("flotilla");
    let work = case.work.as_str();
    let key = ("flotilla".to_string(), case.work.clone());
    let sources = (0..19).map(|index| format!("merged-unclaimed-{index}")).collect::<Vec<_>>();
    // A firing is already writing this convoy's failures.
    table.inner.pending_failures.lock().expect("pending").entry(key.clone()).or_default().writing = true;
    let mut notes = case.home.engine().memory().subscribe();
    for source in &sources {
        let error = DeliveryError::from(format!("terminal was not ready for {source}"));
        table.record_delivery_failure("flotilla", work, source, &error).await.expect("joins the in-flight write");
    }
    assert!(!notes_changed(&mut notes), "followers do not write");
    // The in-flight writer finishes; the next firing writes everything pending at once.
    table.inner.pending_failures.lock().expect("pending").get_mut(&key).expect("pending entry").writing = false;
    let mut watch = convoys.watch(WatchStart::Now).await.expect("convoy watch");
    let error = DeliveryError::from("terminal was not ready".to_string());
    table.record_delivery_failure("flotilla", work, "checks-settled", &error).await.expect("writer");
    let first = tokio::time::timeout(Duration::from_secs(2), watch.next()).await.expect("one write").expect("watch open").expect("event");
    assert!(tokio::time::timeout(Duration::from_millis(50), watch.next()).await.is_err(), "exactly one status write: {first:?}");
    let status = case.status().await;
    for source in sources.iter().map(String::as_str).chain(["checks-settled"]) {
        let failure = status.turn_deliveries[source].failure.as_ref().expect("recorded failure");
        assert_eq!(failure.attempts, 1, "{source}");
        assert!(failure.reason.starts_with("terminal was not ready"), "{source}");
    }
    assert!(table.inner.pending_failures.lock().expect("pending").is_empty(), "the writer releases its slot");
}

// #3455 part 4: a burst of truly concurrent failures for one convoy all record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_delivery_failure_burst_records_every_source() {
    let case = project_supervision_case(&[]).await;
    let table = case.wake.subscriptions.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(32));
    let tasks = (0..32)
        .map(|index| {
            let table = table.clone();
            let barrier = barrier.clone();
            let work = case.work.clone();
            tokio::spawn(
                async move {
                    barrier.wait().await;
                    let error = DeliveryError::from("terminal was not ready".to_string());
                    table.record_delivery_failure("flotilla", &work, &format!("turn-{index}"), &error).await
                }
                .with_subscriber(flotilla_log_testkit::capture(tracing::Level::WARN).0),
            )
        })
        .collect::<Vec<_>>();
    for task in tasks {
        task.await.expect("task").expect("failure recorded without exhausting the retry budget");
    }
    let status = case.status().await;
    for index in 0..32 {
        assert_eq!(status.turn_deliveries[&format!("turn-{index}")].failure.as_ref().expect("failure").attempts, 1);
    }
    assert!(table.inner.pending_failures.lock().expect("pending").is_empty());
}

// #3455 part 4 (review): a failure write for a convoy that no longer exists
// drops its batch and frees the slot, so nothing stays pending for the
// daemon's life; a later convoy of that name starts clean. A write that fails
// for any other reason requeues through the writer's drop, as a cancelled
// write does (covered below).
#[tokio::test]
async fn failure_write_for_a_missing_convoy_prunes_its_batch() {
    async {
        let case = project_supervision_case(&[]).await;
        let table = &case.wake.subscriptions;
        // The launch is reserved, and not yet admitted.
        let late = case.home.reserve("late").await;
        let error = DeliveryError::from("terminal was not ready".to_string());
        let failed = table.record_delivery_failure("flotilla", &late, "merged-unclaimed", &error).await.expect_err("write fails");
        assert!(failed.contains("not found"), "{failed}");
        assert!(table.inner.pending_failures.lock().expect("pending").is_empty(), "a missing convoy's failures are pruned");
        case.home.admit_reserved(&late, convoy_spec(), ledger_owed()).await;
        table.record_delivery_failure("flotilla", &late, "checks-settled", &error).await.expect("next writer");
        let status = case.home.view(&case.backend, &late).await.status.expect("status");
        assert_eq!(status.turn_deliveries["checks-settled"].failure.as_ref().expect("recorded").attempts, 1);
        assert!(!status.turn_deliveries.contains_key("merged-unclaimed"), "the pruned failure is not resurrected");
        assert!(table.inner.pending_failures.lock().expect("pending").is_empty());
    }
    .with_subscriber(flotilla_log_testkit::capture(tracing::Level::WARN).0)
    .await;
}

// #3455 part 4 (review): a writer cancelled mid-write returns its batch and
// frees the slot; a failure recorded meanwhile keeps precedence for its source.
// A writer that found nothing left releases in the same critical section, so
// its drop cannot free a newer writer's slot.
#[test]
fn failure_writer_requeues_on_cancel_and_never_frees_a_newer_slot() {
    let pending = PendingFailureMap::default();
    let key = ("flotilla".to_string(), "convoy".to_string());
    let error = |reason: &str| DeliveryError::from(reason.to_string());
    let seed = |pending: &PendingFailureMap, source: &str, reason: &str| {
        let mut map = pending.lock().expect("pending");
        let entry = map.entry(key.clone()).or_default();
        entry.failures.insert(source.to_string(), error(reason));
        entry.writing = true;
    };
    seed(&pending, "a", "old a");
    seed(&pending, "b", "old b");
    let mut writer = FailureWriter::new(&pending, key.clone());
    assert_eq!(writer.next_batch().expect("batch").len(), 2);
    // A newer failure for `a` arrives while the batch is in flight; then the firing is cancelled.
    seed(&pending, "a", "new a");
    drop(writer);
    {
        let map = pending.lock().expect("pending");
        let entry = &map[&key];
        assert!(!entry.writing);
        assert_eq!(entry.failures["a"].reason, "new a", "the newer failure keeps precedence");
        assert_eq!(entry.failures["b"].reason, "old b", "the abandoned batch is requeued");
    }
    // A writer that drains to empty releases at once; a newer writer's slot survives its drop.
    pending.lock().expect("pending").get_mut(&key).expect("entry").writing = true;
    let mut writer = FailureWriter::new(&pending, key.clone());
    writer.next_batch().expect("batch");
    assert!(writer.next_batch().is_none());
    seed(&pending, "c", "newer writer");
    drop(writer);
    assert!(pending.lock().expect("pending")[&key].writing, "a newer writer keeps its slot");
}
