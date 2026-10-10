use super::*;

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
                    .on(if kind == 2 || kind == 3 { "$issue.state == closed" } else { "$cr.checks == fail" }
                        .parse()
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
                    let name = flotilla_resources::issue_record_name("github.com", "team/repo", 1);
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
async fn held_source_stays_latched_across_many_new_heads() {
    for role in ["coder", "reviewer"] {
        active_checks_scenario(&[false; 12], false, role).await;
    }
}

#[tokio::test]
async fn actionable_review_turn_delivery_enforces_head_identity_records_rungs_and_escalates() {
    assert_turn_delivery_enforces_head_identity_records_rungs_and_escalates(
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
async fn conflicting_mergeability_delivers_once_per_episode_and_escalates_to_hold() {
    assert_turn_delivery_enforces_head_identity_records_rungs_and_escalates(
        "conflicting",
        "$cr.mergeable == conflicting",
        ".mergeable",
        "conflicting",
        false,
        flotilla_resources::ObservedMergeability::Conflicting,
        "Rebase onto the current base branch, resolve additively, run pinned CI, push the same branch, process review, and file a fresh settlement claim; the previous claim is superseded.",
    )
    .await;
}

#[tokio::test]
async fn issue_turn_delivery_fires_once_for_changed_issue() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(4);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".into(),
        Arc::new(UnavailableChangeRequests),
        crate::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
    let actuator = Arc::new(RecordingTurnDelivery::default());
    table.set_turn_delivery_actuator(actuator.clone()).await;
    let rule = TurnDeliveryRule::builder()
        .on("$issue.state == closed".parse().expect("issue rule"))
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
    let leaf = flotilla_resources::issue_address(&reference)
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
    let name = flotilla_resources::issue_record_name("github.com", "flotilla-org/flotilla", 2052);
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
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
        let name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 42);
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
