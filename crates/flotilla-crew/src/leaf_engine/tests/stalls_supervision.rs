use flotilla_change_request::observation;
use flotilla_provisioning_testkit::TerminalsFixture;
use flotilla_resources::attentions::{Sender, Subject};
use flotilla_resources::checkout_records::CheckoutHolder;
use flotilla_resources::convoy::engine::Actor;
use flotilla_resources::promises::{PromiseKind, PromiseOperation, PromiseSource, PromiseState, Submission, SubmissionVerdict};
use flotilla_resources::terminal_records::{AgentLaunch, TerminalLaunchFacts, TerminalLaunchRequest, TerminalLaunchSource};
use flotilla_resources::ResumeIntent;
use std::collections::BTreeSet;

use super::*;

/// Starts the coder's session, `resumed-coder`, in the work's vessel under harness `harness`.
fn resume_coder_under(case: &Case, harness: &str) {
    let mut request = TerminalLaunchRequest { session: "resumed-coder".into(), ..TerminalsFixture::agent(&case.vessel, "coder", "env") };
    if let TerminalLaunchSource::Agent { adapter, .. } = &mut request.source {
        *adapter = Some(harness.into());
    }
    case.home.launch_session(&request);
}

/// When the coder's session started, as its view reads.
async fn coder_session_started(case: &Case) -> DateTime<Utc> {
    case.backend
        .using::<TerminalSession>("flotilla")
        .list()
        .await
        .expect("sessions")
        .items
        .into_iter()
        .find(|session| session.spec.role == "coder")
        .and_then(|session| session.status?.started_at)
        .expect("the coder's session started")
}

/// The case's work, with its coder at work in `resumed-coder` and no stall: [`idle_nudge_scenario`]
/// with `workflow`, its actor rows cleared.
async fn working_coder(workflow: WorkflowSnapshot) -> Case {
    let case = supervised_case(workflow, &[]).await;
    case.note(|status| status.stalls.clear()).await;
    case.wake.subscriptions.inner.rows.lock().await.clear();
    case
}

/// The work's workflow with its coder owing no promise and no completion condition: its dispatch
/// excludes the decision ledger every agent seat otherwise owes by default. The engine admits only
/// a convoy that owes something, so a second vessel, never provisioned, keeps its auditor's ledger;
/// an unprovisioned vessel's seat arms no row.
fn claim_less_workflow() -> WorkflowSnapshot {
    let mut workflow = supervised_workflow();
    let member = &mut workflow.vessels[0].crew[0];
    member.promises.clear();
    member.completion_conditions.clear();
    member.promise_exclusions = vec![PromiseKind::DecisionLedger];
    workflow.vessels.push(
        flotilla_resources::VesselRequirement::builder()
            .name("audit".into())
            .crew(vec![flotilla_engine_convoys_testkit::agent("auditor")])
            .build(),
    );
    workflow
}

/// Whether any actor row is armed.
async fn actor_armed(case: &Case) -> bool {
    case.wake.subscriptions.inner.rows.lock().await.values().any(|row| matches!(row.maker, LeafMaker::Actor { .. }))
}

/// Rebuilds the work's rows from its view, once the case's views have heard it.
async fn sync_work_rows(case: &Case) {
    let objects = case.objects().await;
    case.caught_up(&case.wake).await;
    case.wake.sync_rows("flotilla", &objects).await.expect("sync rows");
}

/// The coder declares a promise for change request `number` and brings it to `state`: submitted,
/// accepted by verdict, or retracted.
async fn coder_promise(case: &Case, number: u64, state: PromiseState) {
    let engine = case.home.engine();
    let id = &format!("pr-{number}");
    let declare = PromiseOperation::Declare { id: id.into(), kind: PromiseKind::Pr, source: PromiseSource::Crew };
    engine.promise(&case.convoy().await, "work", "coder", &declare, Actor::Crew).await.expect("the coder declares");
    let reference = format!("cr/github.com/team/one/{number}");
    let submitted_at = Utc::now();
    match state {
        PromiseState::Open => {}
        PromiseState::Retracted => {
            let retract = PromiseOperation::Retract { id: id.into(), reason: "not needed".into() };
            engine.promise(&case.convoy().await, "work", "coder", &retract, Actor::Crew).await.expect("the coder retracts");
        }
        PromiseState::Submitted | PromiseState::Kept => {
            let submission =
                Submission { reference: reference.clone(), metadata: BTreeMap::new(), submitted_at: Some(submitted_at), verdict: None };
            let submit = PromiseOperation::Submit { id: id.into(), kind: PromiseKind::Pr, source: PromiseSource::Crew, submission };
            engine.promise(&case.convoy().await, "work", "coder", &submit, Actor::Crew).await.expect("the coder submits");
            if state == PromiseState::Kept {
                let verdict = SubmissionVerdict { accepted: true, who: "forge".into(), at: submitted_at, why: "PR merged".into() };
                let verdict = PromiseOperation::Verdict { id: id.into(), reference, submitted_at: Some(submitted_at), verdict };
                engine.promise(&case.convoy().await, "work", "coder", &verdict, Actor::Observer).await.expect("the forge accepts");
            }
        }
    }
}

/// Records the `review-fix` rule's new turn to the coder, accepted now, as turn delivery notes it.
async fn deliver_review_fix(case: &Case) {
    let accepted_at = Utc::now();
    case.note(|status| {
        flotilla_resources::ConvoyStatusPatch::RecordTurnDelivery {
            source: "review-fix".into(),
            episode: TurnDeliveryEpisode {
                subject_revision: "review-1".into(),
                evidence_at: accepted_at,
                judged_claim_at: accepted_at,
                outcome: TurnDeliveryOutcome::MessageAccepted {
                    new_turn: true,
                    message: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "review-fix"),
                    rung: TurnDeliveryRung::WarmSession,
                    accepted_at,
                },
            },
            vessel: "work".into(),
            role: "coder".into(),
            prompt: "Address the review.".into(),
        }
        .apply(status)
    })
    .await;
}

/// Launches generation `generation` of wheelhouse's governor role at `home`, as `name`, its
/// `govern` vessel crewed by a governor at work; returns the launch. With `ensure`, the launch is
/// a generation of the standing role ensured from it.
pub(super) async fn governor_at(home: &EngineHome, name: &str, ensure: Option<&str>, generation: u64) -> String {
    let spec = ConvoySpec::builder()
        .workflow_ref("workflow".into())
        .project_ref("wheelhouse".into())
        .role("governor".into())
        .generation(generation)
        .build();
    let mut status = single_agent("governor");
    status.workflow_snapshot.as_mut().expect("workflow").vessels[0].name = "govern".into();
    let launch = match ensure {
        Some(ensure) => home.admit_ensured(name, ensure, spec, status).await,
        None => home.admit(name, spec, status).await,
    };
    let vessel = home.vessel(&launch, "govern", "env");
    home.session(&vessel, "governor", &format!("{name}-{generation}-governor"), "env");
    launch
}

// #2560, #3108: the inactivity bound follows the newest tool or output this daemon saw, never
// earlier than it began watching, not total turn age or hooks. Generated ages straddle the exact
// boundary, including future evidence, absent evidence and a restart; another home's turn, whose
// progress is not seen here, is not bounded here.
#[hegel::test]
fn turn_inactivity_tracks_meaningful_evidence(tc: hegel::TestCase) {
    use flotilla_protocol::{AgentActivity, AgentSessionReading};
    use flotilla_resources::agent_activity::{AgentActivityView, CrewProgress};
    use hegel::generators as gs;
    let age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
    let evidence_age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
    let restart_age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
    let tool = tc.draw(gs::booleans());
    let output = tc.draw(gs::booleans());
    let hook = tc.draw(gs::booleans());
    let restarted = tc.draw(gs::booleans());
    let now = Utc::now();
    let start = now - chrono::Duration::seconds(age);
    let evidence = now - chrono::Duration::seconds(evidence_age);
    let watched_from = if restarted { now - chrono::Duration::seconds(restart_age) } else { DateTime::<Utc>::UNIX_EPOCH };
    let obligation =
        NudgeObligation::builder().maker(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() }).leaves(Vec::new()).build();
    let mut agent = AgentActivityView {
        reading: AgentSessionReading {
            ended: None,
            activity: Some(AgentActivity::Working),
            activity_since: Some(start),
            activity_cause: Some(flotilla_resources::agent_activity::SCREEN_CAUSE.into()),
            stale: false,
        },
        current: true,
        stale_at: None,
        progress: Some(CrewProgress {
            watched_from,
            tool_at: tool.then_some(evidence),
            output_at: output.then_some(evidence),
            prompt_submit_at: None,
            hook_at: hook.then_some(evidence),
        }),
    };
    let mut quiet_age = if tool || output { age.min(evidence_age) } else { age };
    if restarted {
        quiet_age = quiet_age.min(restart_age);
    }
    assert_eq!(turn_inactivity_reason(&agent, &obligation, now).is_some(), quiet_age >= 900);
    agent.progress = None;
    assert!(turn_inactivity_reason(&agent, &obligation, now).is_none(), "another home's turn is not bounded here");
}

// #2634: a hook-capable crew with no hook after the first-turn grace gets
// advisory attention. Screen activity does not hide it; a real hook clears it.
// The boundary, pinned with unoccupied attention so generated preservation
// cases cannot hide an off-by-one regression, for both harnesses.
#[tokio::test]
async fn missing_turn_hook_grace_boundary() {
    for claude in [false, true] {
        for (age, other_attention) in [(899, false), (900, false), (901, false), (900, true)] {
            missing_turn_hook_case(age, claude, other_attention).await;
        }
    }
}

#[hegel::test]
fn missing_turn_hook_is_visible_and_recovers(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Ages cross the exact grace boundary; both harnesses, existing unrelated
    // attention, and repeated observations exercise preservation and idempotence.
    let age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
    let claude = tc.draw(gs::booleans());
    let other_attention = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(missing_turn_hook_case(age, claude, other_attention));
}

async fn missing_turn_hook_case(age: i64, claude: bool, other_attention: bool) {
    let case = project_supervision_case(&[]).await;
    case.note(|status| status.stalls.clear()).await;
    resume_coder_under(&case, if claude { "claude-code" } else { "codex" });
    let start = coder_session_started(&case).await;
    let now = start + chrono::Duration::seconds(age);
    let other = ConvoyAttention { source: "settlement".into(), reason: "keep this".into(), raised_at: start };
    if other_attention {
        case.note(|status| status.attention = Some(other.clone())).await;
    }
    for _ in 0..2 {
        observe_actor(&case, &case.wake, TerminalAttentionState::Working, now).await;
        let status = case.status().await;
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
    observe_actor_source(&case, &case.wake, TerminalAttentionState::Idle, TerminalAttentionSource::Hook, hook_at).await;
    assert_eq!(case.status().await.attention, other_attention.then_some(other));
    observe_actor(&case, &case.wake, TerminalAttentionState::Working, hook_at + chrono::Duration::seconds(120)).await;
    assert!(case.status().await.attention.is_none_or(|a| a.source != "missing-turn-hook"));
}

// A real crew stall resolves the supervision topic through an unoccupied
// parent and publishes exactly one intent to the fleet's declared holder.
#[tokio::test]
async fn subscribed_fleet_holder_receives_stall_through_empty_parent() {
    use flotilla_resources::{ProjectSpec, RoleDefinition, RoleSubscription};
    let case = project_supervision_case(&[]).await;
    let projects = case.backend.definitions::<Project>("flotilla");
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
    // The coder declared its stall (the case's own). The fleet's guide is at work.
    assert_eq!(case.status().await.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled);
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("root".into()).role("guide".into()).build();
    // The guide is the standing role's generation: the role's holder.
    let guide = case.home.admit_ensured("fleet-guide", "root-guide", spec, single_agent("guide")).await;
    let vessel = case.home.vessel(&guide, "work", "env");
    case.home.seat(&case.backend, &guide, (&vessel, "work"), ("guide", "guide-terminal"), CrewWorkPhase::Working).await;
    case.judge(&case.wake, Utc::now()).await;
    let requests = case.delivery.requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].convoy, guide);
    assert_eq!(requests[0].role, "guide");
}

#[tokio::test]
async fn claude_stop_flickers_and_nudge_reply_do_not_nudge_active_work() {
    let case = idle_nudge_scenario().await;
    let start = Utc::now();
    for second in (0..900).step_by(30) {
        observe_claude_hook(&case, &case.wake, "pre-tool-use", start + chrono::Duration::seconds(second)).await;
        observe_claude_hook(&case, &case.wake, "post-tool-use", start + chrono::Duration::seconds(second + 1)).await;
        observe_claude_hook(&case, &case.wake, "stop", start + chrono::Duration::seconds(second + 2)).await;
    }
    assert!(case.delivery.requests.lock().expect("deliveries").is_empty(), "per-response Stop is not sustained idle");
    // Finally rest long enough to receive a nudge, then reply to it.
    for second in [900, 960, 1020, 1080] {
        observe_actor(&case, &case.wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(case.delivery.requests.lock().expect("deliveries").len(), 1);
    observe_claude_hook(&case, &case.wake, "user-prompt-submit", start + chrono::Duration::seconds(1081)).await;
    observe_claude_hook(&case, &case.wake, "stop", start + chrono::Duration::seconds(1082)).await;
    assert_eq!(case.delivery.requests.lock().expect("deliveries").len(), 1, "reply Stop must not rearm the nudge budget");
}

// A working crew retains the ordinary settlement obligation, and idles into its nudge; a crew
// whose declared stall waits on its supervisor is not nudged. Exercise the real nudge producer.
#[tokio::test]
async fn declared_stall_waits_on_supervision_while_working_crew_is_nudged() {
    for declared in [false, true] {
        let case = project_supervision_case(&[]).await;
        if !declared {
            case.note(|status| status.stalls.clear()).await;
        }
        resume_coder(&case);
        let start = Utc::now();
        for second in [0, 60, 120, 185] {
            observe_actor(&case, &case.wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        let requests = case.delivery.requests.lock().expect("durable nudge admissions");
        let nudge = requests.iter().find(|request| request.role == "coder");
        assert_eq!(nudge.is_some(), !declared, "declared: {declared}");
        if let Some(request) = nudge {
            assert!(!request.brief.contains("Your stall is recorded"), "{}", request.brief);
        }
    }
}

// #3108: the reply to a nudge is a turn; once it ends, the quiet clock counts from its Stop,
// and continued idle gets the second, backed-off nudge without a replenished budget.
#[tokio::test]
async fn nudge_reply_restarts_idle_clock_and_allows_second_nudge_after_backoff() {
    let case = idle_nudge_scenario().await;
    let (wake, delivery) = (&case.wake, &case.delivery);
    let start = Utc::now();
    for second in [0, 60, 120, 185] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    observe_claude_hook(&case, wake, "user-prompt-submit", start + chrono::Duration::seconds(186)).await;
    for second in [240, 300] {
        observe_actor(&case, wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(second)).await;
    }
    observe_claude_hook(&case, wake, "stop", start + chrono::Duration::seconds(359)).await;
    for second in [360, 420, 480, 538] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "the reply's Stop restarts the quiet clock");
    observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(539)).await;
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2, "continued idle gets the second backed-off nudge");
    assert_eq!(case.status().await.nudge_obligations[0].history.len(), 2, "a reply does not replenish the budget");
}

#[tokio::test]
async fn actual_tool_work_after_nudge_cancels_idle_without_replenishing_budget() {
    let case = idle_nudge_scenario().await;
    let (wake, delivery) = (&case.wake, &case.delivery);
    let start = Utc::now();
    for second in [0, 60, 120, 185] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    observe_claude_hook(&case, wake, "user-prompt-submit", start + chrono::Duration::seconds(181)).await;
    observe_claude_hook(&case, wake, "pre-tool-use", start + chrono::Duration::seconds(182)).await;
    for second in [240, 300] {
        observe_actor(&case, wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(second)).await;
    }
    observe_claude_hook(&case, wake, "post-tool-use", start + chrono::Duration::seconds(358)).await;
    observe_claude_hook(&case, wake, "stop", start + chrono::Duration::seconds(359)).await;
    observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(360)).await;
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "real work requires a new continuous idle grace");
    for second in [420, 480, 539] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2);
    assert_eq!(case.status().await.nudge_obligations[0].history.len(), 2, "commands alone do not reset the obligation budget");
}

#[tokio::test]
async fn genuine_idle_has_obligation_budget_backoff_and_operator_escalation() {
    let case = idle_nudge_scenario().await;
    let (wake, delivery) = (&case.wake, &case.delivery);
    let start = Utc::now();
    // A screen read of idle stands 5 seconds before it counts as quiet.
    for second in [0, 60, 120, 184] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert!(delivery.requests.lock().expect("deliveries").is_empty());
    observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(185)).await;
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    // #2592: even mechanical nudges identify the crew and convoy they concern.
    let identity = format!("coder@work in graphql-budget@wheelhouse (resource ref: {})", case.work);
    let brief = delivery.requests.lock().expect("deliveries")[0].brief.clone();
    assert!(brief.contains(&identity), "{brief}");
    // A brief working flicker cannot replenish the unmet claim's budget.
    observe_actor(&case, wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(186)).await;
    for second in [187, 240, 300, 359] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    for second in [372, 420, 480, 540, 600, 659] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2);
    assert_eq!(case.status().await.stalled.expect("stall").rung, StallRung::Nudge);
    observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(732)).await;
    assert_eq!(case.status().await.stalled.expect("stall").rung, StallRung::Operator);
    observe_actor(&case, wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(733)).await;
    for second in [734, 780, 840, 900, 960, 1020] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2, "working does not reset an obligation budget");
}

#[tokio::test]
async fn operator_message_reply_restarts_clock_without_replenishing_budget() {
    let case = idle_nudge_scenario().await;
    let (wake, delivery) = (&case.wake, &case.delivery);
    let start = Utc::now();
    for second in [0, 60, 120, 185] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    message_to_coder(&case, "owner-guidance", "Keep working on the fix").await;
    for second in [240, 300, 360, 420] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "pending owner guidance is never displaced");
    deliver_to_coder(&case, "owner-guidance", start + chrono::Duration::seconds(421)).await;
    // Judge delivery without replacing its receipt with the scenario helper.
    case.judge(wake, start + chrono::Duration::seconds(421)).await;
    // The guidance is a turn: the agent works on it, then stops at 480. Quiet counts from there.
    observe_actor_source(
        &case,
        wake,
        TerminalAttentionState::Working,
        TerminalAttentionSource::Hook,
        start + chrono::Duration::seconds(425),
    )
    .await;
    for second in [480, 540, 600, 659] {
        observe_actor_source(
            &case,
            wake,
            TerminalAttentionState::Idle,
            TerminalAttentionSource::Hook,
            start + chrono::Duration::seconds(second),
        )
        .await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "the reply's Stop restarts the quiet clock");
    observe_actor_source(&case, wake, TerminalAttentionState::Idle, TerminalAttentionSource::Hook, start + chrono::Duration::seconds(660))
        .await;
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2, "continued idle can receive the second nudge after owner reply");
    assert_eq!(case.status().await.nudge_obligations[0].history.len(), 2);
}

#[tokio::test]
async fn observed_commit_resets_budget_but_refresh_does_not() {
    // The engine holds the actor's checkout for the launch, its HEAD inspected; the work's vessel
    // carries its repository.
    let repo = RepositoryKey("repo".into());
    let mut workflow = supervised_workflow();
    workflow.vessels[0].repository_refs = Some(vec![repo.clone()]);
    let case = supervised_case(workflow, &[]).await;
    case.note(|status| status.stalls.clear()).await;
    resume_coder(&case);
    let checkouts = &case.home.engine.checkouts;
    let launch = checkouts.put_ready(
        "actor-checkout",
        &CheckoutSpec::Observed(
            ObservedCheckoutSpec::builder()
                .r#ref("work".into())
                .path("/workspace".into())
                .repo_ref(repo)
                .host_ref("test-host".into())
                .is_main(false)
                .build(),
        ),
        Some(&CheckoutHolder::Convoy(case.work.clone())),
    );
    let inspection =
        |head: &str| flotilla_resources::checkout_records::Inspection { head_revision: Some(head.into()), ..Default::default() };
    checkouts.checkouts.inspected(&launch, inspection("before")).expect("initial HEAD");
    let (wake, delivery) = (&case.wake, &case.delivery);
    let start = Utc::now();
    for second in [0, 60, 120, 185] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    checkouts.checkouts.inspected(&launch, inspection("before")).expect("same HEAD refresh");
    observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(240)).await;
    assert_eq!(case.status().await.nudge_obligations[0].history.len(), 1);
    checkouts.checkouts.inspected(&launch, inspection("after")).expect("new commit");
    for second in [241, 300, 360, 420, 421] {
        observe_actor(&case, wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    let requests = delivery.requests.lock().expect("deliveries");
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.source == "stall-nudge-1"), "new commit resets the obligation budget");
}

#[tokio::test]
async fn resumed_stalled_crew_is_not_nudged_until_its_briefed_turn_ends() {
    let case = project_supervision_case(&[]).await;
    let resumed_at = Utc::now();
    case.note(|status| {
        external_patches::resume_crew_work(
            "work".into(),
            "coder".into(),
            resumed_at,
            "Continue with the operator's guidance".into(),
            Some("resume-brief".into()),
            ResumeIntent::Reopen,
        )
        .apply(status)
    })
    .await;
    resume_coder(&case);
    message_to_coder(&case, "resume-brief", "Continue with the operator's guidance").await;
    let observe = |state: TerminalAttentionState, source: TerminalAttentionSource, at: DateTime<Utc>, attention: bool| {
        observe_session(&case.home, "resumed-coder", attention.then_some(TerminalAttention { state, as_of: at, source }));
        observe_agent(&case.home, "resumed-coder", state, source, at);
    };
    observe(TerminalAttentionState::Idle, TerminalAttentionSource::Hook, Utc::now(), true);
    case.judge(&case.wake, Utc::now()).await;
    assert!(case.delivery.requests.lock().expect("nudge requests").is_empty());
    deliver_to_coder(&case, "resume-brief", Utc::now()).await;
    observe(TerminalAttentionState::Working, TerminalAttentionSource::Hook, Utc::now(), false);
    case.judge(&case.wake, Utc::now()).await;
    assert!(case.delivery.requests.lock().expect("nudge requests").is_empty());
    let ended_at = Utc::now();
    observe(TerminalAttentionState::Idle, TerminalAttentionSource::Hook, ended_at, true);
    case.judge(&case.wake, Utc::now()).await;
    assert!(case.delivery.requests.lock().expect("nudge requests").is_empty());
    let quiet_at = Utc::now() + chrono::Duration::minutes(3);
    // Fresh screen confirmations preserve quiet duration.
    for second in [120, 60, 0] {
        let observed_at = quiet_at - chrono::Duration::seconds(second);
        observe(TerminalAttentionState::Idle, TerminalAttentionSource::Screen, observed_at, true);
        case.judge(&case.wake, observed_at).await;
    }
    assert_eq!(case.delivery.requests.lock().expect("nudge requests").len(), 1);
}

// #3108: a stall whose governor cannot be reached tells the operator why, once however often it is
// judged: one attention for the stalled seat, naming the failed delivery or the missing governor.
#[tokio::test]
async fn unavailable_governor_tells_the_operator_once() {
    for unavailable in GovernorUnavailable::ALL {
        let case = project_supervision_case(&[]).await;
        // The stalled convoy's project is defined, so its operator inbox is a person's inbox.
        case.backend
            .definitions::<Project>("flotilla")
            .create(
                &InputMeta::builder().name("wheelhouse".into()).build(),
                &flotilla_resources::ProjectSpec::builder().display_name("wheelhouse".into()).build(),
            )
            .await
            .expect("project");
        let governor = admit_governor(
            &case,
            "governor",
            1,
            if matches!(unavailable, GovernorUnavailable::CrewMissing) { "deputy" } else { "governor" },
        )
        .await;
        match unavailable {
            GovernorUnavailable::DeliveryError => case.delivery.unavailable.store(true, Ordering::SeqCst),
            GovernorUnavailable::NoLiveGeneration => {
                let governor = case.home.view(&case.backend, &governor).await;
                case.home.engine().abandon(&governor, "operator", "abandoned").await.expect("abandon the governor");
            }
            GovernorUnavailable::CrewMissing => {}
        }
        let now = Utc::now();
        for tick in 0..3 {
            case.judge(&case.wake, now + chrono::Duration::seconds(tick)).await;
        }
        let stall = case.status().await.stalled.expect("stall");
        assert_eq!((stall.rung, stall.supervisor.as_ref()), (StallRung::Operator, None), "{unavailable:?}");
        let told = case.backend.attentions().open("flotilla", Some(Sender::StallJudge), None).await.expect("open attentions");
        let [attention] = told.as_slice() else { panic!("{unavailable:?}: one attention for the stalled seat: {told:?}") };
        assert_eq!(attention.subject, Subject::ConvoyRole { convoy: case.work.clone(), role: "coder".into() });
        let why = if matches!(unavailable, GovernorUnavailable::DeliveryError) { "supervisor delivery failed" } else { "no live governor" };
        assert!(attention.reason.contains(why), "{unavailable:?}: {}", attention.reason);
        // Telling the same stall again stores nothing new.
        let fact = flotilla_resources::attentions::Fact::Stalled { convoy: case.work.clone(), role: "coder".into() };
        let again = flotilla_resources::attentions::Attention::new(fact, attention.reason.clone(), stall.began_at);
        case.backend.attentions().raise("flotilla", &again).await.expect("raise again");
        // The generic verb lists each message as the engine stores it, its sender an address.
        let stored = case.backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("stored messages").items;
        let told = stored.iter().filter(|message| message.spec.sender.ends_with("/role:stall-judge")).count();
        assert_eq!(told, 1, "{unavailable:?}");
    }
}

#[tokio::test]
async fn unavailable_governor_recovers_in_one_pass() {
    for unavailable in GovernorUnavailable::ALL {
        unavailable_governor_scenario(unavailable, &[false, true]).await;
    }
}

// A governor retry after an explicit escalation must not revisit the Bosun
// rung that the supervisor already consumed, even across a restart.
#[tokio::test]
async fn unavailable_governor_preserves_consumed_bosun_rung() {
    // The work's bosun is latent, a seat no session holds yet: it is available to supervise.
    let mut workflow = supervised_workflow();
    workflow.vessels[0].crew.push(flotilla_engine_convoys_testkit::agent("bosun"));
    let case = supervised_case(workflow, &[("governor", 1, ConvoyPhase::Active)]).await;
    let delivery = case.delivery.clone();
    let now = Utc::now();
    sync_work_rows(&case).await;
    case.judge(&case.wake, now).await;
    assert_eq!(case.status().await.stalled.expect("stall").rung, StallRung::Bosun);
    // Same transition as crew supervise ... escalate: clear ownership,
    // preserve the consumed index, and restore the original actor as maker.
    case.note(|status| {
        let mut condition = status.stalls.oldest().cloned().expect("stall");
        condition.supervisor = None;
        condition.maker = Some(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() });
        condition.rung = StallRung::Operator;
        status.stalls.set(condition);
    })
    .await;
    delivery.unavailable.store(true, Ordering::SeqCst);
    let mut wake = case.wake.clone();
    for _ in 0..2 {
        wake = case.restarted_wake().await;
        wake.sync_rows("flotilla", &case.objects().await).await.expect("rebuild rows");
        case.judge(&wake, now).await;
        let stalled = case.status().await.stalled.expect("stall");
        assert_eq!(stalled.rung, StallRung::Operator);
        assert_eq!(stalled.supervision_index, Some(0));
        assert!(!stalled.supervision_exhausted);
    }
    delivery.unavailable.store(false, Ordering::SeqCst);
    wake.sync_rows("flotilla", &case.objects().await).await.expect("rows");
    case.judge(&wake, now).await;
    let stalled = case.status().await.stalled.expect("stall");
    assert_eq!(stalled.rung, StallRung::Governor);
    assert_eq!(stalled.supervision_index, Some(1));
    let requests = delivery.requests.lock().expect("deliveries");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].role, "bosun");
    assert_eq!(requests[1].role, "governor");
}

// Two crews that declare before the judge runs each climb their own ladder: each gets its own
// supervision message from the latent bosun, and each escalates to the governor once its own
// bosun row times out. Neither seat's stall replaces or restarts the other's.
#[tokio::test]
async fn two_declared_seats_climb_their_own_ladders() {
    let mut workflow = supervised_workflow();
    workflow.vessels[0].crew.push(flotilla_engine_convoys_testkit::agent("reviewer"));
    // The bosun owes nothing, so no row of its own holds the judge while it is silent.
    let mut bosun = flotilla_engine_convoys_testkit::agent("bosun");
    bosun.promises.clear();
    bosun.completion_conditions.clear();
    bosun.promise_exclusions = vec![PromiseKind::DecisionLedger];
    workflow.vessels[0].crew.push(bosun);
    let case = supervised_case(workflow, &[("governor", 1, ConvoyPhase::Active)]).await;
    let now = Utc::now();
    let work = case.work.clone();
    case.note(|status| {
        let (reason, disposition) = (flotilla_resources::StallReason::Scope, Some(flotilla_resources::StallProposedDisposition::Fail));
        external_patches::mark_crew_stalled(work, "work".into(), "reviewer".into(), now, reason, disposition, "needs scope".into())
            .apply(status)
    })
    .await;
    let stall = |status: &ConvoyStatus, role: &str| status.stalls.about(Some(("work", role))).cloned().expect("the seat's stall");
    // Each seat's stall, as the deliveries to `role` name it with its evidence; each delivery is
    // its own message.
    let delivered_to = |role: &str| {
        let requests = case.delivery.requests.lock().expect("deliveries");
        let requests = requests.iter().filter(|request| request.role == role).collect::<Vec<_>>();
        let sources = requests.iter().map(|request| request.source.as_str()).collect::<BTreeSet<_>>();
        assert_eq!(sources.len(), requests.len(), "one message source per seat: {sources:?}");
        requests
            .iter()
            .map(|request| request.stall_of.clone().expect("a stall's supervision"))
            .map(|stall| (stall.about.clone(), stall))
            .collect::<BTreeMap<_, _>>()
    };
    // Each seat's escalation carries its own crew's declaration: its report, reason and proposal.
    let seats = BTreeMap::from([
        (
            format!("wheelhouse/{}/work/coder", case.work),
            flotilla_messaging::StallOf {
                about: format!("wheelhouse/{}/work/coder", case.work),
                report: Some("needs decision".into()),
                reason: Some(flotilla_resources::StallReason::Other),
                proposed_disposition: None,
            },
        ),
        (
            format!("wheelhouse/{}/work/reviewer", case.work),
            flotilla_messaging::StallOf {
                about: format!("wheelhouse/{}/work/reviewer", case.work),
                report: Some("needs scope".into()),
                reason: Some(flotilla_resources::StallReason::Scope),
                proposed_disposition: Some(flotilla_resources::StallProposedDisposition::Fail),
            },
        ),
    ]);
    sync_work_rows(&case).await;
    case.judge(&case.wake, now).await;
    let first = case.status().await;
    for role in ["coder", "reviewer"] {
        assert_eq!(stall(&first, role).rung, StallRung::Bosun, "{role}");
    }
    let messages = |status: &ConvoyStatus| ["coder", "reviewer"].map(|role| stall(status, role).supervision_message.expect("its message"));
    let [coder, reviewer] = messages(&first);
    assert_ne!(coder, reviewer, "each seat has its own supervision message");
    assert_eq!(delivered_to("bosun"), seats);
    assert_eq!(case.delivery.requests.lock().expect("deliveries").len(), 2);

    // Before the bosun rows time out, both stalls wait on the bosun.
    sync_work_rows(&case).await;
    case.judge(&case.wake, now + chrono::Duration::seconds(3)).await;
    assert_eq!(case.status().await.stalls, first.stalls);
    assert_eq!(case.delivery.requests.lock().expect("deliveries").len(), 2);

    // Each seat's bosun row times out: each climbs to the governor with its own message.
    case.judge(&case.wake, now + chrono::Duration::seconds(6)).await;
    let second = case.status().await;
    for role in ["coder", "reviewer"] {
        let climbed = stall(&second, role);
        assert_eq!(climbed.rung, StallRung::Governor, "{role}");
        assert_eq!(climbed.began_at, stall(&first, role).began_at, "{role} keeps its episode");
        assert_eq!(climbed.supervisor.expect("its governor").convoy, case.governor("governor"));
    }
    let [coder, reviewer] = messages(&second);
    assert_ne!(coder, reviewer);
    assert_eq!(delivered_to("governor"), seats);
    assert_eq!(case.delivery.requests.lock().expect("deliveries").len(), 4);
}

// Two unable rows at once, about two seats, stall both: each seat's stall is judged from its own
// row and evidence, and a later tick replaces neither.
#[tokio::test]
async fn two_unable_rows_stall_both_seats() {
    let now = Utc::now();
    let case = coder_beside_governor(false, Some(TerminalAttentionState::NeedsInput), now).await;
    observe_actor_source(&case, &case.wake, TerminalAttentionState::NeedsInput, TerminalAttentionSource::Hook, now).await;
    let status = case.status().await;
    let coder = status.stalls.about(Some(("work", "coder"))).cloned().expect("the coder's stall");
    let governor = status.stalls.about(Some(("supervise", "governor"))).cloned().expect("the governor's stall");
    assert_eq!(coder.source, StallEvidenceSource::Hook);
    assert_eq!(governor.source, StallEvidenceSource::Hook);
    assert_eq!(coder.evidence, "NeedsInput");
    assert_eq!(governor.evidence, "NeedsInput");
    // The coder's stall goes up to its supervisor, who is stalled on its own ladder.
    let supervisor = coder.supervisor.as_ref().expect("the coder's supervisor");
    assert_eq!((supervisor.vessel.as_str(), supervisor.role.as_str()), ("supervise", "governor"));
    assert_eq!(status.stalls.iter().count(), 2, "{:?}", status.stalls);
    let delivered = case.delivery.requests.lock().expect("deliveries").len();
    sync_work_rows(&case).await;
    case.judge(&case.wake, now + chrono::Duration::seconds(1)).await;
    assert_eq!(case.delivery.requests.lock().expect("deliveries").len(), delivered, "neither stall is escalated again");
    let later = case.status().await;
    assert_eq!(later.stalls.about(Some(("work", "coder"))).map(|stall| stall.began_at), Some(coder.began_at));
    assert_eq!(later.stalls.about(Some(("supervise", "governor"))).map(|stall| stall.began_at), Some(governor.began_at));
}

// Explicit escalation by the last governor consumes the whole policy.
// Legacy cursor recovery must not send the stall back to that governor.
#[tokio::test]
async fn governor_escalation_exhausts_default_policy_once() {
    let case = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    case.judge(&case.wake, Utc::now()).await;
    case.note(|status| {
        let mut condition = status.stalls.oldest().cloned().expect("stall");
        condition.supervisor = None;
        condition.maker = Some(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() });
        condition.rung = StallRung::Operator;
        status.stalls.set(condition);
    })
    .await;
    for _ in 0..2 {
        case.judge(&case.wake, Utc::now()).await;
        let stalled = case.status().await.stalled.expect("stall");
        assert_eq!(stalled.rung, StallRung::Operator);
        assert!(stalled.supervision_exhausted);
        assert_eq!(stalled.supervision_index, Some(2));
        assert!(stalled.supervisor.is_none());
        assert_eq!(case.delivery.requests.lock().expect("deliveries").len(), 1);
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
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(unavailable_governor_scenario(unavailable, &restarts));
}

#[tokio::test]
async fn stalled_work_routes_to_live_governor_after_abandoned_generation() {
    let case = project_supervision_case(&[("governor-one", 1, ConvoyPhase::Abandoned), ("governor-two", 2, ConvoyPhase::Active)]).await;
    case.judge(&case.wake, Utc::now()).await;
    let stalled = case.status().await.stalled.expect("stalled");
    assert_eq!(stalled.supervisor.expect("supervisor").convoy, case.governor("governor-two"));
    assert_eq!(case.delivery.requests.lock().expect("deliveries")[0].convoy, case.governor("governor-two"));
}

// A governor homed on another daemon must receive the escalation in the same
// pass that assigns its rung; replica visibility alone is not delivery.
#[tokio::test]
async fn replicated_governor_receives_stall_in_one_pass() {
    let case = project_supervision_case(&[]).await;
    // The governors are another daemon's, heard here as its log arrives; the older one is the
    // standing role's generation.
    let remote_backend = ResourceBackend::new(InMemoryBackend::default());
    let remote = EngineHome::install(&remote_backend, "flotilla", "remote");
    let governor = governor_at(&remote, "governor", Some("wheelhouse-governor"), 1).await;
    governor_at(&remote, "newer-unowned-governor", None, 2).await;
    flotilla_provisioning_testkit::replicate_home(remote.service(), "remote", case.home.service());
    case.judge(&case.wake, Utc::now()).await;
    assert_eq!(case.status().await.stalled.expect("stall").supervisor.expect("supervisor").convoy, governor);
    let requests = case.delivery.requests.lock().expect("deliveries");
    assert_eq!(requests.len(), 1, "remote governor must receive its escalation");
    assert_eq!(requests[0].convoy, governor);
    // Escalations retain the fully-qualified source crew, including its convoy.
    let coder = super::super::Party::Seat { project: "wheelhouse".into(), convoy: case.work.clone(), role: "coder".into() };
    assert_eq!(requests[0].sender, coder);
    assert_eq!(requests[0].expectation, flotilla_messaging::IntentExpectation::Reply);
}

// Supervision commands must preserve identifiers containing shell metacharacters.
// Formatting glue: spaces and an apostrophe exercise the shared shell quoting helper.
#[tokio::test]
async fn stall_supervision_commands_quote_role_and_vessel_identifiers() {
    let case = project_supervision_case(&[]).await;
    case.judge(&case.wake, Utc::now()).await;
    let source = case.convoy().await;
    let mut condition = source.status.as_ref().expect("status").stalled.clone().expect("stall");
    let leaf = condition.leaves.first_mut().expect("actor leaf");
    let LeafAddress::Work { work, .. } = &mut leaf.address else { panic!("work leaf") };
    *work = "work space".to_string();
    leaf.field_path = ".crew.coder's role.phase".to_string();
    let brief = stall_supervision_brief(&source, &condition);
    for action in ["resume", "convert-to-failed", "escalate"] {
        let command = format!(r"--convoy '{}' --vessel 'work space' --role 'coder'\''s role' {action}", case.work);
        assert!(brief.contains(&command), "{brief}");
    }
}

// #2592: a non-actor stall retains convoy identity and evidence, without
// fabricating role/vessel supervision commands for an unknown actor.
#[tokio::test]
async fn unattributed_stall_brief_preserves_identity_without_inventing_commands() {
    let case = project_supervision_case(&[]).await;
    case.judge(&case.wake, Utc::now()).await;
    let source = case.convoy().await;
    let mut condition = source.status.as_ref().expect("status").stalled.clone().expect("stall");
    condition.leaves.clear();
    let brief = stall_supervision_brief(&source, &condition);
    let identity = format!("unidentified crew in convoy graphql-budget@wheelhouse (resource ref: {})", case.work);
    assert!(brief.contains(&identity), "{brief}");
    // The coder declared its stall without a reason.
    assert!(brief.contains("Reason: other. Evidence: needs decision"), "{brief}");
    assert!(!brief.contains("flotilla crew supervise"), "{brief}");
}

#[tokio::test]
async fn stalled_work_without_live_governor_names_reason_at_operator_rung() {
    let case = project_supervision_case(&[("governor-one", 1, ConvoyPhase::Abandoned)]).await;
    case.judge(&case.wake, Utc::now()).await;
    let stalled = case.status().await.stalled.expect("stalled");
    assert_eq!(stalled.rung, StallRung::Operator);
    assert!(!stalled.supervision_exhausted);
    // #2592: the operator backstop uses the same actionable description as a supervisor turn.
    let brief = stall_supervision_brief(&case.convoy().await, &stalled);
    let identity = format!("coder@work in convoy graphql-budget@wheelhouse (resource ref: {})", case.work);
    assert!(brief.contains(&identity), "{brief}");
    assert!(brief.contains(&format!("--convoy '{}' --vessel 'work' --role 'coder' resume", case.work)), "{brief}");
    assert!(stalled.evidence.contains("no live governor for project wheelhouse"), "{}", stalled.evidence);
    assert!(case.delivery.requests.lock().expect("deliveries").is_empty());
}

#[tokio::test]
async fn stalled_work_prefers_governor_owned_by_ensure_over_higher_live_generation() {
    let case = project_supervision_case(&[("governor-three", 3, ConvoyPhase::Active)]).await;
    // The standing role's generation is older than an unensured governor at work.
    let owned = governor_at(&case.home, "governor-two", Some("wheelhouse-governor"), 2).await;
    case.judge(&case.wake, Utc::now()).await;
    let stalled = case.status().await.stalled.expect("stalled");
    assert_eq!(stalled.supervisor.expect("supervisor").convoy, owned);
    assert_eq!(case.delivery.requests.lock().expect("deliveries")[0].convoy, owned);
}

#[tokio::test]
async fn stalled_work_names_missing_governor_crew_at_operator_rung() {
    let case = project_supervision_case(&[]).await;
    // A governor whose workflow crews no governor seat is a governor without its crew.
    admit_governor(&case, "governor-two", 2, "deputy").await;
    case.judge(&case.wake, Utc::now()).await;
    let stalled = case.status().await.stalled.expect("stalled");
    assert_eq!(stalled.rung, StallRung::Operator);
    assert!(stalled.evidence.contains("no live governor crew for project wheelhouse"), "{}", stalled.evidence);
    assert!(case.delivery.requests.lock().expect("deliveries").is_empty());
}

#[tokio::test]
async fn stalled_work_ignores_terminal_ensure_attempt_and_uses_highest_live_generation() {
    let case = project_supervision_case(&[("governor-two", 2, ConvoyPhase::Active), ("governor-three", 3, ConvoyPhase::Active)]).await;
    // The standing role's only generation has ended.
    let ended = governor_at(&case.home, "governor-one", Some("wheelhouse-governor"), 1).await;
    case.home.end(&case.backend, &ended, ConvoyPhase::Abandoned).await;
    case.judge(&case.wake, Utc::now()).await;
    let stalled = case.status().await.stalled.expect("stalled");
    assert_eq!(stalled.supervisor.expect("supervisor").convoy, case.governor("governor-three"));
    assert_eq!(case.delivery.requests.lock().expect("deliveries")[0].convoy, case.governor("governor-three"));
}

// A busy screen gone stale is no evidence the crew is dead. (A session that ended is the seat's
// Interrupted, which `interrupted_stalls` covers.)
#[tokio::test]
async fn stale_busy_screen_does_not_stall() {
    let case = idle_nudge_scenario().await;
    let stale = Utc::now() - TerminalAttention::FRESH_FOR - chrono::Duration::seconds(1);
    observe_session(
        &case.home,
        "resumed-coder",
        Some(TerminalAttention { state: TerminalAttentionState::Working, source: TerminalAttentionSource::Screen, as_of: stale }),
    );
    case.judge(&case.wake, Utc::now()).await;
    assert!(case.status().await.stalled.is_none());
}

#[tokio::test]
async fn clone_controller_rows_judge_transient_terminal_and_exhausted_failures() {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let launch = home.admit("delivery", convoy_spec(), single_agent("coder")).await;
    home.vessel(&launch, "work", "delivery-env");
    let repository = flotilla_resources::RepositorySpec::remote("https://example.org/example/repo").expect("repository");
    let clone_spec = flotilla_resources::CloneSpec {
        repo_ref: repository.key(),
        url: "https://example.org/example/repo".into(),
        env_ref: "delivery-env".into(),
        path: "/tmp/clone".into(),
    };
    let clone = flotilla_resources::clone_records::clone_name(&repository, "delivery-env").expect("clone name");
    // The clones are the engine's (#3108): the worktree, held for the launch, names its clone.
    home.engine.checkouts.put_ready(
        "delivery-checkout",
        &CheckoutSpec::Worktree(flotilla_resources::CheckoutWorktreeSpec {
            repo_ref: repository.key(),
            env_ref: "delivery-env".into(),
            r#ref: "main".into(),
            base_ref: None,
            target_path: "/tmp/checkout".into(),
            // Named by its clone, the checkout reads the clone's latest launch.
            clone_ref: clone.clone(),
        }),
        Some(&CheckoutHolder::Convoy(launch.clone())),
    );
    let clones = &home.engine.clones;
    let first = clones.request(&repository, &clone_spec, false);
    let judge_clone = || async {
        let convoy = home.view(&backend, &launch).await;
        caught_up(&wake, &convoy).await;
        let objects = HashMap::from([(launch.clone(), convoy)]);
        wake.sync_rows("flotilla", &objects).await.expect("arm clone controller row");
        wake.judge_stalls("flotilla", &objects).await.expect("judge clone controller row");
        home.view(&backend, &launch).await.status.expect("status").stalled
    };
    // A transient failure retries without stalling.
    clones.retry(&first, "network unreachable");
    assert_eq!(judge_clone().await.as_ref().map(|stalled| stalled.evidence.as_str()), None);
    // A clone that gives up stalls with its cause.
    clones.finish(&first, flotilla_provisioning_testkit::Ending::GaveUp("repository not found"));
    assert_eq!(judge_clone().await.as_ref().map(|stalled| stalled.evidence.as_str()), Some("repository not found"));
    // Its next launch, its attempts spent, gives up in the same step (the Clone policy's three
    // tries) and stalls with the last failure's cause. A launch read between spending its attempts
    // and giving up stalls as retrying without progress (`wake::clone_retry_tests`).
    let second = clones.request(&repository, &clone_spec, false);
    assert_ne!(second, first, "a clone that gave up is requested again as its next launch");
    let clone_row = || async {
        wake.subscriptions
            .rows()
            .await
            .into_iter()
            .find(|row| matches!(&row.maker, LeafMaker::Controller { resource_kind, .. } if resource_kind == "Clone"))
            .map(|row| row.id)
    };
    let mut standing_row_id = None;
    for _ in 0..2 {
        clones.retry(&second, "network unreachable");
        assert_eq!(judge_clone().await.as_ref().map(|stalled| stalled.evidence.as_str()), None, "retries short of the policy");
        let id = clone_row().await.expect("clone controller row");
        assert!(
            standing_row_id.is_none_or(|standing| standing == id),
            "retry changes update the standing row without reopening its watches"
        );
        standing_row_id = Some(id);
    }
    clones.retry(&second, "network unreachable");
    assert_eq!(judge_clone().await.as_ref().map(|stalled| stalled.evidence.as_str()), Some("network unreachable"));
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
    let (backend, home) = engine_backend().await;
    let now = Utc::now();
    let retry_at = now + chrono::Duration::seconds(60);
    let error = {
        let limit =
            GithubRateLimit { kind: GithubRateLimitKind::Secondary, retry_source: GithubRetrySource::RetryAfter, retry_at: Some(retry_at) };
        limit.observation_error("GraphQL".into())
    };
    let refresher = ChangeRequestRefresher::new(
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
    let table = engine_table(backend.clone(), &home, broadcast_test_sink(event_tx), refresher);
    let launch = launch_at(&backend, &home, "cooldown", ledger_owed(), ConvoyPhase::Landing).await;
    let id = uuid::Uuid::new_v4();
    table.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec!["cr/github.com/team/one/1 .state == merged".parse().expect("exit leaf")],
            watcher: LeafWatcher::ReconcilerWake { convoy: launch.clone() },
            maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
            freshness_demand: Some(now),
            created_at: now,
            episode_key: EpisodeKeyFields::default(),
        },
    );
    let wake = ReconcilerWake::new(table);
    let deadline = retry_at + chrono::Duration::seconds(15);
    for at in [now, retry_at, deadline - chrono::Duration::nanoseconds(1), deadline] {
        let convoy = home.view(&backend, &launch).await;
        caught_up(&wake, &convoy).await;
        wake.judge_stalls_at("flotilla", &HashMap::from([(launch.clone(), convoy)]), at).await.expect("judge");
        let status = home.view(&backend, &launch).await.status.expect("status");
        assert_eq!(status.phase, ConvoyPhase::Landing, "missing merge evidence must never land");
        assert_eq!(status.stalled.is_some(), at >= deadline, "only the expired refresh plus cadence can stall");
    }
}

/// The messages a judgement loads and converts and the convoy records it gets, with `others + 1`
/// local launches each holding a message waiting for its coder.
async fn read_by_one_judgement(others: usize) -> (u64, u64, u64) {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let mut objects = HashMap::new();
    for index in 0..=others {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build();
        let launch = home.admit(&format!("convoy-{index}"), spec, single_agent("coder")).await;
        home.vessel(&launch, "work", "env");
        let convoy = home.view(&backend, &launch).await;
        let seat = flotilla_resources::convoy_crew_seats(&convoy).remove(0);
        let intent = flotilla_messaging::Intent::new(
            flotilla_engine_messages_testkit::party("system:turn-rules"),
            flotilla_resources::convoy::engine::role_inbox(&seat).expect("the seat's inbox"),
            flotilla_resources::MessageRelation::System,
            format!("turn {index}"),
        );
        home.messages.primitives().admit(&format!("turn-{index}"), &intent).await.expect("admit the turn");
        objects.insert(launch, convoy);
    }
    for convoy in objects.values() {
        caught_up(&wake, convoy).await;
    }
    let now = Utc::now();
    wake.judge_stalls_at("flotilla", &objects, now).await.expect("bootstrap");
    let before = home.convoys.keyed_gets().convoys;
    let (judged, reads) = flotilla_store::load_metrics::measure(wake.judge_stalls_at("flotilla", &objects, now)).await;
    judged.expect("judgement");
    let messages = reads.get("Message").copied().unwrap_or_default();
    (messages.decoded, messages.converted, home.convoys.keyed_gets().convoys - before)
}

// Behaviour: a stall judgement reads each crew seat's messages as the engine holds them: it
// loads each seat's message once, converts none to the old shape, and gets no convoy record to
// spell their receivers.
#[tokio::test]
async fn a_judgement_reads_seat_messages_without_converting_them() {
    assert_eq!(read_by_one_judgement(5).await, (6, 0, 0), "six seats' messages are loaded, and none converted");
    assert_eq!(read_by_one_judgement(60).await, (61, 0, 0), "sixty-one seats' messages are loaded, and none converted");
}

// #3044: populated kinds are bootstrapped once; repeated no-op judgements perform zero
// backing-store reads, including indirect supervisor routing reads. (The read counter wraps the
// resource backend: the launches' engine reads are not counted, the Project and Message reads
// of routing are.)
#[tokio::test]
async fn steady_state_stall_judgements_do_not_read_the_store() {
    use flotilla_store_testkit::ReadCountsBackendExt;
    let (backend, home) = engine_backend().await;
    let counted = backend.clone().with_read_counts();
    let wake = supervision_wake(&counted, &home);
    backend
        .definitions::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("project".into()).build(),
            &flotilla_resources::ProjectSpec::builder().display_name("Project".into()).build(),
        )
        .await
        .expect("project");
    let mut objects = HashMap::new();
    for index in 0..32 {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("project".into()).build();
        let launch = home.admit(&format!("convoy-{index}"), spec, single_agent("coder")).await;
        home.vessel(&launch, "work", "env");
        objects.insert(launch.clone(), home.view(&backend, &launch).await);
    }
    for convoy in objects.values() {
        caught_up(&wake, convoy).await;
    }
    let now = Utc::now();
    wake.judge_stalls_at("flotilla", &objects, now).await.expect("bootstrap");
    let before = counted.read_snapshot();
    for _ in 0..64 {
        wake.judge_stalls_at("flotilla", &objects, now).await.expect("steady state judgement");
    }
    let reads = counted.read_snapshot().since(&before);
    reads.assert_zero("stall judgement including nested routing helpers");
}

// #3045: the leaf engine's steady state is served by watch views. After one
// bootstrap pass, reconcile passes that observe convoy changes delivered by
// watches read no store, on any task: views refresh from events.
#[tokio::test]
async fn leaf_engine_steady_state_reconcile_reads_no_store() {
    use flotilla_store_testkit::ReadCountsBackendExt;
    let (backend, home) = engine_backend().await;
    let counted = backend.clone().with_read_counts();
    let wake = supervision_wake(&counted, &home);
    backend
        .definitions::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("project".into()).build(),
            &flotilla_resources::ProjectSpec::builder().display_name("Project".into()).build(),
        )
        .await
        .expect("project");
    let mut launches = Vec::new();
    for index in 0..8 {
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("project".into()).build();
        let launch = home.admit(&format!("convoy-{index}"), spec, single_agent("coder")).await;
        home.vessel(&launch, "work", "env");
        launches.push(launch);
    }
    wake.subscriptions.reconcile_stalls_once("flotilla").await.expect("bootstrap pass");
    let bootstrap = counted.read_snapshot();
    assert!(!bootstrap.lists().is_empty(), "the bootstrap pass builds its views from lists\n{bootstrap}");
    for index in 0..16 {
        // A change to a launch reaches the views as an event, not a relist.
        let launch = &launches[index % 8];
        touch(&backend, &home, launch).await;
        let convoy = home.view(&backend, launch).await;
        caught_up(&wake, &convoy).await;
        wake.subscriptions.reconcile_stalls_once("flotilla").await.expect("steady state pass");
        let resident = wake.local_convoys("flotilla").await.expect("resident convoys");
        let scanned = |convoy: &ResourceObject<Convoy>| convoy.status.as_ref().expect("status").branch_subject_scan_at;
        assert_eq!(scanned(&resident[launch]), scanned(&convoy));
    }
    counted.read_snapshot().since(&bootstrap).assert_zero("leaf engine reconcile passes");
}

// #3118: one declared stall episode yields one receiver-homed Message even
// when the crew repeats its report and the judge restarts between passes.
async fn stall_message_scenario(repeat_report: bool, restart: bool) {
    use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
    use flotilla_resources::StallReason;
    struct InboxDelivery(EngineMessagesHome);
    // Boundary stand-in for turn delivery; persist through the real Message path.
    #[async_trait]
    impl TurnDeliveryActuator for InboxDelivery {
        async fn deliver(&self, request: &CrewTurnIntent) -> Result<CrewTurnAdmission, String> {
            let receiver = request.receiver.clone().unwrap_or_else(|| super::super::Party::Seat {
                project: "wheelhouse".into(),
                convoy: request.convoy.clone(),
                role: request.role.clone(),
            });
            let messages: &dyn flotilla_messaging::EngineMessages = self.0.primitives().as_ref();
            let inbox = receiver.inbox(messages, "flotilla", "wheelhouse").await?;
            let sender = request.sender.inbox(messages, "flotilla", "wheelhouse").await?;
            let name = flotilla_resources::message_record_name(
                inbox.as_str(),
                sender.as_str(),
                &turn_message_producer_key(&request.source, &request.subject_revision),
            );
            let intent = crate::crew_ops::turn_intent(request, sender, inbox);
            let object = messages.admit(&name, &intent).await.map_err(|error| error.to_string())?;
            Ok(CrewTurnAdmission {
                new_turn: true,
                rung: TurnDeliveryRung::WarmSession,
                message: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", object.record),
            })
        }
        async fn hold(&self, _: &CrewTurnIntent, _: &HoldAct, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let case = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    designate_fleet(&case.backend, "flotilla").await;
    let delivery = Arc::new(InboxDelivery(EngineMessagesHome::open("home", "flotilla", &case.backend)));
    let now = Utc::now();
    let stale_objects = case.objects().await;
    let mut wake = case.wake.clone();
    for step in 0..3 {
        if repeat_report {
            let work = case.work.clone();
            case.note(|status| {
                external_patches::mark_crew_stalled(
                    work,
                    "work".into(),
                    "coder".into(),
                    now + chrono::Duration::seconds(step),
                    StallReason::Scope,
                    None,
                    "needs decision".into(),
                )
                .apply(status)
            })
            .await;
        }
        if restart {
            wake = case.restarted_wake().await;
        }
        wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
        let objects = if repeat_report { case.objects().await } else { stale_objects.clone() };
        case.caught_up(&wake).await;
        wake.sync_rows("flotilla", &objects).await.expect("rows");
        wake.judge_stalls_at("flotilla", &objects, now + chrono::Duration::seconds(step)).await.expect("judge");
        let messages = delivery.0.views().await.expect("inbox");
        assert_eq!(messages.len(), 1, "one episode must not create a burst of copies");
    }
    // Catching up the outer watch rebuilds rows and continues judging normally.
    let current = case.objects().await;
    case.caught_up(&wake).await;
    wake.sync_rows("flotilla", &current).await.expect("current rows");
    wake.judge_stalls_at("flotilla", &current, now + chrono::Duration::seconds(4)).await.expect("current judge");
    assert_eq!(delivery.0.views().await.expect("inbox").len(), 1);
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

// Only the launch's home may judge it. A caller map holding another home's launch, with a stall
// in it, must not actuate it here or take it over.
#[tokio::test]
async fn replica_only_convoy_in_caller_snapshot_is_not_judged() {
    let case = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let remote_backend = ResourceBackend::new(InMemoryBackend::default());
    let remote = EngineHome::install(&remote_backend, "flotilla", "remote");
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("wheelhouse".into()).build();
    let launch =
        remote.admit("replicated-work", spec, ConvoyStatus { workflow_snapshot: Some(supervised_workflow()), ..Default::default() }).await;
    remote.vessel(&launch, "work", "env");
    flotilla_provisioning_testkit::replicate_home(remote.service(), "remote", case.home.service());
    let replicas = case.backend.including_replicas::<Convoy>("flotilla");
    let visible = replicas.get(&launch).await.expect("visible replica");
    assert!(matches!(visible.provenance, flotilla_resources::ResourceProvenance::Replica { .. }));
    let mut source = visible.object.clone();
    external_patches::mark_crew_stalled(
        launch.clone(),
        "work".into(),
        "coder".into(),
        Utc::now(),
        flotilla_resources::StallReason::Other,
        None,
        "needs decision".into(),
    )
    .apply(source.status.as_mut().expect("status"));
    let objects = HashMap::from([(launch.clone(), source)]);
    case.wake.sync_rows("flotilla", &objects).await.expect("old rows");
    case.wake.judge_stalls_at("flotilla", &objects, Utc::now()).await.expect("judge obsolete caller map");
    assert!(case.delivery.requests.lock().expect("requests").is_empty(), "a replica must never emit a supervision turn");
    let heard = replicas.get(&launch).await.expect("visible replica");
    assert!(heard.object.status.expect("status").stalled.is_none(), "judging must not note a stall on another home's launch");
}

// #3439: a handoff is an outstanding report even without a declared claim.
// A Pending (available) role owes nothing and must remain exempt.
#[tokio::test]
async fn claim_less_working_crew_stalls_with_actor_evidence() {
    for (phase, attention) in [
        (CrewWorkPhase::Working, TerminalAttentionState::Idle),
        (CrewWorkPhase::Working, TerminalAttentionState::Working),
        (CrewWorkPhase::Pending, TerminalAttentionState::Idle),
    ] {
        let case = working_coder(claim_less_workflow()).await;
        // The coder handed its follow-up task to itself: it has a task.
        case.note(|status| status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew").task_assigned = true).await;
        case.home.seat(&case.backend, &case.work, (&case.vessel, "work"), ("coder", "resumed-coder"), phase).await;
        sync_work_rows(&case).await;
        let now = Utc::now();
        for second in [0, 60, 120, 180, 900, 1800] {
            let at = now + chrono::Duration::seconds(second);
            if phase == CrewWorkPhase::Pending {
                case.judge(&case.wake, at).await;
            } else {
                observe_actor(&case, &case.wake, attention, at).await;
            }
            if second == 0 || (attention == TerminalAttentionState::Working && second < 900) {
                assert!(case.status().await.stalled.is_none(), "fresh work and the idle grace must remain able/unknown");
            }
        }
        let status = case.status().await;
        assert_eq!(status.stalled.is_some(), phase == CrewWorkPhase::Working, "{phase:?}: {status:?}");
        if let Some(stalled) = status.stalled {
            assert!(!stalled.evidence.is_empty());
            assert!(!stalled.leaves.is_empty());
            if attention == TerminalAttentionState::Working {
                assert!(stalled.evidence.contains("turn inactivity bound exceeded"));
            } else {
                assert!(!case.delivery.requests.lock().expect("deliveries").is_empty(), "idle work follows the normal nudge ladder");
            }
        }
    }
}

// Launching a claim-less role makes it Working, but does not assign
// a task. Even repeated idle observations beyond grace must create no actor row.
#[tokio::test]
async fn freshly_launched_claim_less_role_stays_unjudged() {
    let case = working_coder(claim_less_workflow()).await;
    resume_coder(&case);
    assert_eq!(case.status().await.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    sync_work_rows(&case).await;
    assert!(!actor_armed(&case).await, "launch alone must not arm an actor obligation");
    let now = Utc::now();
    for second in [0, 60, 120, 1800] {
        observe_actor(&case, &case.wake, TerminalAttentionState::Idle, now + chrono::Duration::seconds(second)).await;
        assert!(case.status().await.stalled.is_none());
    }
}

// A crew that declares its own promise owes a report even without a workflow
// claim or follow-up task. Narrowing launch eligibility must retain this case.
#[tokio::test]
async fn a_launched_role_that_declares_a_promise_owes_a_report() {
    let case = working_coder(claim_less_workflow()).await;
    resume_coder(&case);
    coder_promise(&case, 1, PromiseState::Open).await;
    sync_work_rows(&case).await;
    assert!(actor_armed(&case).await);
    let now = Utc::now();
    for second in [0, 60, 120, 180, 900, 1800] {
        observe_actor(&case, &case.wake, TerminalAttentionState::Idle, now + chrono::Duration::seconds(second)).await;
    }
    assert!(case.status().await.stalled.is_some());
}

// Resolving an explicit stall with guidance assigns a report even if the role
// was only launched before stalling. Silence after that resume must be judged.
#[tokio::test]
async fn resumed_explicit_stall_without_claim_still_owes_a_report() {
    let case = working_coder(claim_less_workflow()).await;
    resume_coder(&case);
    let work = case.work.clone();
    case.note(|status| {
        let at = Utc::now();
        flotilla_resources::ConvoyStatusPatch::MarkCrewStalled {
            convoy: work,
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
        assert!(status.crew_work["work"]["coder"].task_assigned);
    })
    .await;
    assert_eq!(case.status().await.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    sync_work_rows(&case).await;
    let now = Utc::now();
    for second in [0, 60, 120, 180, 900, 1800] {
        observe_actor(&case, &case.wake, TerminalAttentionState::Idle, now + chrono::Duration::seconds(second)).await;
    }
    assert!(case.status().await.stalled.is_some());
}

// #3572: standing convoys never owe settlement claims. For ordinary convoys,
// terminal promises alone owe nothing; tasks and completion conditions still do.
// Every combination: duplicate promise counts including an empty history, each
// state and each obligation, so every eligibility boundary is exercised.
#[tokio::test]
async fn settlement_claim_arming_respects_exit_and_pending_promises() {
    for count in 0..=3usize {
        for standing in [false, true] {
            for state in [PromiseState::Open, PromiseState::Submitted, PromiseState::Kept, PromiseState::Retracted] {
                for (task, completion) in [(false, false), (true, false), (false, true)] {
                    let mut workflow = claim_less_workflow();
                    workflow.exit = (!standing).then(ExitDeclaration::standard_table);
                    if completion {
                        workflow.vessels[0].crew[0].completion_conditions =
                            supervised_workflow().vessels[0].crew[0].completion_conditions.clone();
                    }
                    // The task is a review fix a turn rule delivered after the coder's promises.
                    workflow.turn_delivery.insert(
                        "review-fix".into(),
                        TurnDeliveryRule::builder()
                            .on(flotilla_resources::parse_leaf_template("$issue.state == closed").expect("issue rule"))
                            .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".into()).role("coder".into()).build())
                            .brief("Address the review.".into())
                            .hold(HoldAct::State)
                            .build(),
                    );
                    let case = working_coder(workflow).await;
                    resume_coder(&case);
                    for number in 1..=count as u64 {
                        coder_promise(&case, number, state).await;
                    }
                    if task {
                        deliver_review_fix(&case).await;
                    }
                    sync_work_rows(&case).await;
                    let phase = case.status().await.crew_work["work"]["coder"].phase;
                    // On the engine a seat whose promises are all kept or retracted is done, and a
                    // done seat owes nothing whatever its task or completion conditions.
                    let done = phase == CrewWorkPhase::Done;
                    assert!(
                        !done || (count > 0 && matches!(state, PromiseState::Kept | PromiseState::Retracted)),
                        "only settled promises finish the seat: {state:?}, {count}"
                    );
                    // A seat whose every open promise is submitted waits on its kept rules'
                    // observations, not on itself, unless a rule delivered it a turn since: that is
                    // work it owes whatever its promises (run 3 F4).
                    let all_submitted = count > 0 && state == PromiseState::Submitted;
                    let expected =
                        !standing && !done && (task || (!all_submitted && (completion || (count > 0 && state == PromiseState::Open))));
                    let armed = actor_armed(&case).await;
                    assert_eq!(
                        armed, expected,
                        "standing={standing}, state={state:?}, count={count}, task={task}, completion={completion}, phase={phase:?}"
                    );
                    if !expected {
                        let now = Utc::now();
                        for second in [0, 60, 120, 1800] {
                            observe_actor(&case, &case.wake, TerminalAttentionState::Idle, now + chrono::Duration::seconds(second)).await;
                        }
                        assert!(case.delivery.requests.lock().unwrap().is_empty(), "no settlement nudge without an obligation");
                        // A submitted PR arms a row on its change request, which stalls on the
                        // observation here because no observer runs: that is the observer's, not
                        // the seat's.
                        let stalled = case.status().await.stalled;
                        assert!(
                            stalled.as_ref().is_none_or(|stalled| stalled.source == StallEvidenceSource::Observation),
                            "standing={standing}, state={state:?}, count={count}, task={task}, completion={completion}, phase={phase:?}: {stalled:?}"
                        );
                    }
                }
            }
        }
    }
}

// #3572: explicit stalls do not make standing roles owe settlement claims.
#[tokio::test]
async fn standing_stalled_role_has_no_settlement_obligation() {
    let mut workflow = supervised_workflow();
    workflow.exit = None;
    let case = supervised_case(workflow, &[]).await;
    case.wake.subscriptions.inner.rows.lock().await.clear();
    case.note(|status| status.crew_work.get_mut("work").unwrap().get_mut("coder").unwrap().task_assigned = true).await;
    assert_eq!(case.status().await.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled);
    sync_work_rows(&case).await;
    assert!(!actor_armed(&case).await);
}

// Pending launch stays exempt from dead-session stalls. Lifecycle progress buys another grace
// period; a stuck stage raises stable operator attention and recovers when the session runs.
#[hegel::test]
fn pending_launch_progress_is_bounded_and_recovers(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Repeat judgments on either side of the exact 15-minute boundary.
    let repeat = tc.draw(gs::integers::<i64>().min_value(1).max_value(60));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        // The work's second agent is latent: it awaits a handoff, not a launch.
        let mut workflow = supervised_workflow();
        workflow.vessels[0].crew.push(flotilla_engine_convoys_testkit::agent("latent"));
        let case = working_coder(workflow).await;
        // The sessions' log is followed, as a daemon follows its store: the watch view learns
        // of the request and its retries.
        let _follower = case.home.engine.terminals.terminals.follow(Arc::clone(case.home.service())).expect("follow sessions");
        let status = case.status().await;
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Pending);
        let start = status.started_at.expect("the launch started");
        let judge = |seconds: i64| {
            let case = &case;
            async move {
                case.judge(&case.wake, start + chrono::Duration::seconds(seconds)).await;
                case.status().await
            }
        };
        for seconds in [0, 899] {
            let status = judge(seconds).await;
            assert!(status.stalled.is_none());
            assert!(status.attention.is_none());
        }
        // Requesting the session is real launch progress even after a long queue wait.
        let request = TerminalLaunchRequest { session: "resumed-coder".into(), ..TerminalsFixture::agent(&case.vessel, "coder", "env") };
        let cell = case.wake.views("flotilla").await.expect("views");
        let views = cell.get().expect("initialized views");
        let mut changes = views.sessions.subscribe_local_keys();
        let session = case.home.engine.terminals.request(&request);
        // The judge reads its watch view, so it judges once that view has heard the request: a
        // session for the coder's seat on its vessel.
        let heard = || async {
            views.sessions.shared_list().await.expect("session view").items.iter().any(|session| {
                session.object.spec.role == "coder"
                    && session.object.metadata.labels.get(flotilla_resources::VESSEL_LABEL).map(String::as_str) == Some("work")
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !heard().await {
                match changes.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => panic!("the session view stopped"),
                }
            }
        })
        .await
        .expect("the judge's view hears the request");
        assert!(judge(899).await.attention.is_none());
        for seconds in [1798] {
            let status = judge(seconds).await;
            assert!(status.stalled.is_none());
            assert!(status.attention.is_none());
        }
        let overdue = judge(1799).await;
        assert!(overdue.stalled.is_none(), "Pending remains startup-unknown");
        let attention = overdue.attention.expect("bounded launch diagnostic");
        assert_eq!(attention.source, "launch-progress");
        assert!(attention.reason.contains("coder@work"), "{}", attention.reason);
        assert!(!attention.reason.contains("latent@work"), "latent roles await handoff");
        let repeated = judge(1799 + repeat).await;
        assert_eq!(repeated.attention, Some(attention.clone()));
        let other = ConvoyAttention { source: "operator".into(), reason: "keep this decision".into(), raised_at: start };
        case.note(|status| status.attention = Some(other.clone())).await;
        assert_eq!(judge(1900).await.attention, Some(other), "launch health must preserve unrelated attention");
        case.note(|status| status.attention = None).await;
        // Retry failures are diagnostic evidence, not successful progress: a launch that is
        // retrying shows its reading's reason, as the session view does.
        case.home.engine.terminals.retry(&session, "launcher retry failed");
        assert!(judge(1901).await.attention.expect("retry stays overdue").reason.contains("launcher retry failed"));
        // The session runs: the coder works, and the launch's attention clears.
        let agent = AgentLaunch { adapter: "claude".into(), model: None, stance: "trusted-implicit".into() };
        case.home.engine.terminals.ready(
            &session,
            "resumed-coder-runtime",
            &TerminalLaunchFacts { agent: Some(agent), ..Default::default() },
        );
        assert_eq!(case.status().await.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
        assert!(judge(1902).await.attention.is_none(), "recovery clears launch attention");
    });
}

// A vessel the engine realises is launch progress: its Ready restarts the launch bound.
#[tokio::test]
async fn engine_vessel_progress_resets_the_launch_bound() {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).role("graphql-budget".into()).project_ref("wheelhouse".into()).build();
    let launch =
        home.admit("stalled-work", spec, ConvoyStatus { workflow_snapshot: Some(supervised_workflow()), ..Default::default() }).await;
    // The work's vessel is requested, not yet realised; no session starts before it is.
    let vessel = home.engine.vessels.request(&launch, "work", "test-host");
    let start = home.view(&backend, &launch).await.status.and_then(|status| status.started_at).expect("the launch started");
    let judge = |at: DateTime<Utc>| {
        let (backend, home, wake, launch) = (&backend, &home, &wake, &launch);
        async move {
            let convoy = home.view(backend, launch).await;
            caught_up(wake, &convoy).await;
            wake.judge_stalls_at("flotilla", &HashMap::from([(launch.clone(), convoy)]), at).await.expect("judge");
            home.view(backend, launch).await.status.expect("status").attention
        }
    };
    assert!(judge(start).await.is_none());
    // The vessels' log is followed, as a daemon follows its store: the watch view learns of Ready.
    let _follower = home.engine.vessels.vessels.follow(Arc::clone(home.service())).expect("follow vessels");
    let cell = wake.views("flotilla").await.expect("views");
    let views = cell.get().expect("initialized views");
    let mut changes = views.vessels.subscribe_local_keys();
    home.engine.vessels.ready(&vessel, "env");
    // The judge reads its watch view, so it judges once that view has heard Ready.
    let ready_at = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let heard = views.vessels.shared_named(&vessel).await.expect("vessel view");
            if let Some(ready_at) = heard.iter().find_map(|read| {
                read.object.status.as_ref().filter(|status| status.phase == flotilla_resources::VesselPhase::Ready)?.ready_at
            }) {
                return ready_at;
            }
            match changes.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("the vessel view stopped"),
            }
        }
    })
    .await
    .expect("the judge's view hears Ready");
    // The engine stamps Ready with the wall clock, as it stamps the launch's start, so the bound
    // counts from the vessel's Ready.
    assert!(ready_at >= start);
    assert!(judge(ready_at).await.is_none());
    assert!(judge(ready_at + chrono::Duration::seconds(899)).await.is_none(), "the engine's Ready restarted the bound");
    assert_eq!(
        judge(ready_at + chrono::Duration::seconds(900)).await.expect("bounded from the engine's progress").source,
        "launch-progress"
    );
}

// Supervision has a crew or convoy subject, and only a recipient with an inbox awaits a reply.
#[tokio::test]
async fn supervision_records_have_a_subject_and_operator_notices_expect_nothing() {
    let case = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    case.judge(&case.wake, Utc::now()).await;
    let source = case.convoy().await;
    let mut condition = source.status.as_ref().expect("status").stalled.clone().expect("stall");
    assert_eq!(super::super::stalls::stall_subject(&source, &condition), case.coder());
    condition.leaves.clear();
    let launch = flotilla_resources::convoy::engine::launch_address(&flotilla_address::Address::project("wheelhouse").unwrap(), &case.work)
        .expect("a launch address");
    assert_eq!(super::super::stalls::stall_subject(&source, &condition), launch.to_string());
    assert_eq!(super::super::stalls::supervision_expectation(StallRung::Operator), flotilla_messaging::IntentExpectation::Nothing);
    assert_eq!(super::super::stalls::supervision_expectation(StallRung::Governor), flotilla_messaging::IntentExpectation::Reply);
}
