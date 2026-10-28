use flotilla_change_request::observation;
use flotilla_resources::ResumeIntent;
use flotilla_store_memory::InMemoryBackend;

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
                flotilla_store::apply_status_patch(
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

// #3586: decoded legacy refusals neither customize nudges nor escalate, even
// at the former refusal threshold. Normal actor inactivity policy still applies.
#[tokio::test]
async fn stored_refusals_do_not_change_nudges_after_restore() {
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
                .consecutive_count(2)
                .build(),
        );
        // Construct the previous writer's shape: the new writer omits refusals.
        let mut stored = serde_json::to_value(&status).expect("store status");
        stored["crew_work"]["work"]["coder"]["completion_refusal"] =
            serde_json::to_value(&status.crew_work["work"]["coder"].completion_refusal).unwrap();
        let restored: flotilla_resources::ConvoyStatus = serde_json::from_value(stored).expect("restore status");
        assert!(restored.crew_work["work"]["coder"].completion_refusal.is_some());
        convoys.update_status("stalled-work", &convoy.metadata.resource_version, &restored).await.expect("persist refusal");
        let start = Utc::now();
        for second in [0, 60, 120, 180] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        let requests = delivery.requests.lock().expect("deliveries");
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].brief.contains(remedy), "{}", requests[0].brief);
        assert!(!requests[0].brief.contains("Presentation wording changed"));
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
                fulfilment_kind_ref: "policy".into(),
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
    flotilla_store::apply_status_patch(
        &convoys,
        "stalled-work",
        &flotilla_resources::external_patches::resume_crew_work(
            "work".into(),
            "coder".into(),
            resumed_at,
            "Continue with the operator's guidance".into(),
            Some("resume-brief".into()),
            ResumeIntent::Reopen,
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
    status.crew_work.get_mut("work").expect("crew").insert("bosun".into(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build());
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
// Hegel charges this bounded informer scenario's execution to generation time;
// scheduler delay must not change the verdict. Keep all coverage and other checks.
#[hegel::test(suppress_health_check = [hegel::HealthCheck::TooSlow])]
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
    flotilla_store::apply_status_patch(
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
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "test-host".into(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let wake = ReconcilerWake::new(LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher));
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
    // The first judgement persisted hook health and obligations. Refresh the
    // outer convoy snapshot as its watch handler does before judging new evidence.
    let convoy = backend.using::<Convoy>("flotilla").get("delivery").await.expect("current convoy");
    let objects = HashMap::from([("delivery".into(), convoy)]);
    wake.judge_stalls("flotilla", &objects).await.expect("judge dead session");
    let stalled = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy").status.expect("status").stalled;
    let stalled = stalled.expect("dead actor must stall");
    assert!(stalled.evidence.contains("session dead: daemon generation is dead"));
    assert_eq!(stalled.source, StallEvidenceSource::Session);
}

#[tokio::test]
async fn credential_delivery_and_clone_controller_rows_judge_transient_terminal_and_exhausted_failures() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "test-host".to_string(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let wake = ReconcilerWake::new(LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher));
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
                fulfilment_kind_ref: "test".into(),
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
        flotilla_store::apply_status_patch(
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
    flotilla_store::apply_status_patch(
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
        flotilla_store::apply_status_patch(
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
    impl observation::ChangeRequestObservationSource for LimitedSource {
        async fn observe(&self, _: &ChangeRequestKey) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
            Err(self.0.clone())
        }
    }
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let now = Utc::now();
    let retry_at = now + chrono::Duration::seconds(60);
    let error = {
        let limit =
            GithubRateLimit { kind: GithubRateLimitKind::Secondary, retry_source: GithubRetrySource::RetryAfter, retry_at: Some(retry_at) };
        limit.observation_error("GraphQL".into())
    };
    let refresher = ChangeRequestRefresher::new(
        "fleet".into(),
        backend.clone(),
        "authority".into(),
        Arc::new(LimitedSource(error)),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let subject = ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address("github.com", "team/one", 1, &[]).expect("address"),
    )
    .expect("key");
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
    let wake = ReconcilerWake::new(table);
    let deadline = retry_at + chrono::Duration::seconds(15);
    for at in [now, retry_at, deadline - chrono::Duration::nanoseconds(1), deadline] {
        let convoy = convoys.get("cooldown").await.expect("convoy");
        wake.judge_stalls_at("flotilla", &HashMap::from([("cooldown".into(), convoy)]), at).await.expect("judge");
        let status = convoys.get("cooldown").await.expect("convoy").status.expect("status");
        assert_eq!(status.phase, ConvoyPhase::Landing, "missing merge evidence must never land");
        assert_eq!(status.stalled.is_some(), at >= deadline, "only the expired refresh plus cadence can stall");
    }
}

// #3044: populated kinds are bootstrapped once; repeated no-op judgements
// perform zero backing-store reads, including indirect supervisor routing reads.
#[tokio::test]
async fn steady_state_stall_judgements_do_not_read_the_store() {
    use flotilla_store_testkit::ReadCountsBackendExt;
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
    let wake = supervision_wake(&backend);
    backend
        .definitions::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("project".into()).build(),
            &flotilla_resources::ProjectSpec::builder().display_name("Project".into()).build(),
        )
        .await
        .expect("project");
    for index in 0..32 {
        create_convoy(&backend, &format!("convoy-{index}"), ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    }
    for index in 0..32 {
        let resolver = backend.using::<Convoy>("flotilla");
        let convoy = resolver.get(&format!("convoy-{index}")).await.expect("convoy");
        let mut spec = convoy.spec;
        spec.project_ref = Some("project".into());
        resolver.update(&InputMeta::from(&convoy.metadata), &convoy.metadata.resource_version, &spec).await.expect("project membership");
    }
    let objects = backend
        .using::<Convoy>("flotilla")
        .list()
        .await
        .expect("snapshot")
        .items
        .into_iter()
        .map(|convoy| (convoy.metadata.name.clone(), convoy))
        .collect();
    let now = Utc::now();
    wake.judge_stalls_at("flotilla", &objects, now).await.expect("bootstrap");
    let before = backend.read_snapshot();
    for _ in 0..64 {
        wake.judge_stalls_at("flotilla", &objects, now).await.expect("steady state judgement");
    }
    let reads = backend.read_snapshot().since(&before);
    reads.assert_zero("stall judgement including nested routing helpers");
}

// #3045: the leaf engine's steady state is served by watch views. After one
// bootstrap pass, reconcile passes that observe convoy changes delivered by
// watches read no store, on any task: views refresh from events.
#[tokio::test]
async fn leaf_engine_steady_state_reconcile_reads_no_store() {
    use flotilla_store_testkit::ReadCountsBackendExt;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let counted = backend.clone().with_read_counts();
    let wake = supervision_wake(&counted);
    backend
        .definitions::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("project".into()).build(),
            &flotilla_resources::ProjectSpec::builder().display_name("Project".into()).build(),
        )
        .await
        .expect("project");
    for index in 0..8 {
        create_convoy(&backend, &format!("convoy-{index}"), ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    }
    wake.subscriptions.reconcile_stalls_once("flotilla").await.expect("bootstrap pass");
    let bootstrap = counted.read_snapshot();
    assert!(!bootstrap.lists().is_empty(), "the bootstrap pass builds its views from lists\n{bootstrap}");
    let convoys = backend.using::<Convoy>("flotilla");
    for index in 0..16 {
        // A status write reaches the views as a watch event, not a relist.
        let name = format!("convoy-{}", index % 8);
        let convoy = convoys.get(&name).await.expect("convoy");
        let mut status = convoy.status.clone().expect("status");
        status.message = Some(format!("event-{index}"));
        convoys.update_status(&name, &convoy.metadata.resource_version, &status).await.expect("status write");
        wake.subscriptions.reconcile_stalls_once("flotilla").await.expect("steady state pass");
        let resident = wake.local_convoys("flotilla").await.expect("resident convoys");
        assert_eq!(resident[&name].status.as_ref().expect("resident status").message, status.message);
    }
    counted.read_snapshot().since(&bootstrap).assert_zero("leaf engine reconcile passes");
}

// #3118: one declared stall episode yields one receiver-homed Message even
// when the crew repeats its report and the judge restarts between passes.
async fn stall_message_scenario(repeat_report: bool, restart: bool) {
    use flotilla_resources::{Message, MessageSpec, StallReason};
    struct InboxDelivery(flotilla_messaging::MessageInbox);
    // Boundary stand-in for turn delivery; persist through the real Message inbox.
    #[async_trait]
    impl TurnDeliveryActuator for InboxDelivery {
        async fn deliver(&self, request: &CrewTurnIntent) -> Result<CrewTurnAdmission, String> {
            let receiver =
                request.receiver.clone().unwrap_or_else(|| format!("wheelhouse/{}/{}/{}", request.convoy, request.vessel, request.role,));
            let name = flotilla_resources::message_record_name(
                &receiver,
                &request.sender,
                &turn_message_producer_key(&request.source, &request.subject_revision),
            );
            let spec = MessageSpec::builder()
                .sender(request.sender.clone())
                .receiver(receiver)
                .body(request.brief.clone())
                .relation(request.relation)
                .references(request.references.clone())
                .expectation(request.expectation.clone())
                .build();
            let admission =
                self.0.accept(&InputMeta::builder().name(name).build(), &spec, Utc::now()).await.map_err(|error| error.to_string())?;
            let object = match admission {
                flotilla_messaging::MessageAdmission::Accepted(object) => object,
                flotilla_messaging::MessageAdmission::Suppressed { predecessor } => predecessor,
            };
            Ok(CrewTurnAdmission {
                new_turn: true,
                rung: TurnDeliveryRung::WarmSession,
                message: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", object.metadata.name),
            })
        }
        async fn hold(&self, _: &CrewTurnIntent, _: &HoldAct, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let (backend, _, _) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let delivery = Arc::new(InboxDelivery(flotilla_messaging::MessageInbox::new(backend.clone(), "flotilla")));
    let convoys = backend.using::<Convoy>("flotilla");
    let now = Utc::now();
    let stale_objects: HashMap<_, _> =
        convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
    let mut wake = supervision_wake(&backend);
    for step in 0..3 {
        if repeat_report {
            flotilla_store::apply_status_patch(
                &convoys,
                "stalled-work",
                &flotilla_resources::external_patches::mark_crew_stalled(
                    "stalled-work".into(),
                    "work".into(),
                    "coder".into(),
                    now + chrono::Duration::seconds(step),
                    StallReason::Scope,
                    None,
                    "needs decision".into(),
                ),
            )
            .await
            .expect("report stall");
        }
        if restart {
            wake = supervision_wake(&backend);
        }
        wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
        let objects = if repeat_report {
            convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect()
        } else {
            stale_objects.clone()
        };
        wake.sync_rows("flotilla", &objects).await.expect("rows");
        wake.judge_stalls_at("flotilla", &objects, now + chrono::Duration::seconds(step)).await.expect("judge");
        let messages = backend.using::<Message>("flotilla").list().await.expect("inbox").items;
        assert_eq!(messages.len(), 1, "one episode must not create a burst of copies");
    }
    // Catching up the outer watch rebuilds rows and continues judging normally.
    let current = convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
    wake.sync_rows("flotilla", &current).await.expect("current rows");
    wake.judge_stalls_at("flotilla", &current, now + chrono::Duration::seconds(4)).await.expect("current judge");
    assert_eq!(backend.using::<Message>("flotilla").list().await.expect("inbox").items.len(), 1);
}

#[tokio::test]
async fn repeated_stall_reports_publish_one_supervisor_message() {
    for restart in [false, true] {
        stall_message_scenario(true, restart).await;
    }
}

// A delayed convoy watch must not recreate an inferred episode at each tick.
#[tokio::test]
async fn stale_stall_snapshot_publishes_one_supervisor_message() {
    for restart in [false, true] {
        stall_message_scenario(false, restart).await;
    }
}

// Only the authority may judge a convoy. An old caller map must not actuate a
// replica that remains visible after its local record has been removed.
#[tokio::test]
async fn replica_only_convoy_in_caller_snapshot_is_not_judged() {
    let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let source = convoys.get("stalled-work").await.expect("source");
    convoys.delete("stalled-work").await.expect("remove local authority");
    backend
        .replica_writer::<Convoy>(NodeId::new("other-authority"), "flotilla")
        .apply(WatchEvent::Added(source.clone()), Utc::now())
        .await
        .expect("replica");
    let visible = backend.including_replicas::<Convoy>("flotilla").get("stalled-work").await.expect("visible replica");
    assert!(matches!(visible.provenance, flotilla_resources::ResourceProvenance::Replica { .. }));
    let objects = HashMap::from([("stalled-work".into(), source)]);
    wake.sync_rows("flotilla", &objects).await.expect("old rows");
    wake.judge_stalls_at("flotilla", &objects, Utc::now()).await.expect("judge obsolete caller map");
    assert!(delivery.requests.lock().expect("requests").is_empty(), "a replica must never emit a supervision turn");
    assert!(convoys.get("stalled-work").await.is_err(), "judging must not recreate local authority");
}

// #3439: a handoff is an outstanding report even without a declared claim.
// Pending (available) and Done roles owe nothing and must remain exempt.
#[tokio::test]
async fn claim_less_working_crew_stalls_with_actor_evidence() {
    for (phase, attention) in [
        (CrewWorkPhase::Working, TerminalAttentionState::Idle),
        (CrewWorkPhase::Working, TerminalAttentionState::Working),
        (CrewWorkPhase::Pending, TerminalAttentionState::Idle),
        (CrewWorkPhase::Done, TerminalAttentionState::Idle),
        (CrewWorkPhase::HandedBack, TerminalAttentionState::Idle),
    ] {
        let (backend, wake, delivery) = idle_nudge_scenario().await;
        wake.subscriptions.inner.rows.lock().await.clear();
        update_convoy(&backend, "stalled-work", |status| {
            let member = &mut status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew[0];
            member.promises.clear();
            member.completion_conditions.clear();
            status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew").phase = CrewWorkPhase::Done;
            flotilla_resources::ConvoyStatusPatch::HandoffCrewWork {
                vessel: "work".into(),
                sender_role: "coder".into(),
                target_role: "coder".into(),
                handed_off_at: Utc::now(),
                message: "Follow-up task".into(),
            }
            .apply(status);
            status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew").phase = phase;
        })
        .await;
        let convoys = backend.using::<Convoy>("flotilla");
        let objects = HashMap::from([("stalled-work".into(), convoys.get("stalled-work").await.expect("convoy"))]);
        wake.sync_rows("flotilla", &objects).await.expect("sync rows");
        let now = Utc::now();
        for second in [0, 60, 120, 180, 900, 1800] {
            observe_actor(&backend, &wake, attention, now + chrono::Duration::seconds(second)).await;
            if second == 0 || (attention == TerminalAttentionState::Working && second < 900) {
                assert!(
                    convoys.get("stalled-work").await.expect("convoy").status.expect("status").stalled.is_none(),
                    "fresh work and the idle grace must remain able/unknown"
                );
            }
        }
        let status = convoys.get("stalled-work").await.expect("convoy").status.expect("status");
        assert_eq!(status.stalled.is_some(), phase == CrewWorkPhase::Working, "{phase:?}: {status:?}");
        if let Some(stalled) = status.stalled {
            assert!(!stalled.evidence.is_empty());
            assert!(!stalled.leaves.is_empty());
            if attention == TerminalAttentionState::Working {
                assert!(stalled.evidence.contains("turn inactivity bound exceeded"));
            } else {
                assert!(!delivery.requests.lock().expect("deliveries").is_empty(), "idle work follows the normal nudge ladder");
            }
        }
    }
}

// Launching a claim-less role makes it Working, but does not assign
// a task. Even repeated idle observations beyond grace must create no actor row.
#[tokio::test]
async fn freshly_launched_claim_less_role_stays_unjudged() {
    let (backend, wake, _) = idle_nudge_scenario().await;
    wake.subscriptions.inner.rows.lock().await.clear();
    update_convoy(&backend, "stalled-work", |status| {
        let member = &mut status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew[0];
        member.promises.clear();
        member.completion_conditions.clear();
        status.crew_work.get_mut("work").unwrap().get_mut("coder").unwrap().phase = CrewWorkPhase::Pending;
        flotilla_resources::ConvoyStatusPatch::WorkRunning {
            work: "work".into(),
            started_at: Utc::now(),
            launched_roles: ["coder".into()].into(),
        }
        .apply(status);
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    })
    .await;
    let convoys = backend.using::<Convoy>("flotilla");
    let objects = HashMap::from([("stalled-work".into(), convoys.get("stalled-work").await.expect("convoy"))]);
    wake.sync_rows("flotilla", &objects).await.expect("sync");
    assert!(
        wake.subscriptions.inner.rows.lock().await.values().all(|row| !matches!(row.maker, LeafMaker::Actor { .. })),
        "launch alone must not arm an actor obligation"
    );
    let now = Utc::now();
    for second in [0, 60, 120, 1800] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, now + chrono::Duration::seconds(second)).await;
        assert!(convoys.get("stalled-work").await.unwrap().status.unwrap().stalled.is_none());
    }
}

// A crew that declares its own promise owes a report even without a workflow
// claim or follow-up task. Narrowing launch eligibility must retain this case.
#[tokio::test]
async fn a_launched_role_that_declares_a_promise_owes_a_report() {
    let (backend, wake, _) = idle_nudge_scenario().await;
    wake.subscriptions.inner.rows.lock().await.clear();
    update_convoy(&backend, "stalled-work", |status| {
        let member = &mut status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew[0];
        member.promises.clear();
        member.completion_conditions.clear();
        flotilla_resources::ConvoyStatusPatch::Promise {
            vessel: "work".into(),
            role: "coder".into(),
            operation: flotilla_resources::promises::PromiseOperation::Declare {
                id: "pr".into(),
                kind: flotilla_resources::promises::PromiseKind::Pr,
                source: flotilla_resources::promises::PromiseSource::Crew,
            },
        }
        .apply(status);
    })
    .await;
    let convoys = backend.using::<Convoy>("flotilla");
    let objects = HashMap::from([("stalled-work".into(), convoys.get("stalled-work").await.expect("convoy"))]);
    wake.sync_rows("flotilla", &objects).await.expect("sync");
    assert!(wake.subscriptions.inner.rows.lock().await.values().any(|row| matches!(row.maker, LeafMaker::Actor { .. })));
    let now = Utc::now();
    for second in [0, 60, 120, 180, 900, 1800] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, now + chrono::Duration::seconds(second)).await;
    }
    assert!(convoys.get("stalled-work").await.unwrap().status.unwrap().stalled.is_some());
}

// Resolving an explicit stall with guidance assigns a report even if the role
// was only launched before stalling. Silence after that resume must be judged.
#[tokio::test]
async fn resumed_explicit_stall_without_claim_still_owes_a_report() {
    let (backend, wake, _) = idle_nudge_scenario().await;
    wake.subscriptions.inner.rows.lock().await.clear();
    update_convoy(&backend, "stalled-work", |status| {
        let member = &mut status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew[0];
        member.promises.clear();
        member.completion_conditions.clear();
        let at = Utc::now();
        flotilla_resources::ConvoyStatusPatch::MarkCrewStalled {
            convoy: "stalled-work".into(),
            vessel: "work".into(),
            role: "coder".into(),
            at,
            reason: flotilla_resources::StallReason::Decision,
            proposed_disposition: None,
            message: "Need guidance".into(),
        }
        .apply(status);
        assert!(!status.crew_work["work"]["coder"].task_assigned);
        flotilla_resources::ConvoyStatusPatch::ResumeCrewWork {
            vessel: "work".into(),
            role: "coder".into(),
            resumed_at: at,
            prompt: "Continue with this guidance".into(),
            brief_id: None,
            intent: ResumeIntent::Reopen,
        }
        .apply(status);
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
        assert!(status.crew_work["work"]["coder"].task_assigned);
    })
    .await;
    let convoys = backend.using::<Convoy>("flotilla");
    let objects = HashMap::from([("stalled-work".into(), convoys.get("stalled-work").await.expect("convoy"))]);
    wake.sync_rows("flotilla", &objects).await.expect("sync");
    let now = Utc::now();
    for second in [0, 60, 120, 180, 900, 1800] {
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, now + chrono::Duration::seconds(second)).await;
    }
    assert!(convoys.get("stalled-work").await.unwrap().status.unwrap().stalled.is_some());
}

// #3572: standing convoys never owe settlement claims. For ordinary convoys,
// terminal promises alone owe nothing; tasks and completion conditions still do.
#[hegel::test]
fn settlement_claim_arming_respects_exit_and_pending_promises(tc: hegel::TestCase) {
    use flotilla_resources::promises::{Promise, PromiseKind, PromiseSource, PromiseState};
    use hegel::generators as gs;
    // Generate duplicate promise counts, including an empty history. Pin every
    // state and obligation combination so each eligibility boundary is exercised.
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        for standing in [false, true] {
            for state in [PromiseState::Open, PromiseState::Submitted, PromiseState::Kept, PromiseState::Retracted] {
                for (task, completion) in [(false, false), (true, false), (false, true)] {
                    let (backend, wake, delivery) = idle_nudge_scenario().await;
                    update_convoy(&backend, "stalled-work", |status| {
                        let snapshot = status.workflow_snapshot.as_mut().expect("workflow");
                        snapshot.exit = if standing { None } else { Some(ExitDeclaration::standard_table()) };
                        let member = &mut snapshot.vessels[0].crew[0];
                        member.promises.clear();
                        if !completion {
                            member.completion_conditions.clear();
                        }
                        status.crew_work.get_mut("work").unwrap().get_mut("coder").unwrap().task_assigned = task;
                        status.promises.entry("work".into()).or_default().insert(
                            "coder".into(),
                            (0..count)
                                .map(|i| Promise {
                                    id: format!("pr-{i}"),
                                    vessel: "work".into(),
                                    role: "coder".into(),
                                    source: PromiseSource::Crew,
                                    kind: PromiseKind::Pr,
                                    state,
                                    retraction_reason: None,
                                    submissions: Vec::new(),
                                })
                                .collect(),
                        );
                    })
                    .await;
                    let convoys = backend.using::<Convoy>("flotilla");
                    let objects = HashMap::from([("stalled-work".into(), convoys.get("stalled-work").await.unwrap())]);
                    wake.sync_rows("flotilla", &objects).await.expect("sync");
                    let expected =
                        !standing && (task || completion || (count > 0 && matches!(state, PromiseState::Open | PromiseState::Submitted)));
                    let armed = wake.subscriptions.inner.rows.lock().await.values().any(|row| matches!(row.maker, LeafMaker::Actor { .. }));
                    assert_eq!(
                        armed, expected,
                        "standing={standing}, state={state:?}, count={count}, task={task}, completion={completion}"
                    );
                    if !expected {
                        let now = Utc::now();
                        for second in [0, 60, 120, 1800] {
                            observe_actor(&backend, &wake, TerminalAttentionState::Idle, now + chrono::Duration::seconds(second)).await;
                        }
                        assert!(delivery.requests.lock().unwrap().is_empty(), "no settlement nudge without an obligation");
                        assert!(convoys.get("stalled-work").await.unwrap().status.unwrap().stalled.is_none());
                    }
                }
            }
        }
    });
}

// #3572: explicit stalls do not make standing roles owe settlement claims.
// A missing workflow snapshot also declares no exit and must remain exempt.
#[tokio::test]
async fn standing_stalled_and_snapshot_less_roles_have_no_settlement_obligation() {
    // Glue: pin the two no-exit representations and the explicit-stall branch
    // through the same in-memory subscription derivation as the property above.
    for (phase, missing_snapshot) in [(CrewWorkPhase::Stalled, false), (CrewWorkPhase::Working, true), (CrewWorkPhase::Stalled, true)] {
        let (backend, wake, _) = idle_nudge_scenario().await;
        update_convoy(&backend, "stalled-work", |status| {
            if missing_snapshot {
                status.workflow_snapshot = None;
            } else {
                status.workflow_snapshot.as_mut().expect("workflow").exit = None;
            }
            let work = status.crew_work.get_mut("work").unwrap().get_mut("coder").unwrap();
            work.phase = phase;
            work.task_assigned = true;
        })
        .await;
        let convoys = backend.using::<Convoy>("flotilla");
        let objects = HashMap::from([("stalled-work".into(), convoys.get("stalled-work").await.unwrap())]);
        wake.sync_rows("flotilla", &objects).await.expect("sync");
        assert!(
            wake.subscriptions.inner.rows.lock().await.values().all(|row| !matches!(row.maker, LeafMaker::Actor { .. })),
            "phase={phase:?}, missing_snapshot={missing_snapshot}"
        );
    }
}

// Pending launch stays exempt from dead-session stalls. Lifecycle progress buys
// another grace period; a stuck stage raises stable operator attention and recovers.
#[hegel::test]
fn pending_launch_progress_is_bounded_and_recovers(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Repeat judgments on either side of the exact 15-minute boundary.
    let repeat = tc.draw(gs::integers::<i64>().min_value(1).max_value(60));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (backend, wake, _) = actor_nudge_scenario(&[]).await;
        let sessions = backend.using::<TerminalSession>("flotilla");
        let saved = sessions.get("resumed-coder").await.expect("session");
        sessions.delete("resumed-coder").await.expect("remove launch");
        let start = Utc::now();
        update_convoy(&backend, "stalled-work", |status| {
            status.started_at = Some(start);
            status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew[0].source = flotilla_resources::CrewSource::Agent {
                selector: flotilla_resources::Selector::for_capability("coding"),
                prompt: None,
                brief_template: None,
            };
            let mut latent = status.workflow_snapshot.as_ref().expect("workflow").vessels[0].crew[0].clone();
            latent.role = "latent".into();
            status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew.push(latent);
            status
                .crew_work
                .get_mut("work")
                .expect("work")
                .insert("latent".into(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build());
            status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew").phase = CrewWorkPhase::Pending;
        })
        .await;
        let judge = |seconds| {
            let backend = backend.clone();
            let wake = wake.clone();
            async move {
                let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
                wake.judge_stalls_at(
                    "flotilla",
                    &HashMap::from([("stalled-work".into(), convoy)]),
                    start + chrono::Duration::seconds(seconds),
                )
                .await
                .expect("judge");
                backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status")
            }
        };
        for seconds in [0, 899] {
            let status = judge(seconds).await;
            assert!(status.stalled.is_none());
            assert!(status.attention.is_none());
        }
        // Session creation is real launch progress even after a long queue wait.
        let session = sessions
            .create(&InputMeta::builder().name("resumed-coder".into()).labels(saved.metadata.labels).build(), &saved.spec)
            .await
            .expect("new launch");
        assert!(judge(899).await.attention.is_none());
        // Receiving a Starting status is another lifecycle transition.
        sessions.update_status("resumed-coder", &session.metadata.resource_version, &Default::default()).await.expect("starting");
        let progressing = judge(1798).await;
        assert!(progressing.attention.is_none(), "{:?}", progressing.attention);
        for seconds in [1799, 2697] {
            let status = judge(seconds).await;
            assert!(status.stalled.is_none());
            assert!(status.attention.is_none());
        }
        let overdue = judge(2698).await;
        assert!(overdue.stalled.is_none(), "Pending remains startup-unknown");
        let attention = overdue.attention.expect("bounded launch diagnostic");
        assert_eq!(attention.source, "launch-progress");
        assert!(attention.reason.contains("coder@work") && attention.reason.contains("Starting"), "{}", attention.reason);
        assert!(!attention.reason.contains("latent@work"), "latent roles await handoff");
        let repeated = judge(2698 + repeat).await;
        assert_eq!(repeated.attention, Some(attention.clone()));
        let other = ConvoyAttention { source: "operator".into(), reason: "keep this decision".into(), raised_at: start };
        update_convoy(&backend, "stalled-work", |status| status.attention = Some(other.clone())).await;
        assert_eq!(judge(2799).await.attention, Some(other), "launch health must preserve unrelated attention");
        update_convoy(&backend, "stalled-work", |status| status.attention = None).await;
        // Retry failures are diagnostic evidence, not successful progress.
        let session = sessions.get("resumed-coder").await.expect("session");
        let mut status = session.status.expect("starting");
        status.degraded = Some(flotilla_resources::TerminalSessionDegradedCondition {
            reason: "launch".into(),
            message: "launcher retry failed".into(),
            message_id: None,
            consecutive_failures: 3,
            observed_at: start + chrono::Duration::seconds(2800),
        });
        sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("retry evidence");
        assert!(judge(2800).await.attention.expect("retry stays overdue").reason.contains("launcher retry failed"));
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(2801)).await;
        assert!(judge(2801).await.attention.is_none(), "Running is progress, not a dead session");
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(3701)).await;
        assert!(judge(3701).await.attention.is_some(), "fresh idle observations are not launch progress");
        update_convoy(&backend, "stalled-work", |status| {
            status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew").phase = CrewWorkPhase::Working;
        })
        .await;
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(3702)).await;
        assert!(judge(3702).await.attention.is_none(), "recovery clears launch attention");
        // Working crews still regain absent-session detection after launch.
        sessions.delete("resumed-coder").await.expect("lost session");
        judge(3800).await;
        assert!(judge(3861).await.stalled.is_some());
    });
}

// A vessel the engine realises is launch progress even while its stored status still says Provisioning.
#[tokio::test]
async fn engine_vessel_progress_resets_the_launch_bound() {
    use flotilla_protocol::{CanonicalHostId, PlacementDecision, PlacementTargetHost};
    use flotilla_provisioning_testkit::{object, Ending, VesselFixture};
    let (backend, wake, _) = actor_nudge_scenario(&[]).await;
    let fixture = VesselFixture::new(false);
    let wake = ReconcilerWake::new(wake.subscriptions.with_lifecycle_reads(fixture.reads.clone()));
    backend.using::<TerminalSession>("flotilla").delete("resumed-coder").await.expect("no launch yet");
    let start = Utc::now();
    update_convoy(&backend, "stalled-work", |status| {
        status.started_at = Some(start);
        let work = status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew");
        work.phase = CrewWorkPhase::Pending;
        work.task_assigned = true;
    })
    .await;
    let vessel = object::<flotilla_resources::Vessel>(
        "stalled-work-work",
        flotilla_resources::VesselSpec {
            convoy_ref: "stalled-work".into(),
            vessel_name: "work".into(),
            fulfilment_kind_ref: "local".into(),
            adopted_checkout_refs: Default::default(),
        },
        flotilla_resources::VesselStatus {
            phase: flotilla_resources::VesselPhase::Provisioning,
            placement_decision: Some(
                PlacementDecision::builder()
                    .kind("local".into())
                    .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved("host"), display_name: "host".into() })
                    .build(),
            ),
            ..Default::default()
        },
    );
    let vessels = backend.using::<flotilla_resources::Vessel>("flotilla");
    let created = vessels
        .create(
            &InputMeta::builder()
                .name(vessel.metadata.name.clone())
                .labels(BTreeMap::from([(CONVOY_LABEL.into(), "stalled-work".into())]))
                .build(),
            &vessel.spec,
        )
        .await
        .expect("vessel");
    vessels
        .update_status(&vessel.metadata.name, &created.metadata.resource_version, vessel.status.as_ref().expect("status"))
        .await
        .expect("stored");
    fixture.request(&vessel);
    let judge = |seconds| {
        let backend = backend.clone();
        let wake = &wake;
        async move {
            let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
            wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), start + chrono::Duration::seconds(seconds))
                .await
                .expect("judge");
            backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status").attention
        }
    };
    assert!(judge(0).await.is_none());
    fixture.finish(&vessel, Ending::Ready("env"));
    assert!(judge(600).await.is_none());
    assert!(judge(1499).await.is_none(), "the engine's Ready restarted the bound");
    assert_eq!(judge(1500).await.expect("bounded from the engine's progress").source, "launch-progress");
}

// Supervision has a crew or convoy subject, and only a recipient with an inbox awaits a reply.
#[tokio::test]
async fn supervision_records_have_a_subject_and_operator_notices_expect_nothing() {
    let (backend, wake, _) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let source = convoys.get("stalled-work").await.expect("source");
    wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("judge");
    let source = convoys.get("stalled-work").await.expect("source");
    let mut condition = source.status.as_ref().expect("status").stalled.clone().expect("stall");
    assert_eq!(super::super::stalls::stall_subject("flotilla", &source, &condition), "wheelhouse/stalled-work/work/coder");
    condition.leaves.clear();
    assert_eq!(super::super::stalls::stall_subject("flotilla", &source, &condition), "project:wheelhouse/convoy:stalled-work");
    assert_eq!(super::super::stalls::supervision_expectation(StallRung::Operator), flotilla_resources::MessageExpectation::None);
    assert_eq!(super::super::stalls::supervision_expectation(StallRung::Governor), flotilla_resources::MessageExpectation::Reply);
}
