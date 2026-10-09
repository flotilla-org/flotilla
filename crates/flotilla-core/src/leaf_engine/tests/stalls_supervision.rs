use super::*;

// #2560: the inactivity bound follows the newest tool/hook evidence, not
// total turn age or refreshed screen timestamps. Generated ages straddle
// the exact boundary, including future evidence and absent tool/hooks.
#[hegel::test]
fn turn_inactivity_tracks_meaningful_evidence(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
    let evidence_age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
    let tool = tc.draw(gs::booleans());
    let hook = tc.draw(gs::booleans());
    let output = tc.draw(gs::booleans());
    let now = Utc::now();
    let start = now - chrono::Duration::seconds(age);
    let evidence = now - chrono::Duration::seconds(evidence_age);
    let obligation = NudgeObligation::builder()
        .maker(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() })
        .leaves(Vec::new())
        .working_since(start)
        .maybe_last_hook_at(hook.then_some(evidence))
        .build();
    let status = flotilla_resources::TerminalSessionStatus {
        last_tool_activity_at: tool.then_some(evidence),
        last_output_activity_at: output.then_some(evidence),
        attention: Some(TerminalAttention { state: TerminalAttentionState::Working, source: TerminalAttentionSource::Screen, as_of: now }),
        ..Default::default()
    };
    let quiet_age = if tool || hook || output { age.min(evidence_age) } else { age };
    assert_eq!(turn_inactivity_reason(&status, &obligation, now).is_some(), quiet_age >= 900);
}

// #2634: a hook-capable crew with no hook after the first-turn grace gets
// advisory attention. Screen activity does not hide it; a real hook clears it.
#[hegel::test]
fn missing_turn_hook_is_visible_and_recovers(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Ages cross the exact grace boundary; both harnesses, existing unrelated
    // attention, and repeated observations exercise preservation and idempotence.
    let age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
    let claude = tc.draw(gs::booleans());
    let other_attention = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        // Pin the boundary with unoccupied attention so generated preservation
        // cases cannot hide an off-by-one regression.
        for (age, other_attention) in [(899, false), (900, false), (901, false), (900, true), (age, other_attention)] {
            let (backend, wake, _) = idle_nudge_scenario().await;
            let start = Utc::now();
            let now = start + chrono::Duration::seconds(age);
            let sessions = backend.using::<TerminalSession>("flotilla");
            let session = sessions.get("resumed-coder").await.expect("session");
            sessions
                .update_status(
                    "resumed-coder",
                    &session.metadata.resource_version,
                    &flotilla_resources::TerminalSessionStatus {
                        phase: TerminalSessionPhase::Running,
                        started_at: Some(start),
                        crew: Some(
                            flotilla_resources::CrewSessionStatus::builder()
                                .id("crew".into())
                                .adapter(if claude { "claude-code" } else { "codex" }.into())
                                .stance("trusted-implicit".into())
                                .build(),
                        ),
                        ..Default::default()
                    },
                )
                .await
                .expect("running session");
            let convoys = backend.using::<Convoy>("flotilla");
            let other = ConvoyAttention { source: "settlement".into(), reason: "keep this".into(), raised_at: start };
            if other_attention {
                flotilla_resources::apply_status_patch(
                    &convoys,
                    "stalled-work",
                    &flotilla_resources::ConvoyStatusPatch::SetSettlementAttention { attention: Some(other.clone()) },
                )
                .await
                .expect("other attention");
            }
            for _ in 0..2 {
                observe_actor(&backend, &wake, TerminalAttentionState::Working, now).await;
                let status = convoys.get("stalled-work").await.expect("convoy").status.expect("status");
                if other_attention {
                    assert_eq!(status.attention, Some(other.clone()));
                } else {
                    assert_eq!(status.attention.as_ref().map(|a| a.source.as_str()), (age >= 900).then_some("missing-turn-hook"));
                    if let Some(attention) = status.attention {
                        assert_eq!(attention.raised_at, now);
                        assert!(attention.reason.contains("no hook"));
                    }
                }
                assert!(status.stalled.is_none(), "missing hooks alone never stall a working crew");
            }
            let hook_at = now + chrono::Duration::seconds(1);
            observe_actor_source(&backend, &wake, TerminalAttentionState::Idle, TerminalAttentionSource::Hook, hook_at).await;
            let status = convoys.get("stalled-work").await.expect("convoy").status.expect("status");
            assert_eq!(status.attention, other_attention.then_some(other));
            assert_eq!(status.nudge_obligations[0].last_hook_at, Some(hook_at));
            observe_actor(&backend, &wake, TerminalAttentionState::Working, hook_at + chrono::Duration::seconds(120)).await;
            assert!(convoys
                .get("stalled-work")
                .await
                .expect("convoy")
                .status
                .expect("status")
                .attention
                .is_none_or(|a| a.source != "missing-turn-hook"));
        }
    });
}

// #2211: typed causes and PR identity select remedies regardless of explanation wording.
#[hegel::test]
fn refusal_remedies_ignore_explanation_wording(tc: hegel::TestCase) {
    use hegel::generators as gs;

    // Cover both remedies, empty/legacy cause lists, duplicates, combined causes,
    // misleading old matcher phrases, and PR numbers across the u64 boundaries.
    let number = tc.draw(gs::integers::<u64>());
    let variant = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let conflict =
        CrewCompletionRefusalCause::ConflictingChangeRequest { service: "github.com".into(), scope: "owner/repo".into(), number };
    let missing =
        CrewCompletionRefusalCause::MissingChangeRequestObservation { service: "forge.example".into(), scope: "other/repo".into(), number };
    let causes = match variant {
        0 => vec![],
        1 => vec![conflict.clone()],
        2 => vec![missing.clone()],
        3 => vec![missing, conflict.clone()],
        _ => vec![conflict.clone(), conflict],
    };
    let expected = match variant {
        0 => "Resolve the unmet expectation".to_string(),
        2 => format!("PR #{number} has no observation yet"),
        _ => format!("PR #{number} is conflicting"),
    };
    for text in ["", "different human explanation", "no federated observation; could not observe PR 42; cr/github.com/owner/repo/42"] {
        let refusal = CrewCompletionRefusal::builder().expectation(text.into()).causes(causes.clone()).consecutive_count(1).build();
        let brief = super::refusal_nudge_brief(&refusal);
        assert!(brief.contains(&expected), "{brief}");
        assert!(brief.contains("flotilla crew complete"));
    }
}

// A real crew stall resolves the supervision topic through an unoccupied
// parent and publishes exactly one intent to the fleet's declared holder.
#[tokio::test]
async fn subscribed_fleet_holder_receives_stall_through_empty_parent() {
    use flotilla_resources::{ConvoyEnsureSpec, ConvoyEnsureStatus, ProjectSpec, RoleDefinition, RoleSubscription};
    let (backend, wake, delivery) = idle_nudge_scenario().await;
    let projects = backend.definitions::<Project>("flotilla");
    for (name, parent) in [("root", None), ("empty-parent", Some("root")), ("wheelhouse", Some("empty-parent"))] {
        projects
            .create(
                &InputMeta::builder().name(name.into()).build(),
                &ProjectSpec::builder()
                    .display_name(name.into())
                    .maybe_parent(parent.map(str::to_string))
                    .role_definitions(if name == "root" {
                        BTreeMap::from([(
                            "guide".into(),
                            RoleDefinition {
                                subscriptions: Some(vec![RoleSubscription::builder().topic("supervision".into()).subtree(true).build()]),
                                ..Default::default()
                            },
                        )])
                    } else {
                        BTreeMap::new()
                    })
                    .build(),
            )
            .await
            .expect("project");
    }
    let convoys = backend.using::<Convoy>("flotilla");
    let source = convoys.get("stalled-work").await.expect("source");
    let mut status = source.status.expect("source status");
    status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("coder").phase = CrewWorkPhase::Stalled;
    convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("stall");
    let target = convoys
        .create(
            &InputMeta::builder().name("fleet-guide".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("root".into()).role("guide".into()).build(),
        )
        .await
        .expect("target");
    convoys
        .update_status(
            "fleet-guide",
            &target.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("guide".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("target status");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures
        .create(
            &InputMeta::builder().name("root-guide".into()).build(),
            &ConvoyEnsureSpec::builder().project_ref("root".into()).role("guide".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("presence");
    let ensure = ensures.get(&ensure.metadata.name).await.expect("local ensure");
    ensures
        .update_status(
            "root-guide",
            &ensure.metadata.resource_version,
            &ConvoyEnsureStatus { convoy_ref: Some("fleet-guide".into()), ..Default::default() },
        )
        .await
        .expect("holder");
    let terminals = backend.using::<TerminalSession>("flotilla");
    let mut spec = terminals.get("resumed-coder").await.expect("source terminal").spec;
    spec.role = "guide".into();
    if let TerminalSessionSource::Agent { context, .. } = &mut spec.source {
        context.convoy = "fleet-guide".into();
    }
    terminals
        .create(
            &InputMeta::builder()
                .name("guide-terminal".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "fleet-guide".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "guide".into()),
                ]))
                .build(),
            &spec,
        )
        .await
        .expect("guide terminal");
    let source = convoys.get("stalled-work").await.expect("source");
    let snapshot = HashMap::from([("stalled-work".into(), source)]);
    wake.judge_stalls_at("flotilla", &snapshot, Utc::now()).await.expect("route stall");
    let requests = delivery.requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].convoy, "fleet-guide");
    assert_eq!(requests[0].role, "guide");
}

// #2211: both typed remedies survive durable status restoration and reach an
// idle crew even when the explanation omits all former matcher phrases.
#[tokio::test]
async fn stored_refusal_nudges_use_typed_causes_after_restore() {
    for (cause, remedy) in [
        (
            CrewCompletionRefusalCause::ConflictingChangeRequest { service: "github.com".into(), scope: "owner/repo".into(), number: 42 },
            "PR #42 is conflicting",
        ),
        (
            CrewCompletionRefusalCause::MissingChangeRequestObservation {
                service: "forge.example".into(),
                scope: "other/repo".into(),
                number: 73,
            },
            "PR #73 has no observation yet",
        ),
    ] {
        let (backend, wake, delivery) = idle_nudge_scenario().await;
        let convoys = backend.using::<Convoy>("flotilla");
        let convoy = convoys.get("stalled-work").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").completion_refusal = Some(
            CrewCompletionRefusal::builder()
                .expectation("Presentation wording changed".into())
                .causes(vec![cause])
                .consecutive_count(1)
                .build(),
        );
        let stored = serde_json::to_vec(&status).expect("store status");
        let restored = serde_json::from_slice(&stored).expect("restore status");
        convoys.update_status("stalled-work", &convoy.metadata.resource_version, &restored).await.expect("persist refusal");
        let start = Utc::now();
        for second in [0, 60, 120, 180] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        let requests = delivery.requests.lock().expect("deliveries");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].brief.contains(remedy), "{}", requests[0].brief);
        assert!(requests[0].brief.contains("Presentation wording changed"));
    }
}

#[tokio::test]
async fn claude_stop_flickers_and_nudge_reply_do_not_nudge_active_work() {
    let (backend, wake, delivery) = idle_nudge_scenario().await;
    let start = Utc::now();
    for second in (0..900).step_by(30) {
        observe_claude_hook(&backend, &wake, "pre-tool-use", start + chrono::Duration::seconds(second)).await;
        observe_claude_hook(&backend, &wake, "post-tool-use", start + chrono::Duration::seconds(second + 1)).await;
        observe_claude_hook(&backend, &wake, "stop", start + chrono::Duration::seconds(second + 2)).await;
    }
    assert!(delivery.requests.lock().expect("deliveries").is_empty(), "per-response Stop is not sustained idle");
    // Finally rest long enough to receive a nudge, then reply to it.
    for second in [900, 960, 1020, 1080] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    observe_claude_hook(&backend, &wake, "user-prompt-submit", start + chrono::Duration::seconds(1081)).await;
    observe_claude_hook(&backend, &wake, "stop", start + chrono::Duration::seconds(1082)).await;
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "reply Stop must not rearm the nudge budget");
}

// A declared stall asks for changed evidence; a working crew retains the
// ordinary settlement obligation. Exercise the real nudge producer at idle.
#[tokio::test]
async fn declared_stall_nudges_preserve_the_changed_evidence_guidance() {
    for phase in [CrewWorkPhase::Working, CrewWorkPhase::Stalled] {
        let (backend, wake, delivery) = idle_nudge_scenario().await;
        let convoys = backend.using::<Convoy>("flotilla");
        let convoy = convoys.get("stalled-work").await.expect("nudge authority");
        let mut status = convoy.status.expect("crew state");
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("actor").phase = phase;
        convoys.update_status("stalled-work", &convoy.metadata.resource_version, &status).await.expect("declared crew phase");
        let start = Utc::now();
        for second in [0, 60, 120, 180] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        let requests = delivery.requests.lock().expect("durable nudge admissions");
        let request = requests.first().expect("settlement nudge");
        assert_eq!(request.brief.contains("Your stall is recorded. What changed since your report?"), phase == CrewWorkPhase::Stalled);
        if phase == CrewWorkPhase::Stalled {
            assert!(request.brief.contains("update the stall with new evidence"));
            assert!(request.brief.contains("ask your supervisor to resume you"));
        }
    }
}

#[tokio::test]
async fn nudge_reply_preserves_idle_clock_and_allows_second_nudge_after_backoff() {
    let (backend, wake, delivery) = idle_nudge_scenario().await;
    let start = Utc::now();
    for second in [0, 60, 120, 180] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    observe_claude_hook(&backend, &wake, "user-prompt-submit", start + chrono::Duration::seconds(181)).await;
    for second in [240, 300] {
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(second)).await;
    }
    observe_claude_hook(&backend, &wake, "stop", start + chrono::Duration::seconds(359)).await;
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").nudge_obligations[0].quiet_since, Some(start), "echo does not restart the idle clock");
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(360)).await;
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2, "continued idle gets the second backed-off nudge");
}

#[tokio::test]
async fn actual_tool_work_after_nudge_cancels_idle_without_replenishing_budget() {
    let (backend, wake, delivery) = idle_nudge_scenario().await;
    let start = Utc::now();
    for second in [0, 60, 120, 180] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    observe_claude_hook(&backend, &wake, "user-prompt-submit", start + chrono::Duration::seconds(181)).await;
    observe_claude_hook(&backend, &wake, "pre-tool-use", start + chrono::Duration::seconds(182)).await;
    for second in [240, 300] {
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(second)).await;
    }
    observe_claude_hook(&backend, &wake, "post-tool-use", start + chrono::Duration::seconds(358)).await;
    observe_claude_hook(&backend, &wake, "stop", start + chrono::Duration::seconds(359)).await;
    observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(360)).await;
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "real work requires a new continuous idle grace");
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").nudge_obligations[0].quiet_since, Some(start + chrono::Duration::seconds(359)));
    for second in [420, 480, 539] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2);
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").nudge_obligations[0].history.len(), 2, "commands alone do not reset the obligation budget");
}

#[tokio::test]
async fn genuine_idle_has_obligation_budget_backoff_and_operator_escalation() {
    let (backend, wake, delivery) = idle_nudge_scenario().await;
    let start = Utc::now();
    for second in [0, 60, 120, 179] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert!(delivery.requests.lock().expect("deliveries").is_empty());
    observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(180)).await;
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    // #2592: even mechanical nudges identify the crew and convoy they concern.
    assert!(delivery.requests.lock().expect("deliveries")[0]
        .brief
        .contains("coder@work in graphql-budget@wheelhouse (resource ref: stalled-work)"));
    // A brief working flicker cannot replenish the unmet claim's budget.
    observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(181)).await;
    for second in [182, 240, 300, 359] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    for second in [362, 420, 480, 540, 600, 659] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2);
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").stalled.expect("stall").rung, StallRung::Nudge);
    observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(722)).await;
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").stalled.expect("stall").rung, StallRung::Operator);
    observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(723)).await;
    for second in [724, 780, 840, 900, 960, 1020] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2, "working does not reset an obligation budget");
}

#[tokio::test]
async fn operator_message_echo_preserves_clock_without_replenishing_budget() {
    let (backend, wake, delivery) = idle_nudge_scenario().await;
    let start = Utc::now();
    for second in [0, 60, 120, 180] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    let sessions = backend.using::<TerminalSession>("flotilla");
    let session = sessions.get("resumed-coder").await.expect("session");
    let mut spec = session.spec.clone();
    let TerminalSessionSource::Agent { message, .. } = &mut spec.source else { panic!("agent") };
    *message = Some(flotilla_resources::TerminalCrewMessage {
        id: "owner-guidance".into(),
        text: "Keep working on the fix".into(),
        sender: flotilla_resources::CrewMessageSender::OperatorResume { principal: None },
        delivery: flotilla_resources::CrewMessageDelivery::Queued,
        acknowledged: Default::default(),
        following: Vec::new(),
    });
    sessions.update(&InputMeta::from(&session.metadata), &session.metadata.resource_version, &spec).await.expect("owner guidance");
    for second in [240, 300, 360, 420] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "pending owner guidance is never displaced");
    let session = sessions.get("resumed-coder").await.expect("session");
    let mut status = session.status.expect("status");
    status.delivered_message_id = Some("owner-guidance".into());
    sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("deliver guidance");
    // Judge delivery without replacing its receipt with the scenario helper.
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), start + chrono::Duration::seconds(421))
        .await
        .expect("delivery grace");
    for second in [480, 540, 600] {
        let session = sessions.get("resumed-coder").await.expect("session");
        let mut status = session.status.expect("status");
        status.attention = Some(TerminalAttention {
            state: TerminalAttentionState::Idle,
            source: TerminalAttentionSource::Hook,
            as_of: start + chrono::Duration::seconds(second),
        });
        sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("response Stop");
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), start + chrono::Duration::seconds(second))
            .await
            .expect("response grace");
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2, "continued idle can receive the second nudge after owner reply");
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").nudge_obligations[0].history.len(), 2);
}

#[tokio::test]
async fn observed_commit_resets_budget_but_refresh_does_not() {
    let (backend, wake, delivery) = idle_nudge_scenario().await;
    let checkouts = backend.using::<Checkout>("flotilla");
    let checkout = checkouts
        .create(
            &InputMeta::builder()
                .name("actor-checkout".into())
                .labels(BTreeMap::from([(CONVOY_LABEL.into(), "stalled-work".into())]))
                .build(),
            &CheckoutSpec::Observed(
                flotilla_resources::ObservedCheckoutSpec::builder()
                    .r#ref("work".into())
                    .path("/workspace".into())
                    .repo_ref(flotilla_protocol::RepositoryKey("repo".into()))
                    .host_ref("host".into())
                    .is_main(false)
                    .build(),
            ),
        )
        .await
        .expect("checkout");
    let mut checkout_status = flotilla_resources::CheckoutStatus::default();
    checkout_status.integration.head_revision = Some("before".into());
    checkouts.update_status("actor-checkout", &checkout.metadata.resource_version, &checkout_status).await.expect("initial HEAD");
    let vessels = backend.using::<Vessel>("flotilla");
    let vessel = vessels
        .create(
            &InputMeta::builder()
                .name("actor-vessel".into())
                .labels(BTreeMap::from([(CONVOY_LABEL.into(), "stalled-work".into())]))
                .build(),
            &flotilla_resources::VesselSpec {
                convoy_ref: "stalled-work".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    vessels
        .update_status(
            "actor-vessel",
            &vessel.metadata.resource_version,
            &flotilla_resources::VesselStatus {
                checkout_refs: BTreeMap::from([(flotilla_protocol::RepositoryKey("repo".into()), "actor-checkout".into())]),
                ..Default::default()
            },
        )
        .await
        .expect("checkout association");
    let start = Utc::now();
    for second in [0, 60, 120, 180] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    let checkout = checkouts.get("actor-checkout").await.expect("checkout");
    checkouts.update_status("actor-checkout", &checkout.metadata.resource_version, &checkout_status).await.expect("same HEAD refresh");
    observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(240)).await;
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").nudge_obligations[0].history.len(), 1);
    let checkout = checkouts.get("actor-checkout").await.expect("checkout");
    checkout_status.integration.head_revision = Some("after".into());
    checkouts.update_status("actor-checkout", &checkout.metadata.resource_version, &checkout_status).await.expect("new commit");
    for second in [241, 300, 360, 420, 421] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    let requests = delivery.requests.lock().expect("deliveries");
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.source == "stall-nudge-1"), "new commit resets the obligation budget");
}

#[tokio::test]
async fn resumed_stalled_crew_is_not_nudged_until_its_briefed_turn_ends() {
    let (backend, wake, delivery) = project_supervision_case(&[]).await;
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let resumed_at = Utc::now();
    flotilla_resources::apply_status_patch(
        &convoys,
        "stalled-work",
        &flotilla_resources::external_patches::resume_crew_work(
            "work".into(),
            "coder".into(),
            resumed_at,
            "Continue with the operator's guidance".into(),
            Some("resume-brief".into()),
        ),
    )
    .await
    .expect("resume declared stall");
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("resumed-coder".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "stalled-work".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &flotilla_resources::TerminalSessionSpec {
                env_ref: "env".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("coding"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".into(),
                        content: "Initial".into(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".into(),
                        convoy: "stalled-work".into(),
                        vessel_ref: "work".into(),
                    }),
                    message: Some(flotilla_resources::TerminalCrewMessage {
                        id: "resume-brief".into(),
                        text: "Continue with the operator's guidance".into(),
                        sender: flotilla_resources::CrewMessageSender::OperatorResume { principal: None },
                        delivery: flotilla_resources::CrewMessageDelivery::Queued,
                        acknowledged: Default::default(),
                        following: Vec::new(),
                    }),
                },
                cwd: "/workspace".into(),
                env: Default::default(),
                pool: "cleat".into(),
            },
        )
        .await
        .expect("crew session");
    let idle_before_delivery = TerminalAttention {
        state: TerminalAttentionState::Idle,
        as_of: Utc::now(),
        source: flotilla_resources::TerminalAttentionSource::Hook,
    };
    sessions
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &flotilla_resources::TerminalSessionStatus {
                phase: TerminalSessionPhase::Running,
                attention: Some(idle_before_delivery),
                ..Default::default()
            },
        )
        .await
        .expect("idle before brief delivery");
    let convoy = convoys.get("stalled-work").await.expect("resumed convoy");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), convoy)])).await.expect("judge before delivery");
    assert!(delivery.requests.lock().expect("nudge requests").is_empty());
    let session = sessions.get("resumed-coder").await.expect("session");
    let mut status = session.status.expect("status");
    status.delivered_message_id = Some("resume-brief".into());
    status.attention = None;
    sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("brief delivered");
    let convoy = convoys.get("stalled-work").await.expect("resumed convoy");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), convoy)])).await.expect("judge during turn");
    assert!(delivery.requests.lock().expect("nudge requests").is_empty());
    let session = sessions.get("resumed-coder").await.expect("session");
    let mut status = session.status.expect("status");
    status.attention = Some(TerminalAttention {
        state: TerminalAttentionState::Idle,
        as_of: Utc::now(),
        source: flotilla_resources::TerminalAttentionSource::Hook,
    });
    sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("turn ended");
    let convoy = convoys.get("stalled-work").await.expect("resumed convoy");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), convoy)])).await.expect("judge after turn");
    assert!(delivery.requests.lock().expect("nudge requests").is_empty());
    let quiet_at = Utc::now() + chrono::Duration::minutes(3);
    // Fresh screen confirmations preserve quiet duration; another Stop
    // would instead restart it because a response just happened.
    for second in [120, 60, 0] {
        let observed_at = quiet_at - chrono::Duration::seconds(second);
        let session = sessions.get("resumed-coder").await.expect("session");
        let mut status = session.status.expect("status");
        status.attention =
            Some(TerminalAttention { state: TerminalAttentionState::Idle, as_of: observed_at, source: TerminalAttentionSource::Screen });
        sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("quiet confirmation");
        let convoy = convoys.get("stalled-work").await.expect("convoy");
        wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), observed_at).await.expect("judge quiet turn");
    }
    assert_eq!(delivery.requests.lock().expect("nudge requests").len(), 1);
}

#[tokio::test]
async fn unavailable_governor_recovers_in_one_pass() {
    for unavailable in GovernorUnavailable::ALL {
        unavailable_governor_scenario(unavailable, &[false, true], FallbackRecord::Current).await;
        unavailable_governor_scenario(unavailable, &[true], FallbackRecord::LegacyExhausted).await;
    }
}

// A governor retry after an explicit escalation must not revisit the Bosun
// rung that the supervisor already consumed, even across a restart.
#[tokio::test]
async fn unavailable_governor_preserves_consumed_bosun_rung() {
    let (backend, mut wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let source = convoys.get("stalled-work").await.expect("source");
    let mut status = source.status.expect("status");
    status.crew_work.get_mut("work").expect("crew").insert("bosun".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build());
    convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("bosun available");
    let now = Utc::now();
    let objects = convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
    wake.sync_rows("flotilla", &objects).await.expect("rows");
    wake.judge_stalls_at("flotilla", &objects, now).await.expect("route to bosun");
    let source = convoys.get("stalled-work").await.expect("source");
    let mut status = source.status.expect("status");
    let condition = status.stalled.as_mut().expect("stall");
    assert_eq!(condition.rung, StallRung::Bosun);
    // Same persisted transition as crew supervise ... escalate: clear ownership,
    // preserve the consumed index, and restore the original actor as maker.
    condition.supervisor = None;
    condition.maker = Some(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() });
    condition.rung = StallRung::Operator;
    convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("escalate");
    delivery.unavailable.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        wake = supervision_wake(&backend);
        wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
        let objects =
            convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
        wake.sync_rows("flotilla", &objects).await.expect("rebuild rows");
        wake.judge_stalls_at("flotilla", &objects, now).await.expect("retry governor");
        let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stalled.rung, StallRung::Operator);
        assert_eq!(stalled.supervision_index, Some(0));
        assert!(!stalled.supervision_exhausted);
    }
    delivery.unavailable.store(false, Ordering::SeqCst);
    let objects = convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
    wake.sync_rows("flotilla", &objects).await.expect("rows");
    wake.judge_stalls_at("flotilla", &objects, now).await.expect("recover governor");
    let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
    assert_eq!(stalled.rung, StallRung::Governor);
    assert_eq!(stalled.supervision_index, Some(1));
    let requests = delivery.requests.lock().expect("deliveries");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].role, "bosun");
    assert_eq!(requests[1].role, "governor");
}

// Explicit escalation by the last governor consumes the whole policy.
// Legacy cursor recovery must not send the stall back to that governor.
#[tokio::test]
async fn governor_escalation_exhausts_default_policy_once() {
    let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let source = convoys.get("stalled-work").await.expect("source");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("route governor");
    let source = convoys.get("stalled-work").await.expect("source");
    let mut status = source.status.expect("status");
    let condition = status.stalled.as_mut().expect("stall");
    condition.supervisor = None;
    condition.maker = Some(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() });
    condition.rung = StallRung::Operator;
    convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("governor escalates");
    for _ in 0..2 {
        let source = convoys.get("stalled-work").await.expect("source");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("consume policy");
        let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stalled.rung, StallRung::Operator);
        assert!(stalled.supervision_exhausted);
        assert_eq!(stalled.supervision_index, Some(2));
        assert!(stalled.supervisor.is_none());
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    }
}

// WARN events must expose routing failures as structured convoy/target/reason
// fields; skipped lower candidates must not warn when a governor accepts delivery.
#[test]
fn unavailable_governor_warns_with_routing_fields() {
    #[derive(Clone)]
    struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("logs").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for unavailable in GovernorUnavailable::ALL {
        let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(unavailable_governor_scenario(unavailable, &[false, true], FallbackRecord::Current));
        });
        let text = String::from_utf8(logs.lock().expect("logs").clone()).expect("utf8 logs");
        let warnings = text.lines().collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1, "unchanged unavailable routing warns once: {text}");
        for warning in warnings {
            assert!(warning.contains("WARN"), "{warning}");
            assert!(warning.contains("convoy=stalled-work"), "{warning}");
            assert!(warning.contains("target="), "{warning}");
            assert!(warning.contains("reason="), "{warning}");
            // DeliveryError logs the transport failure at the attempted send,
            // before the policy-fallback warning that carries cursor/evidence.
            if !matches!(unavailable, GovernorUnavailable::DeliveryError) {
                assert!(warning.contains("supervision_start="), "{warning}");
                assert!(warning.contains("supervision_policy_len="), "{warning}");
                assert!(warning.contains("evidence="), "{warning}");
            }
            // #2592: operator fallback logs contain the source address and exact actionable crew.
            assert!(warning.contains("coder@work in convoy graphql-budget@wheelhouse (resource ref: stalled-work)"), "{warning}");
            assert!(warning.contains("--convoy 'stalled-work' --vessel 'work' --role 'coder' resume"), "{warning}");
            assert!(
                warning.contains(if matches!(unavailable, GovernorUnavailable::DeliveryError) {
                    "supervisor reconnecting"
                } else {
                    "supervisor_lookup_failed"
                }),
                "{warning}"
            );
        }
    }
    // A live governor does not turn an explicit/consumed operator policy
    // into a lookup failure. Both operator routes must explain that choice.
    for (policy, expected_reason) in
        [(Vec::new(), "supervision_policy_exhausted"), (vec![SupervisionTarget::Operator], "operator_rung_selected")]
    {
        let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async {
                let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
                let convoys = backend.using::<Convoy>("flotilla");
                let source = convoys.get("stalled-work").await.expect("source");
                let now = source.metadata.creation_timestamp + chrono::Duration::seconds(120);
                let mut status = source.status.expect("status");
                status.workflow_snapshot.as_mut().expect("snapshot").supervision = Some(policy);
                let source =
                    convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("operator policy");
                wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), source)]), now).await.expect("judge policy");
                assert!(delivery.requests.lock().expect("deliveries").is_empty(), "policy must not contact the live governor");
            });
        });
        let text = String::from_utf8(logs.lock().expect("logs").clone()).expect("utf8 logs");
        assert_eq!(text.lines().count(), 1, "one operator decision: {text}");
        assert!(text.contains(expected_reason), "{text}");
        assert!(text.contains("target=Operator"), "{text}");
        assert!(text.contains("supervision_start=0"), "{text}");
        assert!(!text.contains("supervisor_lookup_failed"), "{text}");
    }
}

// Draw failure modes and sequences of reconciles/restarts. Check fallback after
// every step and require one-pass recovery and exactly one delivery after healing.
#[hegel::test]
fn generated_unavailable_governor_recovers(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let unavailable = GovernorUnavailable::ALL[tc.draw(gs::integers::<usize>().min_value(0).max_value(GovernorUnavailable::ALL.len() - 1))];
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    let restarts = (0..steps).map(|_| tc.draw(gs::booleans())).collect::<Vec<_>>();
    let record = [FallbackRecord::Current, FallbackRecord::LegacyExhausted, FallbackRecord::LegacyExhaustedCursor]
        [tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(unavailable_governor_scenario(unavailable, &restarts, record));
}

#[tokio::test]
async fn stalled_work_routes_to_live_governor_after_abandoned_generation() {
    let (backend, wake, delivery) =
        project_supervision_case(&[("governor-one", 1, ConvoyPhase::Abandoned), ("governor-two", 2, ConvoyPhase::Active)]).await;
    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
    let objects = HashMap::from([
        ("stalled-work".into(), source),
        ("governor-two".into(), backend.using::<Convoy>("flotilla").get("governor-two").await.expect("live governor")),
    ]);
    wake.judge_stalls("flotilla", &objects).await.expect("judge stalls");
    let stalled =
        backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stalled");
    assert_eq!(stalled.supervisor.expect("supervisor").convoy, "governor-two");
    assert_eq!(delivery.requests.lock().expect("deliveries")[0].convoy, "governor-two");
}

// Legacy stalls consumed the governor rung while no supervisor was assigned.
// They must retry that rung after a roll, even when the cursor is present.
#[tokio::test]
async fn exhausted_governor_cursor_retries_in_one_pass() {
    let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let convoys = backend.using::<Convoy>("flotilla");
    flotilla_resources::apply_status_patch(
        &convoys,
        "stalled-work",
        &external_patches::mark_crew_stalled(
            "stalled-work".into(),
            "work".into(),
            "coder".into(),
            Utc::now(),
            flotilla_resources::StallReason::Infra,
            None,
            "needs decision".into(),
        ),
    )
    .await
    .expect("declare stall");
    let source = convoys.get("stalled-work").await.expect("source");
    let mut status = source.status.expect("status");
    let stalled = status.stalled.as_mut().expect("stall");
    stalled.supervision_exhausted = true;
    stalled.supervision_index = Some(1);
    convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("legacy cursor");
    let source = convoys.get("stalled-work").await.expect("source");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("one pass");
    let source = convoys.get("stalled-work").await.expect("source");
    assert_eq!(source.status.expect("status").stalled.expect("stall").rung, StallRung::Governor);
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
}

// A governor homed on another root must receive the escalation in the same
// pass that assigns its rung; replica visibility alone is not delivery.
#[tokio::test]
async fn replicated_governor_receives_stall_in_one_pass() {
    let (remote, _, _) =
        project_supervision_case(&[("governor", 1, ConvoyPhase::Active), ("newer-unowned-governor", 2, ConvoyPhase::Active)]).await;
    create_governor_ensure(&remote, "governor").await;
    let (backend, wake, delivery) = project_supervision_case(&[]).await;
    let mut list = remote.using::<Convoy>("flotilla").list().await.expect("remote convoys");
    list.items.retain(|object| object.metadata.name != "stalled-work");
    backend.replica_writer::<Convoy>(NodeId::new("host-b"), "flotilla").replace(&list, Utc::now()).await.expect("replicate governor");
    backend
        .replica_writer::<ConvoyEnsure>(NodeId::new("host-b"), "flotilla")
        .replace(&remote.using::<ConvoyEnsure>("flotilla").list().await.expect("remote ensure"), Utc::now())
        .await
        .expect("replicate governor ownership");
    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("one pass");
    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
    assert_eq!(source.status.expect("status").stalled.expect("stall").supervisor.expect("supervisor").convoy, "governor");
    let requests = delivery.requests.lock().expect("deliveries");
    assert_eq!(requests.len(), 1, "remote governor must receive its escalation");
    assert_eq!(requests[0].convoy, "governor");
    // Escalations retain the fully-qualified source crew, including its convoy.
    assert_eq!(requests[0].sender, "wheelhouse/stalled-work/work/coder");
    assert_eq!(requests[0].expectation, flotilla_resources::MessageExpectation::Reply);
}

// ProjectCrew requires project scope; a missing ref must be diagnostic,
// never a match with an unrelated projectless governor.
#[tokio::test]
async fn stalled_work_without_project_records_lookup_failure() {
    let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let convoys = backend.using::<Convoy>("flotilla");
    for name in ["stalled-work", "governor"] {
        let source = convoys.get(name).await.expect("convoy");
        let mut spec = source.spec;
        spec.project_ref = None;
        convoys.update(&InputMeta::from(&source.metadata), &source.metadata.resource_version, &spec).await.expect("clear project");
    }
    let source = convoys.get("stalled-work").await.expect("source");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("judge stalls");
    let stall = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
    assert_eq!(stall.rung, StallRung::Operator);
    assert!(stall.evidence.contains("convoy has no project_ref"), "{}", stall.evidence);
    assert!(!stall.supervision_exhausted);
    assert!(delivery.requests.lock().expect("deliveries").is_empty());
}

// Supervision commands must preserve identifiers containing shell metacharacters.
// Formatting glue: spaces and an apostrophe exercise the shared shell quoting helper.
#[tokio::test]
async fn stall_supervision_commands_quote_role_and_vessel_identifiers() {
    let (backend, wake, _) = project_supervision_case(&[]).await;
    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("judge");
    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
    let mut condition = source.status.as_ref().expect("status").stalled.clone().expect("stall");
    let leaf = condition.leaves.first_mut().expect("actor leaf");
    let LeafAddress::Work { work, .. } = &mut leaf.address else { panic!("work leaf") };
    *work = "work space".to_string();
    leaf.field_path = ".crew.coder's role.phase".to_string();
    let brief = stall_supervision_brief(&source, &condition);
    for action in ["resume", "convert-to-failed", "escalate"] {
        assert!(brief.contains(&format!(r"--convoy 'stalled-work' --vessel 'work space' --role 'coder'\''s role' {action}")), "{brief}");
    }
}

// #2592: a non-actor stall retains convoy identity and evidence, without
// fabricating role/vessel supervision commands for an unknown actor.
#[tokio::test]
async fn unattributed_stall_brief_preserves_identity_without_inventing_commands() {
    let (backend, wake, _) = project_supervision_case(&[]).await;
    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("judge");
    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
    let mut condition = source.status.as_ref().expect("status").stalled.clone().expect("stall");
    condition.leaves.clear();
    let brief = stall_supervision_brief(&source, &condition);
    assert!(brief.contains("unidentified crew in convoy graphql-budget@wheelhouse (resource ref: stalled-work)"), "{brief}");
    assert!(brief.contains("Reason: inferred stall. Evidence: needs decision"), "{brief}");
    assert!(!brief.contains("flotilla crew supervise"), "{brief}");
}

// #2654: unchanged operator fallback emits once across ticks, including a restart.
#[test]
fn unchanged_governorless_stall_logs_once() {
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = captured_subscriber(logs.clone(), tracing::Level::WARN);
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tracing scenario").block_on(async {
            let (backend, mut wake, _) = project_supervision_case(&[]).await;
            for tick in 0..10 {
                if tick == 5 {
                    wake = supervision_wake(&backend);
                }
                let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("tracing scenario");
                let objects = HashMap::from([("stalled-work".into(), source)]);
                wake.sync_rows("flotilla", &objects).await.expect("tracing scenario");
                wake.judge_stalls("flotilla", &objects).await.expect("tracing scenario");
            }
        });
    });
    let text = String::from_utf8(logs.lock().expect("tracing scenario").clone()).expect("tracing scenario");
    assert_eq!(text.matches("stall escalation fell back to operator").count(), 1, "{text}");
}

#[tokio::test]
async fn stalled_work_without_live_governor_names_reason_at_operator_rung() {
    let (backend, wake, delivery) = project_supervision_case(&[("governor-one", 1, ConvoyPhase::Abandoned)]).await;
    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("judge stalls");
    let stalled =
        backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stalled");
    assert_eq!(stalled.rung, StallRung::Operator);
    assert!(!stalled.supervision_exhausted);
    // #2592: the operator backstop uses the same actionable description as a supervisor turn.
    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
    let brief = stall_supervision_brief(&source, &stalled);
    assert!(brief.contains("coder@work in convoy graphql-budget@wheelhouse (resource ref: stalled-work)"), "{brief}");
    assert!(brief.contains("--convoy 'stalled-work' --vessel 'work' --role 'coder' resume"), "{brief}");
    assert!(stalled.evidence.contains("no live governor for project wheelhouse"), "{}", stalled.evidence);
    assert!(delivery.requests.lock().expect("deliveries").is_empty());
}

#[tokio::test]
async fn stalled_work_prefers_governor_owned_by_ensure_over_higher_live_generation() {
    let (backend, wake, delivery) =
        project_supervision_case(&[("governor-two", 2, ConvoyPhase::Active), ("governor-three", 3, ConvoyPhase::Active)]).await;
    create_governor_ensure(&backend, "governor-two").await;
    let convoys = backend.using::<Convoy>("flotilla");
    let source = convoys.get("stalled-work").await.expect("source");
    let second = convoys.get("governor-two").await.expect("owned governor");
    let third = convoys.get("governor-three").await.expect("higher generation");
    wake.judge_stalls(
        "flotilla",
        &HashMap::from([("stalled-work".into(), source), ("governor-two".into(), second), ("governor-three".into(), third)]),
    )
    .await
    .expect("judge stalls");
    let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stalled");
    assert_eq!(stalled.supervisor.expect("supervisor").convoy, "governor-two");
    assert_eq!(delivery.requests.lock().expect("deliveries")[0].convoy, "governor-two");
}

#[tokio::test]
async fn stalled_work_names_missing_governor_crew_at_operator_rung() {
    let (backend, wake, delivery) = project_supervision_case(&[("governor-two", 2, ConvoyPhase::Active)]).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let governor = convoys.get("governor-two").await.expect("governor");
    let mut status = governor.status.expect("governor status");
    status.crew_work.clear();
    convoys.update_status("governor-two", &governor.metadata.resource_version, &status).await.expect("clear governor crew");
    let source = convoys.get("stalled-work").await.expect("source");
    let governor = convoys.get("governor-two").await.expect("governor");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source), ("governor-two".into(), governor)]))
        .await
        .expect("judge stalls");
    let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stalled");
    assert_eq!(stalled.rung, StallRung::Operator);
    assert!(stalled.evidence.contains("no live governor crew for project wheelhouse"), "{}", stalled.evidence);
    assert!(delivery.requests.lock().expect("deliveries").is_empty());
}

#[tokio::test]
async fn stalled_work_ignores_terminal_ensure_attempt_and_uses_highest_live_generation() {
    let (backend, wake, delivery) = project_supervision_case(&[
        ("governor-one", 1, ConvoyPhase::Abandoned),
        ("governor-two", 2, ConvoyPhase::Active),
        ("governor-three", 3, ConvoyPhase::Active),
    ])
    .await;
    create_governor_ensure(&backend, "governor-one").await;
    let convoys = backend.using::<Convoy>("flotilla");
    let source = convoys.get("stalled-work").await.expect("source");
    let second = convoys.get("governor-two").await.expect("live governor");
    let third = convoys.get("governor-three").await.expect("highest live governor");
    wake.judge_stalls(
        "flotilla",
        &HashMap::from([("stalled-work".into(), source), ("governor-two".into(), second), ("governor-three".into(), third)]),
    )
    .await
    .expect("judge stalls");
    let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stalled");
    assert_eq!(stalled.supervisor.expect("supervisor").convoy, "governor-three");
    assert_eq!(delivery.requests.lock().expect("deliveries")[0].convoy, "governor-three");
}

#[tokio::test]
async fn lost_crew_session_stalls_with_dead_evidence_but_stale_busy_screen_does_not() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "test-host".into(),
        Arc::new(UnavailableChangeRequests),
        crate::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let wake = ReconcilerWake {
        subscriptions: LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher),
        _marker: PhantomData,
    };
    create_convoy(
        &backend,
        "delivery",
        ConvoyStatus {
            phase: ConvoyPhase::Active,
            crew_work: BTreeMap::from([(
                "work".into(),
                BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            ..Default::default()
        },
    )
    .await;
    let terminal = backend.using::<TerminalSession>("flotilla");
    let created = terminal
        .create(
            &InputMeta::builder()
                .name("terminal-delivery-work-coder".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "delivery".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &flotilla_resources::TerminalSessionSpec {
                env_ref: "env".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Tool { command: "codex".into() },
                cwd: "/workspace".into(),
                env: Default::default(),
                pool: "cleat".into(),
            },
        )
        .await
        .expect("create terminal");
    let mut status = flotilla_resources::TerminalSessionStatus {
        phase: TerminalSessionPhase::Running,
        attention: Some(TerminalAttention {
            state: TerminalAttentionState::Working,
            source: TerminalAttentionSource::Screen,
            as_of: Utc::now() - TerminalAttention::FRESH_FOR - chrono::Duration::seconds(1),
        }),
        ..Default::default()
    };
    let running = terminal.update_status(&created.metadata.name, &created.metadata.resource_version, &status).await.expect("running");
    let leaf = Leaf {
        address: LeafAddress::Work { convoy: "delivery".into(), work: "work".into() },
        field_path: ".crew.coder.phase".into(),
        operator: LeafOperator::Equal,
        literal: "Done".into(),
    };
    let id = uuid::Uuid::new_v4();
    wake.subscriptions.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf],
            watcher: LeafWatcher::ReconcilerWake { convoy: "delivery".into() },
            maker: LeafMaker::Actor { vessel: "work".into(), role: "coder".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        },
    );
    let convoy = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy");
    let objects = HashMap::from([("delivery".into(), convoy)]);
    wake.judge_stalls("flotilla", &objects).await.expect("judge stale screen");
    assert!(backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy").status.expect("status").stalled.is_none());

    flotilla_resources::TerminalSessionStatusPatch::MarkLost {
        reason: "daemon generation is dead; session is recreatable".into(),
        lost_at: Utc::now(),
    }
    .apply(&mut status);
    terminal.update_status(&created.metadata.name, &running.metadata.resource_version, &status).await.expect("lost");
    wake.judge_stalls("flotilla", &objects).await.expect("judge dead session");
    let stalled = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy").status.expect("status").stalled;
    let stalled = stalled.expect("dead actor must stall");
    assert!(stalled.evidence.contains("session dead: daemon generation is dead"));
    assert_eq!(stalled.source, StallEvidenceSource::Session);
}

#[tokio::test]
async fn credential_delivery_and_clone_controller_rows_judge_transient_terminal_and_exhausted_failures() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "test-host".to_string(),
        Arc::new(UnavailableChangeRequests),
        crate::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let wake = ReconcilerWake {
        subscriptions: LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher),
        _marker: PhantomData,
    };
    create_convoy(&backend, "delivery", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    let vessels = backend.using::<Vessel>("flotilla");
    let vessel = vessels
        .create(
            &InputMeta::builder()
                .name("delivery-work".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "delivery".to_string())]))
                .build(),
            &flotilla_resources::VesselSpec {
                convoy_ref: "delivery".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "test".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    vessels
        .update_status(
            "delivery-work",
            &vessel.metadata.resource_version,
            &flotilla_resources::VesselStatus { environment_ref: Some("delivery-env".into()), ..Default::default() },
        )
        .await
        .expect("place vessel");
    let now = Utc::now();
    let backoff = flotilla_resources::RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(120) };
    let scenarios = [
        (ControllerRetry::retryable(None, now, backoff), None),
        (ControllerRetry::terminal(None, now, "credential spec does not decode"), Some("credential spec does not decode")),
        (
            ControllerRetry {
                attempts: RetryCeiling::default().attempts,
                first_failure_at: now,
                disposition: flotilla_resources::ControllerRetryDisposition::Retryable { next_attempt_at: now },
            },
            Some("retrying without progress"),
        ),
    ];
    let mut standing_row_id = None;
    for (retry, expected) in scenarios {
        flotilla_resources::apply_status_patch(
            &vessels,
            "delivery-work",
            &flotilla_resources::VesselStatusPatch::CredentialDelivery { retry: Some(retry) },
        )
        .await
        .expect("record delivery retry");
        let convoy = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy");
        let objects = HashMap::from([("delivery".to_string(), convoy)]);
        wake.sync_rows("flotilla", &objects).await.expect("arm controller row");
        let controller_row = wake
            .subscriptions
            .rows()
            .await
            .into_iter()
            .find(|row| matches!(&row.maker, LeafMaker::Controller { resource_kind, .. } if resource_kind == "CredentialDelivery"))
            .expect("credential delivery row");
        if let Some(id) = standing_row_id {
            assert_eq!(controller_row.id, id, "retry changes should update the standing row without reopening its watches");
        }
        standing_row_id = Some(controller_row.id);
        wake.judge_stalls("flotilla", &objects).await.expect("judge controller row");
        let stalled = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy").status.expect("status").stalled;
        assert_eq!(stalled.as_ref().map(|stalled| stalled.evidence.as_str()), expected);
    }
    flotilla_resources::apply_status_patch(
        &vessels,
        "delivery-work",
        &flotilla_resources::VesselStatusPatch::CredentialDelivery { retry: None },
    )
    .await
    .expect("clear credential retry");
    let checkouts = backend.using::<Checkout>("flotilla");
    checkouts
        .create(
            &InputMeta::builder()
                .name("delivery-checkout".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "delivery".to_string())]))
                .build(),
            &CheckoutSpec::Worktree(flotilla_resources::CheckoutWorktreeSpec {
                repo_ref: RepositoryKey("repo".into()),
                env_ref: "delivery-env".into(),
                r#ref: "main".into(),
                base_ref: None,
                target_path: "/tmp/checkout".into(),
                clone_ref: "delivery-clone".into(),
            }),
        )
        .await
        .expect("worktree checkout");
    for (retry, expected) in [
        (ControllerRetry::retryable(None, now, backoff), None),
        (ControllerRetry::terminal(None, now, "repository not found"), Some("repository not found")),
        (
            ControllerRetry {
                attempts: RetryCeiling::default().attempts,
                first_failure_at: now,
                disposition: flotilla_resources::ControllerRetryDisposition::Retryable { next_attempt_at: now },
            },
            Some("retrying without progress"),
        ),
    ] {
        flotilla_resources::apply_status_patch(
            &checkouts,
            "delivery-checkout",
            &flotilla_resources::CheckoutStatusPatch::ObserveCloneRetry { retry: Some(retry) },
        )
        .await
        .expect("project clone retry");
        let convoy = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy");
        let objects = HashMap::from([("delivery".to_string(), convoy)]);
        wake.sync_rows("flotilla", &objects).await.expect("arm clone controller row");
        wake.judge_stalls("flotilla", &objects).await.expect("judge clone controller row");
        let stalled = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy").status.expect("status").stalled;
        assert_eq!(stalled.as_ref().map(|stalled| stalled.evidence.as_str()), expected);
    }
}

// #2499: Landing keeps its exit gates while the observed maker waits at a
// known forge deadline. Only an expired deadline without recovery can stall.
#[tokio::test]
async fn landing_observation_cooldown_waits_until_deadline_without_stalling() {
    struct LimitedSource(ObservationError);
    #[async_trait]
    impl crate::change_request_observer::ChangeRequestObservationSource for LimitedSource {
        async fn observe(&self, _: &ChangeRequestRef) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
            Err(self.0.clone())
        }
    }
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let now = Utc::now();
    let retry_at = now + chrono::Duration::seconds(60);
    let error = ObservationError::RateLimited {
        budget: "GraphQL".into(),
        limit: GithubRateLimit {
            kind: GithubRateLimitKind::Secondary,
            retry_source: GithubRetrySource::RetryAfter,
            retry_at: Some(retry_at),
        },
    };
    let refresher = ChangeRequestRefresher::new(
        "fleet".into(),
        backend.clone(),
        "authority".into(),
        Arc::new(LimitedSource(error)),
        crate::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let subject = ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "team/one".into(), number: 1 };
    refresher.refresh_once(&subject).await.expect_err("limited observation records its diagnostic");
    let (event_tx, _) = broadcast::channel(4);
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
    let convoys = backend.using::<Convoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("cooldown".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("convoy");
    convoys
        .update_status("cooldown", &created.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Landing, ..Default::default() })
        .await
        .expect("landing");
    let id = uuid::Uuid::new_v4();
    table.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec!["cr/github.com/team/one/1 .state == merged".parse().expect("exit leaf")],
            watcher: LeafWatcher::ReconcilerWake { convoy: "cooldown".into() },
            maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
            freshness_demand: Some(now),
            created_at: now,
            episode_key: EpisodeKeyFields::default(),
        },
    );
    let wake = ReconcilerWake { subscriptions: table, _marker: PhantomData };
    for at in [now, retry_at - chrono::Duration::nanoseconds(1), retry_at] {
        let convoy = convoys.get("cooldown").await.expect("convoy");
        wake.judge_stalls_at("flotilla", &HashMap::from([("cooldown".into(), convoy)]), at).await.expect("judge");
        let status = convoys.get("cooldown").await.expect("convoy").status.expect("status");
        assert_eq!(status.phase, ConvoyPhase::Landing, "missing merge evidence must never land");
        assert_eq!(status.stalled.is_some(), at >= retry_at, "only the expired unrecovered wait can stall");
    }
}
