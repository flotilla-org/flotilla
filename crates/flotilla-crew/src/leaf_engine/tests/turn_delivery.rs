use super::*;
use flotilla_resources::queued_turn_evidence;
use flotilla_resources::{Observation, ObservedChangeRequestState, ObservedMergeability};
use flotilla_store_memory::InMemoryBackend;
use tracing::instrument::WithSubscriber;

use crate::leaf_engine::turn_delivery::{DeliveryError, FailureWriter, PendingFailureMap};

// #2684: queue acceptance remains queued for every attention state. Only an
// exact terminal receipt confirms submission; age raises advisory attention.
#[hegel::test]
fn queued_turn_waits_for_receipt_and_exposes_bound(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // All attention states, FIFO/receipt positions, missing sessions and ages
    // across the bound; pin the boundary cases too so shrinking cannot hide them.
    let rung = if tc.draw(gs::booleans()) { TurnDeliveryRung::WarmSession } else { TurnDeliveryRung::FreshAgent };
    let state = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let receipt_kind = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(601));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        for age in [-1, 0, 299, 300, 301, age] {
            let (backend, wake, _) = idle_nudge_scenario().await;
            let start = Utc::now();
            let now = start + chrono::Duration::seconds(age);
            let convoys = backend.using::<Convoy>("flotilla");
            let convoy = convoys.get("stalled-work").await.unwrap();
            let mut status = convoy.status.unwrap();
            status.attention = None;
            // The original firing need not remain eligible: health still follows receipts.
            status.phase = ConvoyPhase::Interrupted;
            status.stalled = None;
            status.turn_deliveries.insert(
                "review".into(),
                flotilla_resources::TurnDeliveryStatus {
                    episodes: vec![TurnDeliveryEpisode {
                        subject_revision: "head".into(),
                        evidence_at: start,
                        judged_claim_at: start,
                        outcome: TurnDeliveryOutcome::Queued {
                            rung,
                            queued_at: start,
                            vessel: "work".into(),
                            role: "coder".into(),
                            message_id: "turn".into(),
                            blocking_reason: "initial".into(),
                        },
                        sender: Default::default(),
                    }],
                    ..Default::default()
                },
            );
            // Stored episodes, including their original age, survive daemon restoration.
            let status: ConvoyStatus = serde_json::from_value(serde_json::to_value(status).unwrap()).unwrap();
            convoys.update_status("stalled-work", &convoy.metadata.resource_version, &status).await.unwrap();
            let sessions = backend.using::<TerminalSession>("flotilla");
            let session = sessions.get("resumed-coder").await.unwrap();
            let mut spec = session.spec;
            let TerminalSessionSource::Agent { message, .. } = &mut spec.source else { panic!("agent") };
            let make_message = |id: &str| flotilla_resources::TerminalCrewMessage {
                id: id.into(),
                text: "review wake".into(),
                sender: Default::default(),
                delivery: flotilla_resources::CrewMessageDelivery::Queued,
                following: Vec::new(),
                acknowledged: Default::default(),
            };
            let mut head = make_message("turn");
            head.append(make_message("later"));
            *message = Some(head);
            let session = sessions.update(&InputMeta::from(&session.metadata), &session.metadata.resource_version, &spec).await.unwrap();
            sessions
                .update_status(
                    "resumed-coder",
                    &session.metadata.resource_version,
                    &flotilla_resources::TerminalSessionStatus {
                        phase: TerminalSessionPhase::Running,
                        attention: Some(TerminalAttention {
                            state: [
                                TerminalAttentionState::Unobservable,
                                TerminalAttentionState::Working,
                                TerminalAttentionState::Idle,
                                TerminalAttentionState::NeedsInput,
                            ][state],
                            source: TerminalAttentionSource::Screen,
                            as_of: now,
                        }),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            for repeat in 0..2 {
                let convoy = convoys.get("stalled-work").await.unwrap();
                let prior_version = convoy.metadata.resource_version.clone();
                wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), now).await.unwrap();
                let observed = convoys.get("stalled-work").await.unwrap();
                if repeat == 1 {
                    assert_eq!(observed.metadata.resource_version, prior_version, "unchanged queue evidence must not write every tick");
                }
                let status = observed.status.unwrap();
                assert!(
                    matches!(status.turn_deliveries["review"].episodes[0].outcome, TurnDeliveryOutcome::Queued { .. }),
                    "repeat {repeat}"
                );
                assert_eq!(
                    status.attention.as_ref().map(|attention| attention.source.as_str()),
                    (age > 300).then_some(ConvoyAttention::QUEUED_TURN_SOURCE)
                );
                let TurnDeliveryOutcome::Queued { queued_at, blocking_reason, .. } = &status.turn_deliveries["review"].episodes[0].outcome
                else {
                    panic!("queued")
                };
                assert_eq!(*queued_at, start);
                assert!(blocking_reason.contains("attention"));
            }
            // Terminal settlement can race a submission receipt. It must neither
            // strand the episode nor retain its queued-turn advisory.
            let convoy = convoys.get("stalled-work").await.unwrap();
            let mut status = convoy.status.unwrap();
            let terminal_phase = [ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Abandoned][state % 3];
            status.phase = terminal_phase;
            convoys.update_status("stalled-work", &convoy.metadata.resource_version, &status).await.unwrap();
            let session = sessions.get("resumed-coder").await.unwrap();
            if receipt_kind == 2 {
                let mut spec = session.spec.clone();
                let TerminalSessionSource::Agent { message: Some(message), .. } = &mut spec.source else { panic!("message") };
                message.prune_acknowledged(Some("later"));
                sessions.update(&InputMeta::from(&session.metadata), &session.metadata.resource_version, &spec).await.unwrap();
            } else if receipt_kind == 3 {
                let mut spec = session.spec.clone();
                let TerminalSessionSource::Agent { message, .. } = &mut spec.source else { panic!("agent") };
                *message = None;
                let session =
                    sessions.update(&InputMeta::from(&session.metadata), &session.metadata.resource_version, &spec).await.unwrap();
                let mut status = session.status.unwrap();
                status.delivered_message_id = Some("turn".into());
                sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.unwrap();
            } else {
                let mut status = session.status.unwrap();
                status.delivered_message_id = Some(if receipt_kind == 0 { "turn" } else { "later" }.into());
                sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.unwrap();
            }
            let convoy = convoys.get("stalled-work").await.unwrap();
            wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), now).await.unwrap();
            let status = convoys.get("stalled-work").await.unwrap().status.unwrap();
            assert!(matches!(status.turn_deliveries["review"].episodes[0].outcome, TurnDeliveryOutcome::Delivered {
                rung: delivered_rung, ..
            } if delivered_rung == rung));
            assert!(status.attention.is_none());
            assert_eq!(status.phase, terminal_phase, "receipt observations must preserve the terminal outcome");
            assert_eq!(status.turn_deliveries["review"].episodes.len(), 1);
            assert_eq!(queued_turn_evidence(None, "turn"), (false, "terminal session unavailable".into()));
        }
    });
}

// A health tick applies all queued observations atomically, with one resource
// watch event even when several blockers and the advisory attention change.
#[tokio::test]
async fn queued_turn_observations_write_once_per_convoy() {
    use futures::FutureExt;
    let (backend, wake, _) = idle_nudge_scenario().await;
    let now = Utc::now();
    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.get("stalled-work").await.unwrap();
    let mut status = convoy.status.unwrap();
    status.phase = ConvoyPhase::Interrupted;
    status.stalled = None;
    status.attention = None;
    for source in ["review", "checks"] {
        status.turn_deliveries.insert(
            source.into(),
            flotilla_resources::TurnDeliveryStatus {
                episodes: vec![TurnDeliveryEpisode {
                    subject_revision: "head".into(),
                    evidence_at: now,
                    judged_claim_at: now,
                    outcome: TurnDeliveryOutcome::Queued {
                        rung: TurnDeliveryRung::WarmSession,
                        queued_at: now - chrono::Duration::seconds(301),
                        vessel: "work".into(),
                        role: "coder".into(),
                        message_id: source.into(),
                        blocking_reason: "initial".into(),
                    },
                    sender: Default::default(),
                }],
                ..Default::default()
            },
        );
    }
    let convoy = convoys.update_status("stalled-work", &convoy.metadata.resource_version, &status).await.unwrap();
    let mut watch = convoys.watch(WatchStart::Now).await.unwrap();
    wake.observe_queued_turns("flotilla", &convoy, &BTreeMap::new(), now).await.unwrap();
    assert!(matches!(watch.next().await.unwrap().unwrap(), WatchEvent::Modified(_)));
    assert!(watch.next().now_or_never().is_none(), "one tick must emit only one status update");
    let observed = convoys.get("stalled-work").await.unwrap();
    let attention = observed.status.as_ref().unwrap().attention.as_ref().unwrap();
    assert!(attention.reason.contains("review"));
    assert!(attention.reason.contains("checks"));
    for delivery in observed.status.as_ref().unwrap().turn_deliveries.values() {
        let TurnDeliveryOutcome::Queued { blocking_reason, .. } = &delivery.episodes[0].outcome else { panic!("queued") };
        assert_eq!(blocking_reason, "terminal session unavailable");
    }
    wake.observe_queued_turns("flotilla", &observed, &BTreeMap::new(), now).await.unwrap();
    assert!(watch.next().now_or_never().is_none(), "unchanged batches must not write");
}

// #2560: fresh screen redraws cannot keep a silent turn able forever.
// The injected clock crosses the inactivity boundary without sleeps or a live crew.
#[tokio::test]
async fn silent_working_turn_reaches_supervision() {
    let (backend, wake, delivery) = actor_nudge_scenario(&[("governor", 1, ConvoyPhase::Active)]).await;
    let start = Utc::now();
    let sessions = backend.using::<TerminalSession>("flotilla");
    let governor = sessions
        .create(
            &InputMeta::builder()
                .name("governor-session".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "governor".into()),
                    (VESSEL_LABEL.into(), "govern".into()),
                    (ROLE_LABEL.into(), "governor".into()),
                ]))
                .build(),
            &flotilla_resources::TerminalSessionSpec::builder()
                .env_ref("env".into())
                .role("governor".into())
                .source(TerminalSessionSource::Tool { command: "test".into() })
                .cwd("/".into())
                .pool("cleat".into())
                .build(),
        )
        .await
        .expect("governor session");
    sessions
        .update_status(
            "governor-session",
            &governor.metadata.resource_version,
            &flotilla_resources::TerminalSessionStatus {
                phase: TerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Working,
                    source: TerminalAttentionSource::Screen,
                    as_of: start + chrono::Duration::seconds(900),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("working supervisor");
    observe_actor(&backend, &wake, TerminalAttentionState::Working, start).await;
    observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(899)).await;
    assert!(backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status").stalled.is_none());
    observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(900)).await;
    let status = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status");
    let stall = status.stalled.expect("silent working turn must stall");
    assert!(stall.evidence.contains("turn inactivity bound"), "{}", stall.evidence);
    assert_eq!(stall.source, StallEvidenceSource::Session);
    assert_eq!(stall.rung, StallRung::Governor);
    assert_eq!(stall.supervisor.expect("supervisor").convoy, "governor");
    assert_eq!(delivery.requests.lock().expect("deliveries")[0].convoy, "governor", "a busy TUI needs supervision, not a queued nudge");
    // The real watch loop re-arms the row with its supervisor as maker.
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    wake.sync_rows("flotilla", &HashMap::from([("stalled-work".into(), convoy)])).await.expect("re-arm supervised rows");
    // Repeated Working redraws must not clear a supervised stall.
    observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(901)).await;
    assert!(backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status").stalled.is_some());
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    observe_claude_hook(&backend, &wake, "pre-tool-use", start + chrono::Duration::seconds(902)).await;
    let recovered = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status");
    assert!(recovered.stalled.is_none(), "tool activity recovers the maker: {:?}", recovered.stalled);
}

// Losing observation is not a new turn boundary: stale/Unobservable
// evidence must not reset a hung turn's bound or masquerade as a real hook.
#[tokio::test]
async fn silent_turn_survives_stale_and_unobservable_evidence() {
    for source in [TerminalAttentionSource::Screen, TerminalAttentionSource::Hook] {
        let (backend, wake, _) = idle_nudge_scenario().await;
        let start = Utc::now();
        observe_actor_source(&backend, &wake, TerminalAttentionState::Working, source, start).await;
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), start + chrono::Duration::seconds(120))
            .await
            .expect("stale observation");
        observe_actor_source(&backend, &wake, TerminalAttentionState::Unobservable, source, start + chrono::Duration::seconds(121)).await;
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(900)).await;
        let status = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status");
        assert!(status.stalled.expect("silent turn still stalls").evidence.contains("turn inactivity bound"));
        observe_claude_hook(&backend, &wake, "pre-tool-use", start + chrono::Duration::seconds(901)).await;
        assert!(
            backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status").stalled.is_none(),
            "new turn activity restores ability"
        );
    }
}

// #2654: permanent failures park once, transient failures retain a retry
// deadline; neither repeated ticks nor a reconstructed subscription retries early.
#[test]
fn delivery_failures_are_durable_and_log_once() {
    for kind in 0..5 {
        // Pending convoy, missing observation, unsupported leaf, timed retry, and missing crew.
        let permanent = kind == 2;
        let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = captured_subscriber(logs.clone(), tracing::Level::WARN);
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tracing scenario").block_on(async {
                let (backend, _, _) = project_supervision_case(&[]).await;
                let limit = if kind == 2 || kind == 3 { 0 } else { 3 };
                let mut wake = supervision_wake_with_limit(&backend, limit);
                let convoys = backend.using::<Convoy>("flotilla");
                let convoy = convoys.get("stalled-work").await.expect("tracing scenario");
                let mut status = convoy.status.expect("tracing scenario");
                status.phase = ConvoyPhase::Landing;
                if kind == 0 || kind == 3 {
                    status.phase = ConvoyPhase::Pending;
                }
                status.crew_work.get_mut("work").expect("tracing scenario").get_mut("coder").expect("tracing scenario").finished_at =
                    Some(Utc::now() - chrono::Duration::seconds(2));
                let target_crew = status.crew_work["work"]["coder"].clone();
                if kind == 4 {
                    status.crew_work.get_mut("work").expect("delivery retry fixture").remove("coder");
                }
                convoys.update_status("stalled-work", &convoy.metadata.resource_version, &status).await.expect("tracing scenario");
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
                        wake = supervision_wake_with_limit(&backend, limit);
                    }
                    let id = uuid::Uuid::new_v4();
                    wake.subscriptions.inner.rows.lock().await.insert(
                        id,
                        LeafSubscriptionRow {
                            id,
                            namespace: "flotilla".into(),
                            leaves: vec![leaf.clone()],
                            watcher: LeafWatcher::TurnDelivery {
                                convoy: "stalled-work".into(),
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
                    let status = convoys.get("stalled-work").await.expect("tracing scenario").status.expect("tracing scenario");
                    let failure = status.turn_deliveries["checks"].failure.as_ref().expect("tracing scenario");
                    assert_eq!(failure.kind == flotilla_resources::TurnDeliveryFailureKind::Permanent, permanent);
                    assert_eq!(failure.attempts, 1);
                    assert!(failure.retry_at > Utc::now());
                }
                if kind == 3 {
                    // No observation or convoy event after the watch starts:
                    // its own deadline must re-evaluate this level-triggered leaf.
                    let current = convoys.get("stalled-work").await.expect("convoy");
                    let mut status = current.status.expect("status");
                    status
                        .turn_deliveries
                        .get_mut("checks")
                        .expect("delivery retry fixture")
                        .failure
                        .as_mut()
                        .expect("delivery retry fixture")
                        .retry_at = Utc::now() + chrono::Duration::milliseconds(30);
                    convoys.update_status("stalled-work", &current.metadata.resource_version, &status).await.expect("deadline");
                    let row = wake.subscriptions.rows().await.into_iter().next().expect("row");
                    let _ = tokio::time::timeout(Duration::from_millis(150), wake.subscriptions.watch_row_once(row)).await;
                    let status = convoys.get("stalled-work").await.expect("delivery retry fixture").status.expect("delivery retry fixture");
                    assert_eq!(status.turn_deliveries["checks"].failure.as_ref().expect("delivery retry fixture").attempts, 2);
                    return;
                }
                // Advance the durable deadline without a wall-clock sleep.
                let current = convoys.get("stalled-work").await.expect("tracing scenario");
                let mut status = current.status.expect("tracing scenario");
                status.turn_deliveries.get_mut("checks").expect("tracing scenario").failure.as_mut().expect("tracing scenario").retry_at =
                    Utc::now() - chrono::Duration::seconds(1);
                convoys.update_status("stalled-work", &current.metadata.resource_version, &status).await.expect("tracing scenario");
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
                let status = convoys.get("stalled-work").await.expect("tracing scenario").status.expect("tracing scenario");
                let failure = status.turn_deliveries["checks"].failure.as_ref().expect("tracing scenario");
                assert_eq!(failure.attempts, if permanent { 1 } else { 2 });
                if !permanent {
                    assert_eq!(failure.retry_at.signed_duration_since(failure.failed_at), chrono::Duration::seconds(10));
                }
                if kind == 0 || kind == 4 {
                    let current = convoys.get("stalled-work").await.expect("delivery retry fixture");
                    let mut status = current.status.expect("delivery retry fixture");
                    status.phase = ConvoyPhase::Landing;
                    status.crew_work.get_mut("work").expect("delivery retry fixture").insert("coder".into(), target_crew);
                    status
                        .turn_deliveries
                        .get_mut("checks")
                        .expect("delivery retry fixture")
                        .failure
                        .as_mut()
                        .expect("delivery retry fixture")
                        .retry_at = Utc::now();
                    convoys
                        .update_status("stalled-work", &current.metadata.resource_version, &status)
                        .await
                        .expect("delivery retry fixture");
                    wake.subscriptions
                        .fire(row.id, LeafFire { subscription_id: row.id, watcher_id: uuid::Uuid::nil(), leaf, value: "fail".into() })
                        .await;
                    let status = convoys.get("stalled-work").await.expect("delivery retry fixture").status.expect("delivery retry fixture");
                    let failure = status.turn_deliveries["checks"].failure.as_ref().expect("delivery retry fixture");
                    assert_eq!(failure.attempts, 3);
                    assert!(
                        !failure.reason.contains("phase") && !failure.reason.contains("crew is absent"),
                        "recovery must get past the old failure"
                    );
                }
            });
        });
        let text = String::from_utf8(logs.lock().expect("tracing scenario").clone()).expect("tracing scenario");
        assert_eq!(text.matches("turn delivery failed").count(), if kind == 0 || kind == 4 { 2 } else { 1 }, "{text}");
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
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(4);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".into(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
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
    let created = convoys.create(&InputMeta::builder().name("issue-turn".into()).build(), &spec).await.expect("convoy");
    let claim_at = Utc::now() - chrono::Duration::seconds(2);
    convoys
        .update_status(
            "issue-turn",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([(
                        "coder".into(),
                        CrewWorkState::builder()
                            .phase(CrewWorkPhase::Done)
                            .finished_at(claim_at)
                            .decision_ledger_ref("https://example.com/ledger".into())
                            .build(),
                    )]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("claim");
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
            watcher: LeafWatcher::TurnDelivery { convoy: "issue-turn".into(), source: "issue".into(), rule: Box::new(rule.clone()) },
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
                .get("issue-turn")
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
    table.deliver_turn(subscription_id, "issue-turn", "issue", &rule, &leaf).await.expect("duplicate issue event");
    assert_eq!(actuator.requests.lock().expect("requests").len(), 1);
    let subject = actuator.requests.lock().expect("requests")[0].message_subject.clone().expect("issue subject");
    assert!(matches!(subject, flotilla_resources::MessageReference::Issue { number: 2052, revision, .. }
        if revision == changed_at.to_rfc3339()));
    let brief = actuator.requests.lock().expect("requests")[0].brief.clone();
    assert!(brief.contains("Target crew: `work/coder`"));
    assert!(brief.contains("Decision ledger: https://example.com/ledger"));
    assert!(brief.contains("feature/issue"));
    let status = convoys.get("issue-turn").await.expect("convoy").status.expect("status");
    assert_eq!(status.turn_deliveries["issue"].episodes.len(), 1);
    watch.abort();
}

// ADR 0061: a closed, unmerged submission wakes its active owner once, with
// the promise and rejection reason, through the existing delivery collaborator.
#[hegel::test]
fn rejected_promise_arms_owner_turn(tc: hegel::TestCase) {
    // Cover working, yielded and stalled owners; rejection must reach each.
    let phase = [CrewWorkPhase::Working, CrewWorkPhase::Interrupted, CrewWorkPhase::Stalled]
        [tc.draw(hegel::generators::integers::<usize>().max_value(2))];
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        use flotilla_resources::promises::{PromiseKind, PromiseOperation, PromiseSource, Submission};
        use flotilla_resources::{Observation, ObservedChangeRequestState};
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let wake = supervision_wake(&backend);
        let table = &wake.subscriptions;
        let actuator = Arc::new(RecordingTurnDelivery::default());
        table.set_turn_delivery_actuator(actuator.clone()).await;
        let workflow = flotilla_resources::single_agent_workflow_spec();
        let convoys = backend.using::<Convoy>("flotilla");
        let created = convoys
            .create(
                &InputMeta::builder().name("rejected-promise".into()).build(),
                &ConvoySpec::builder().workflow_ref("single-agent".into()).build(),
            )
            .await
            .unwrap();
        let started_at = Utc::now() - chrono::Duration::seconds(5);
        let address = LeafAddress::ChangeRequest { service: "github.com".into(), scope: "flotilla-org/flotilla".into(), number: 42 };
        let mut status = ConvoyStatus {
            phase: ConvoyPhase::Active,
            started_at: Some(started_at),
            workflow_snapshot: Some(WorkflowSnapshot {
                cascade: None,
                stall_nudges: workflow.stall_nudges,
                supervision: workflow.supervision,
                exit: workflow.exit,
                turn_delivery: workflow.turn_delivery,
                vessels: workflow.vessels,
            }),
            work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Running).build())]),
            crew_work: BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(phase).build())]))]),
            ..Default::default()
        };
        flotilla_resources::promises::apply(
            &mut status,
            "work",
            "coder",
            &PromiseOperation::Submit {
                id: "implementation".into(),
                kind: PromiseKind::Pr,
                source: PromiseSource::Crew,
                submission: Submission {
                    reference: address.to_string(),
                    metadata: BTreeMap::new(),
                    submitted_at: started_at,
                    verdict: None,
                },
            },
        );
        let current = convoys.update_status("rejected-promise", &created.metadata.resource_version, &status).await.unwrap();
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
        let patch = flotilla_resources::promises::next_observation(
            &status,
            "rejected-promise",
            &BTreeMap::from([(name.clone(), observed)]),
            &BTreeMap::new(),
            &[],
            Utc::now(),
            Duration::from_secs(180),
        )
        .unwrap();
        let current = flotilla_store::apply_status_patch(&convoys, &current.metadata.name, &patch).await.unwrap();
        wake.sync_rows("flotilla", &HashMap::from([("rejected-promise".into(), current)])).await.unwrap();
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
                    .get("rejected-promise")
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
        table.deliver_turn(row.id, "rejected-promise", source, rule, &row.leaves[0]).await.unwrap();
        table.deliver_turn(row.id, "rejected-promise", source, rule, &row.leaves[0]).await.unwrap();
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
    let phases = [
        ConvoyPhase::Pending,
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
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let wake = supervision_wake(&backend);
        let now = Utc::now();
        let attention = (attention_kind != 0).then(|| flotilla_resources::ConvoyAttention {
            source: if attention_kind == 1 { flotilla_resources::ConvoyAttention::QUEUED_TURN_SOURCE.into() } else { "operator".into() },
            reason: "attention must be reconciled without queued episodes".into(),
            raised_at: now,
        });
        create_convoy(&backend, "attention", ConvoyStatus { phase, attention: attention.clone(), ..Default::default() }).await;
        let resolver = backend.using::<Convoy>("flotilla");
        let before = resolver.get("attention").await.unwrap();
        wake.observe_queued_turns("flotilla", &before, &BTreeMap::new(), now).await.unwrap();
        let after = resolver.get("attention").await.unwrap();
        let expected = if attention_kind == 1 { None } else { attention };
        assert_eq!(after.status.as_ref().unwrap().attention, expected);
        assert_eq!(after.metadata.resource_version == before.metadata.resource_version, attention_kind != 1);
    });
}

async fn demand_placeholder_spec<T: flotilla_resources::Resource>(
    home: &ResourceBackend,
    name: &str,
    demand: impl std::future::Future<Output = Result<(), String>>,
) -> T::Spec {
    demand.await.unwrap();
    let local = home.using::<T>("flotilla").get(name).await.unwrap();
    // Demand must create a placeholder without inventing observed evidence.
    assert!(local.status.is_none(), "demand creates an empty local placeholder");
    local.spec
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
    observed_at: Option<DateTime<Utc>>,
}

impl ReplicaTurnSetup<'_> {
    async fn observe_issue(&self, issues: &IssueRefresher) -> DateTime<Utc> {
        let home = self.home;
        let observer = self.observer;
        let name = self.name;
        let id = self.id;
        let key = flotilla_observation::issue_observer::IssueKey::new(
            "flotilla",
            flotilla_resources::address::stored_issue_address("github.com", "flotilla-org/flotilla", 3556, &[]).unwrap(),
        )
        .unwrap();
        let mut spec = demand_placeholder_spec::<Issue>(home, name, issues.demand(id, key, None)).await;
        let observed_at = self.observed_at.unwrap_or_else(Utc::now);
        spec.observing_authority = "observer".into();
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
        let mut spec = demand_placeholder_spec::<ChangeRequest>(home, name, refresher.demand(id, key, None)).await;
        let observed_at = self.observed_at.unwrap_or_else(Utc::now);
        spec.observing_authority = "observer".into();
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
    fn assert_message_subject(&self, subject: flotilla_resources::MessageReference, observed_at: DateTime<Utc>) {
        if self.is_issue() {
            assert!(matches!(subject, flotilla_resources::MessageReference::Issue { number: 3556, revision, .. }
                if revision == observed_at.to_rfc3339()));
        } else {
            assert!(matches!(subject, flotilla_resources::MessageReference::ChangeRequest { number: 3556, revision, .. }
                if revision == "head"));
        }
    }
}

// A demanded placeholder on the convoy home must never hide observations from
// another host: triggering and delivery must agree, including a stalled merge.
#[hegel::test]
fn replica_status_turn_delivery(tc: hegel::TestCase) {
    use super::super::routing::{ObjectAddress, ObjectKind};
    // Cross both timestamp orders; a newly created empty copy is not evidence.
    let newer_placeholder = tc.draw(hegel::generators::booleans());
    // Both terminal check outcomes satisfy the stock checks-settled rule.
    let failed_checks = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
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
            let refresher = ChangeRequestRefresher::new(
                "fleet".into(),
                home.clone(),
                "home".into(),
                Arc::new(UnavailableChangeRequests),
                Default::default(),
            );
            let issues = IssueRefresher::new(home.clone(), "home".into(), Arc::new(UnavailableIssues), Default::default());
            let table = LeafSubscriptionTable::new(home.clone(), broadcast_test_sink(tx), refresher.clone());
            let actuator = Arc::new(RecordingTurnDelivery::default());
            table.set_turn_delivery_actuator(actuator.clone()).await;
            let base = Utc::now() - chrono::Duration::seconds(10);
            let id = uuid::Uuid::new_v4();
            let is_issue = case.is_issue();
            let leaf = case.leaf();
            let name = flotilla_resources::address::leaf_record_name(&leaf.address, &[]).unwrap();
            let rule = case.rule(&leaf);
            let convoys = home.using::<Convoy>("flotilla");
            let convoy = convoys
                .create(
                    &InputMeta::builder().name("remote-turn".into()).build(),
                    &ConvoySpec::builder().workflow_ref("workflow".into()).build(),
                )
                .await
                .unwrap();
            let merged = source == "merged";
            convoys
                .update_status(
                    "remote-turn",
                    &convoy.metadata.resource_version,
                    &ConvoyStatus {
                        phase: if merged { ConvoyPhase::Active } else { ConvoyPhase::Landing },
                        crew_work: BTreeMap::from([(
                            "work".into(),
                            BTreeMap::from([(
                                "coder".into(),
                                CrewWorkState::builder()
                                    .phase(if merged { CrewWorkPhase::Stalled } else { CrewWorkPhase::Done })
                                    .maybe_finished_at((!merged).then_some(base))
                                    .build(),
                            )]),
                        )]),
                        started_at: Some(base),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let setup = ReplicaTurnSetup {
                home: &home,
                observer: &observer,
                name: &name,
                id,
                observed_at: newer_placeholder.then_some(base + chrono::Duration::seconds(1)),
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
                watcher: LeafWatcher::TurnDelivery { convoy: "remote-turn".into(), source: source.into(), rule: Box::new(rule.clone()) },
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
                super::super::sources::LeafObservationStaleness { change_request: Duration::from_secs(60), issue: Duration::from_secs(60) },
            )
            .unwrap()
            .unwrap_or_else(|| panic!("replica triggers {source} rule"));
            table.fire(id, firing.clone()).await;
            assert_eq!(actuator.requests.lock().unwrap().len(), 1, "{source} must deliver from replica status");
            let subject = actuator.requests.lock().unwrap()[0].message_subject.clone().unwrap();
            case.assert_message_subject(subject, observed_at);
            table.fire(id, firing).await;
            assert_eq!(actuator.requests.lock().unwrap().len(), 1, "duplicate {source} is idempotent");
            let status = convoys.get("remote-turn").await.unwrap().status.unwrap();
            assert!(status.turn_deliveries[source].failure.is_none());
            refresher.release(id).await;
            issues.release(id).await;
        }
    });
}

/// Active stock-workflow convoy with a produced PR, and its armed turn rows by source.
async fn review_rounds_fixture() -> (ResourceBackend, ReconcilerWake, Arc<RecordingTurnDelivery>, String) {
    use flotilla_protocol::Relationship;
    use flotilla_resources::SubjectDiscoverySource;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let wake = supervision_wake(&backend);
    let actuator = Arc::new(RecordingTurnDelivery::default());
    wake.subscriptions.set_turn_delivery_actuator(actuator.clone()).await;
    let workflow = flotilla_resources::single_agent_workflow_spec();
    let convoys = backend.using::<Convoy>("flotilla");
    let created = convoys
        .create(&InputMeta::builder().name("rounds".into()).build(), &ConvoySpec::builder().workflow_ref("single-agent".into()).build())
        .await
        .expect("convoy");
    let base = Utc::now() - chrono::Duration::seconds(2);
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        started_at: Some(base),
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            stall_nudges: workflow.stall_nudges,
            supervision: workflow.supervision,
            exit: workflow.exit,
            turn_delivery: workflow.turn_delivery,
            vessels: workflow.vessels,
        }),
        work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Running).build())]),
        crew_work: BTreeMap::from([(
            "work".into(),
            BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
        )]),
        ..Default::default()
    };
    status.discover_subject(
        flotilla_protocol::Subject {
            kind: flotilla_protocol::SubjectKind::ChangeRequest,
            source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
            id: "3557".into(),
        },
        Relationship::Produces,
        SubjectDiscoverySource::Claim,
        base,
    );
    let updated = convoys.update_status("rounds", &created.metadata.resource_version, &status).await.expect("active crew");
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
    wake.sync_rows("flotilla", &HashMap::from([("rounds".into(), updated)])).await.expect("turn rows");
    // Drive each firing deterministically rather than through background watches.
    for task in wake.subscriptions.inner.tasks.lock().await.drain().map(|(_, task)| task) {
        task.abort();
    }
    (backend, wake, actuator, name)
}

async fn turn_row(wake: &ReconcilerWake, wanted: &str) -> LeafSubscriptionRow {
    wake.subscriptions
        .rows()
        .await
        .into_iter()
        .find(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == wanted))
        .unwrap_or_else(|| panic!("armed `{wanted}` row"))
}

async fn fire_turn_row(wake: &ReconcilerWake, row: &LeafSubscriptionRow) {
    let LeafWatcher::TurnDelivery { source, rule, .. } = &row.watcher else { unreachable!() };
    wake.subscriptions.deliver_turn(row.id, "rounds", source, rule, &row.leaves[0]).await.expect("turn firing");
}

// #3565 (PR #3557): a crew pushing review fixes is making progress. Four
// rounds, each a new head with a review and settled checks, all reach the
// crew past the episode limit. A fail-to-pass rerun at the last head must
// deliver green once, while duplicate outcomes stay deduplicated.
#[tokio::test]
async fn four_review_rounds_deliver_the_green_checks_on_the_last_round() {
    let (backend, wake, actuator, name) = review_rounds_fixture().await;
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
            fire_turn_row(&wake, &review).await;
        }
        fire_turn_row(&wake, &checks).await;
        // A repeat firing at the same head stays one episode.
        fire_turn_row(&wake, &checks).await;
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
    let status = backend.using::<Convoy>("flotilla").get("rounds").await.expect("convoy").status.expect("status");
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
        let (backend, wake, actuator, name) = review_rounds_fixture().await;
        let convoys = backend.using::<Convoy>("flotilla");
        let current = convoys.get("rounds").await.expect("convoy");
        let mut status = current.status.expect("status");
        let rules = &mut status.workflow_snapshot.as_mut().expect("workflow").turn_delivery;
        let mut rule = rules["checks-settled"].clone();
        rule.on = flotilla_resources::parse_leaf_template(predicate).expect("custom predicate");
        rules.insert("custom-pending".into(), rule);
        status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew").phase = phase;
        let updated = convoys.update_status("rounds", &current.metadata.resource_version, &status).await.expect("custom rule");
        wake.sync_rows("flotilla", &HashMap::from([("rounds".into(), updated)])).await.expect("custom subscription");
        for task in wake.subscriptions.inner.tasks.lock().await.drain().map(|(_, task)| task) {
            task.abort();
        }
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
        fire_turn_row(&wake, &row).await;
        fire_turn_row(&wake, &row).await;
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
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let wake = supervision_wake(&backend);
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
    let created = convoys.create(&InputMeta::builder().name("rounds".into()).build(), &spec).await.expect("convoy");
    let base = Utc::now();
    convoys
        .update_status(
            "rounds",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Complete).build())]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Done).finished_at(base).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("landing status");
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

    observe_review_head(&backend, "aaa").await;
    for round in 1..=3 {
        publish_review_round(&backend, &wake, &rule, "aaa", round, base).await;
        assert_eq!((delivered(), holds()), (round as usize, 0), "round {round} at an unchanged head is delivered");
    }

    // Round 4: a fourth consecutive turn without a new head is refused and held.
    publish_review_round(&backend, &wake, &rule, "aaa", 4, base).await;
    assert_eq!((delivered(), holds()), (3, 1));
    let status = convoys.get("rounds").await.expect("convoy").status.expect("status");
    let refusal = status.turn_deliveries[source].episodes.last().expect("refusal");
    let TurnDeliveryOutcome::Refused { reason, refused_at, hold_executed: true } = &refusal.outcome else {
        panic!("fourth round at one head is refused: {refusal:?}");
    };
    assert!(reason.contains("at subject revision `aaa`"), "{reason}");
    // The production actuator records the hold state field beside the refusal.
    let hold = ConvoyAttention { source: source.into(), reason: reason.clone(), raised_at: *refused_at };
    update_convoy(&backend, "rounds", |status| {
        flotilla_resources::ConvoyStatusPatch::HoldTurnDelivery { source: source.into(), hold }.apply(status)
    })
    .await;

    // Round 5 at the same head stays held, without a second hold.
    publish_review_round(&backend, &wake, &rule, "aaa", 5, base).await;
    assert_eq!((delivered(), holds()), (3, 1));
    assert_eq!(held(&convoys.get("rounds").await.expect("convoy").status.expect("status")), (true, true));

    // Round 6 at a new head is progress: delivered, and the hold is released.
    observe_review_head(&backend, "bbb").await;
    publish_review_round(&backend, &wake, &rule, "bbb", 6, base).await;
    assert_eq!((delivered(), holds()), (4, 1));
    assert_eq!(held(&convoys.get("rounds").await.expect("convoy").status.expect("status")), (false, false));
    assert_eq!(actuator.requests.lock().expect("requests").last().expect("new head turn").subject_revision, "bbb@round-6");
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

/// The crew claims after the previous round; the reviewer then republishes
/// its review-round artifact about `head`, and its turn row fires.
async fn publish_review_round(
    backend: &ResourceBackend,
    wake: &ReconcilerWake,
    rule: &TurnDeliveryRule,
    head: &str,
    round: i64,
    base: DateTime<Utc>,
) {
    let claim_at = base + chrono::Duration::seconds(round * 2);
    update_convoy(backend, "rounds", |status| {
        let coder = status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("coder");
        coder.phase = CrewWorkPhase::Done;
        coder.finished_at = Some(claim_at);
        status.work.get_mut("work").expect("work").phase = WorkPhase::Complete;
        status.phase = ConvoyPhase::Landing;
    })
    .await;
    let artifacts = backend.using::<Artifact>("flotilla");
    let record_name = flotilla_resources::address::record_name(
        &flotilla_resources::address::artifact_address("rounds", "reviewer", "review-round", head).expect("artifact address"),
    )
    .expect("record name");
    let spec = flotilla_resources::ArtifactSpec::builder()
        .convoy("rounds".into())
        .producer("reviewer".into())
        .kind("review-round".into())
        .subject(head.into())
        .summary(BTreeMap::from([("disposition".into(), serde_json::json!("changes-requested"))]))
        .digest(format!("round-{round}"))
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
            convoy: "rounds".into(),
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
            watcher: LeafWatcher::TurnDelivery { convoy: "rounds".into(), source: "review-round".into(), rule: Box::new(rule.clone()) },
            maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
            freshness_demand: None,
            created_at: base,
            episode_key: Default::default(),
        },
    );
    wake.subscriptions.deliver_turn(id, "rounds", "review-round", rule, &leaf).await.expect("artifact turn");
}

// #3455 part 4: failures that arrive while a convoy's failure write is in
// flight join it. They are recorded by one status write, not one write each,
// so a burst of firings cannot exhaust the status patch retry budget.
#[tokio::test]
async fn concurrent_delivery_failures_for_one_convoy_share_one_status_write() {
    // Scope this test's failure warnings, as the log-counting tests do, so the
    // shared callsite never registers without a subscriber.
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    concurrent_failures_share_one_write().with_subscriber(captured_subscriber(logs, tracing::Level::WARN)).await;
}

async fn concurrent_failures_share_one_write() {
    let (backend, wake, _) = project_supervision_case(&[]).await;
    let table = &wake.subscriptions;
    let convoys = backend.using::<Convoy>("flotilla");
    let key = ("flotilla".to_string(), "stalled-work".to_string());
    let sources = (0..19).map(|index| format!("merged-unclaimed-{index}")).collect::<Vec<_>>();
    // A firing is already writing this convoy's failures.
    table.inner.pending_failures.lock().expect("pending").entry(key.clone()).or_default().writing = true;
    let before = convoys.get("stalled-work").await.expect("convoy").metadata.resource_version;
    for source in &sources {
        let error = DeliveryError::from(format!("terminal was not ready for {source}"));
        table.record_delivery_failure("flotilla", "stalled-work", source, &error).await.expect("joins the in-flight write");
    }
    assert_eq!(convoys.get("stalled-work").await.expect("convoy").metadata.resource_version, before, "followers do not write");
    // The in-flight writer finishes; the next firing writes everything pending at once.
    table.inner.pending_failures.lock().expect("pending").get_mut(&key).expect("pending entry").writing = false;
    let mut watch = convoys.watch(WatchStart::Now).await.expect("convoy watch");
    let error = DeliveryError::from("terminal was not ready".to_string());
    table.record_delivery_failure("flotilla", "stalled-work", "checks-settled", &error).await.expect("writer");
    let first = tokio::time::timeout(Duration::from_secs(2), watch.next()).await.expect("one write").expect("watch open").expect("event");
    assert!(tokio::time::timeout(Duration::from_millis(50), watch.next()).await.is_err(), "exactly one status write: {first:?}");
    let status = convoys.get("stalled-work").await.expect("convoy").status.expect("status");
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
    let (backend, wake, _) = project_supervision_case(&[]).await;
    let table = wake.subscriptions.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(32));
    let tasks = (0..32)
        .map(|index| {
            let table = table.clone();
            let barrier = barrier.clone();
            let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
            tokio::spawn(
                async move {
                    barrier.wait().await;
                    let error = DeliveryError::from("terminal was not ready".to_string());
                    table.record_delivery_failure("flotilla", "stalled-work", &format!("turn-{index}"), &error).await
                }
                .with_subscriber(captured_subscriber(logs, tracing::Level::WARN)),
            )
        })
        .collect::<Vec<_>>();
    for task in tasks {
        task.await.expect("task").expect("failure recorded without exhausting the retry budget");
    }
    let status = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status");
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
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    async {
        let (backend, wake, _) = project_supervision_case(&[]).await;
        let table = &wake.subscriptions;
        let error = DeliveryError::from("terminal was not ready".to_string());
        let failed = table.record_delivery_failure("flotilla", "late", "merged-unclaimed", &error).await.expect_err("write fails");
        assert!(failed.contains("not found"), "{failed}");
        assert!(table.inner.pending_failures.lock().expect("pending").is_empty(), "a missing convoy's failures are pruned");
        create_convoy(&backend, "late", ConvoyStatus { phase: ConvoyPhase::Landing, ..Default::default() }).await;
        table.record_delivery_failure("flotilla", "late", "checks-settled", &error).await.expect("next writer");
        let status = backend.using::<Convoy>("flotilla").get("late").await.expect("convoy").status.expect("status");
        assert_eq!(status.turn_deliveries["checks-settled"].failure.as_ref().expect("recorded").attempts, 1);
        assert!(!status.turn_deliveries.contains_key("merged-unclaimed"), "the pruned failure is not resurrected");
        assert!(table.inner.pending_failures.lock().expect("pending").is_empty());
    }
    .with_subscriber(captured_subscriber(logs, tracing::Level::WARN))
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
