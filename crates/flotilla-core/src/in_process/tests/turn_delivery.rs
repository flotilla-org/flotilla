use flotilla_store_memory::engine_home_backend;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_protocol::HostName;
use flotilla_resources::{
    terminal_records::{TerminalLaunchRequest, TerminalLaunchSource},
    Convoy as ResourceConvoy, ConvoySpec, CrewWorkPhase, TerminalAttention, TerminalAttentionSource, TerminalAttentionState,
    TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase, TurnDeliveryRung,
};

use super::support::{crew_messages, ensure_vessel, observe_attention, receive_turn, resume_staging_fixture, RecordingWorkCredentials};
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;

// A fresh turn is durable intent independent of the boot brief; it waits for
// the replacement holder's readiness and transport acceptance evidence.
#[tokio::test]
async fn fresh_turn_stores_intent_independently_of_boot_brief() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    // The agent's process exited, ending its launch.
    let terminals = &probe.engine.terminals;
    let exited = terminals.record("resume-staging-session");
    terminals.ready(&exited.name, "resume-staging-pool-session", &Default::default());
    terminals.exited(&exited.name, "exit 0");
    let request = flotilla_crew::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy(probe.convoy.clone())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("fresh turn".to_string())
        .subject_revision("next-head".to_string())
        .sender(flotilla_crew::leaf_engine::Party::fleet("turn-rules"))
        .build();
    daemon.deliver_standing_turn(&request).await.expect("deliver fresh turn");
    let session = sessions.get("resume-staging-session").await.expect("updated session");
    // The session's next launch boots with the same brief; the turn is not written into it.
    let relaunched = terminals.record("resume-staging-session");
    assert_ne!(relaunched.name, exited.name);
    assert_eq!(relaunched.source, exited.source);
    let messages = crew_messages(&daemon, "flotilla").await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.body, "fresh turn");
    assert_eq!(messages[0].spec.sender, "system:turn-rules");
    assert_eq!(messages[0].spec.receiver, format!("flotilla/{}/work/coder", probe.convoy));
    assert_eq!(session.status.expect("status").phase, ResourceTerminalSessionPhase::Starting);
}

#[tokio::test]
async fn repeated_standing_turn_does_not_restart_a_lost_session_after_delivery() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let request = flotilla_crew::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy(probe.convoy.clone())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("same turn".to_string())
        .subject_revision("same-head".to_string())
        .sender(flotilla_crew::leaf_engine::Party::fleet("turn-rules"))
        .build();
    daemon.deliver_standing_turn(&request).await.expect("first intent");
    let message = crew_messages(&daemon, "flotilla").await.remove(0);
    receive_turn(&daemon, &message.spec.receiver, &message.metadata.name, "old-session", Utc::now()).await;
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let terminals = &probe.engine.terminals;
    let lost = terminals.record("resume-staging-session").name;
    terminals.ready(&lost, "resume-staging-pool-session", &Default::default());
    terminals.lost(&lost, "pool lost the session");
    assert_eq!(daemon.deliver_standing_turn(&request).await.expect("duplicate already accepted"), TurnDeliveryRung::FreshAgent);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(crew_messages(&daemon, "flotilla").await.len(), 1);
    assert_eq!(
        sessions.get("resume-staging-session").await.expect("session").status.expect("status").phase,
        ResourceTerminalSessionPhase::Lost
    );
    assert_eq!(terminals.record("resume-staging-session").name, lost, "no next launch was started");
}

#[tokio::test]
async fn turn_delivery_reopens_work_and_stages_credentials_before_queuing_every_rung() {
    for phase in [ResourceTerminalSessionPhase::Running, ResourceTerminalSessionPhase::Starting, ResourceTerminalSessionPhase::Stopped] {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"turn-credential-test\"\n").expect("daemon config");
        let backend = engine_home_backend();
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("test-host"),
            backend.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        // The engine's readings, which a running daemon always has.
        let engine = super::test_engine(&daemon);
        super::support::designate_fleet(&backend).await;
        // The coder's work is kept: the convoy is Landing, its work Complete and its crew Done.
        let mut snapshot = super::support::stall_workflow_snapshot(vec![super::support::claim_crew("coder")]);
        snapshot.vessels[0].credential_refs = BTreeSet::from(["github-crew-pr".to_string()]);
        let convoy = super::admit_snapshot(
            &daemon,
            "turn-credential-work",
            ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("implement-review".to_string()).build(),
            snapshot,
        )
        .await;
        super::complete_work(&daemon, &convoy, "work").await;
        let credentials = Arc::new(RecordingWorkCredentials {
            backend: backend.clone(),
            convoy: convoy.clone(),
            delivered: tokio::sync::Mutex::new(BTreeSet::new()),
            fail_next: std::sync::atomic::AtomicBool::new(true),
        });
        daemon.set_work_credential_reconciler(credentials.clone()).await;
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
        let vessel = ensure_vessel(&engine, &convoy, "work");
        let launch = engine.terminals.request(&TerminalLaunchRequest {
            session: "turn-credential-session".to_string(),
            environment: "credential-env".to_string(),
            vessel: Some(vessel),
            role: "coder".to_string(),
            pool: "passthrough".to_string(),
            cwd: "/repo".to_string(),
            env: Default::default(),
            source: TerminalLaunchSource::Agent {
                capability: "code".to_string(),
                adapter: None,
                model: None,
                brief: flotilla_resources::TerminalBrief {
                    artifact_digest: None,
                    path: "brief.md".to_string(),
                    content: "original".to_string(),
                    copies: vec![],
                },
            },
        });
        match phase {
            ResourceTerminalSessionPhase::Running => engine.terminals.ready(&launch, "pool-session", &Default::default()),
            ResourceTerminalSessionPhase::Stopped => {
                engine.terminals.ready(&launch, "pool-session", &Default::default());
                engine.terminals.exited(&launch, "exit 0");
            }
            _ => {}
        }
        let request = flotilla_crew::leaf_engine::CrewTurnIntent::builder()
            .namespace("flotilla".to_string())
            .convoy(convoy.clone())
            .source("conflicting".to_string())
            .vessel("work".to_string())
            .role("coder".to_string())
            .brief("rebase the PR".to_string())
            .subject_revision("new-head".to_string())
            .sender(flotilla_crew::leaf_engine::Party::fleet("turn-rules"))
            .build();
        assert!(daemon.deliver_standing_turn(&request).await.is_err(), "failed staging must prevent delivery");
        let after_failure = convoys.get(&convoy).await.expect("convoy after failed staging").status.expect("status");
        assert_eq!(after_failure.phase, flotilla_resources::ConvoyPhase::Landing);
        assert_eq!(after_failure.work["work"].phase, flotilla_resources::WorkPhase::Complete);
        assert_eq!(after_failure.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
        assert!(credentials.delivered.lock().await.is_empty());
        daemon.deliver_standing_turn(&request).await.expect("retry conflicting turn");
        let delivered = sessions.get("turn-credential-session").await.expect("delivered session");
        assert_eq!(*credentials.delivered.lock().await, BTreeSet::from(["github-crew-pr".to_string()]));
        let messages = crew_messages(&daemon, "flotilla").await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].spec.body, "rebase the PR");
        // Admitted and not yet received: the engine's pending Message.
        assert_eq!(messages[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Deliverable);
        if phase == ResourceTerminalSessionPhase::Stopped {
            assert_eq!(delivered.status.expect("status").phase, ResourceTerminalSessionPhase::Starting);
        }
    }
}

// Idle episodes stage credentials before delivery, stop at the declared nudge
// ceiling (including zero), and clear their stall when the convoy lands.
#[tokio::test]
async fn idle_crew_nudges_are_bounded_and_credential_staged() {
    use super::support::resume_staging_fixture_shaped;
    use flotilla_resources::{Clock, StallRung};
    use flotilla_state_machines::machines::agent_session::Activity;
    use flotilla_store_testkit::VirtualClock;

    for limit in [0, 2] {
        let clock = Arc::new(VirtualClock::new(Utc::now() + chrono::Duration::hours(1)));
        // The coder's work is open, its convoy Active, under the standard exit table and the nudge ceiling.
        let (daemon, backend, probe) = resume_staging_fixture_shaped(engine_home_backend(), clock.clone(), false, |_, snapshot| {
            snapshot.exit = Some(flotilla_resources::ExitDeclaration::standard_table());
            snapshot.stall_nudges.insert(
                "work/coder".to_string(),
                flotilla_resources::StallNudgePolicy { max_per_episode: limit, max_refusals: None, idle_grace_seconds: Some(0) },
            );
        })
        .await;
        probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        // The vessel is realised and the session ready: the coder is at work.
        let vessel = ensure_vessel(&probe.engine, &probe.convoy, "work");
        probe.engine.vessels.ready(&vessel, "resume-env");
        let launch = probe.engine.terminals.record("resume-staging-session").name;
        probe.engine.terminals.ready(&launch, "resume-staging-pool-session", &Default::default());
        let idle = |as_of| TerminalAttention { state: TerminalAttentionState::Idle, as_of, source: TerminalAttentionSource::Hook };
        observe_attention(&probe.engine, &launch, idle(clock.now()));
        let agents = super::support::agent_sessions(&daemon);
        let hook = |activity, cause: &str, at| {
            agents.observe(&launch, activity, cause.into(), super::support::engine_at(at)).expect("hook activity");
        };
        hook(Activity::Idle, "Stop", clock.now());
        let expected_rung = if limit == 0 { StallRung::Operator } else { StallRung::Nudge };
        daemon.reconcile_crew_stalls_once("flotilla").await.expect("first stall judgement");
        let first = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").stalled.expect("stalled");
        assert_eq!(first.rung, expected_rung);
        assert_eq!(first.nudge_history.len(), limit.min(1) as usize);
        if limit == 0 {
            assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
        } else {
            let nudge = crew_messages(&daemon, "flotilla").await.remove(0);
            assert!(nudge.spec.body.contains("You owe deliverables for work/coder"));
            assert!(matches!(nudge.spec.expectation, flotilla_resources::MessageExpectation::Outcome { .. }));
            assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
            for (cooldown_seconds, desired_rung) in [(1, StallRung::Nudge), (2, StallRung::Operator)] {
                for nudge in crew_messages(&daemon, "flotilla").await {
                    if nudge.status.as_ref().is_some_and(|status| status.phase.is_waiting()) {
                        receive_turn(&daemon, &nudge.spec.receiver, &nudge.metadata.name, "resume-staging-session", clock.now()).await;
                    }
                }
                // First observe delivery; only a later idle observation is a reply
                // to that turn. Coalescing both writes would erase that ordering.
                daemon.reconcile_crew_stalls_once("flotilla").await.expect("observe delivered nudge");
                // The agent takes the nudge and stops again: its next turn ends after the delivery.
                hook(Activity::Working, "UserPromptSubmit", clock.now());
                let idle_again = clock.advance(chrono::Duration::seconds(cooldown_seconds));
                hook(Activity::Idle, "Stop", clock.now());
                observe_attention(&probe.engine, &launch, idle(idle_again));
                daemon.reconcile_crew_stalls_once("flotilla").await.expect("next idle episode judgement");
                let stalled = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").stalled.expect("stalled");
                assert_eq!(stalled.nudge_history.len(), 2);
                assert_eq!(stalled.rung, desired_rung);
            }
            let stalled = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").stalled.expect("stalled");
            assert_eq!(stalled.nudge_history.len(), 2);
        }
        // The coder's work is kept and the convoy delivered: it lands.
        super::complete_work(&daemon, &probe.convoy, "work").await;
        let landing = convoys.get(&probe.convoy).await.expect("convoy");
        super::convoy_engine(&daemon).deliver(&landing, "claim").await.expect("land");
        assert_eq!(
            convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").phase,
            flotilla_resources::ConvoyPhase::Landed
        );
        daemon.reconcile_crew_stalls_once("flotilla").await.expect("clear stall");
        assert!(convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").stalled.is_none());
    }
}

// F4 (run 3): an idle seat whose every open promise is submitted owes nothing it can do: its PR
// waits on the forge's merge or close. It is not nudged to submit what it has submitted. Once the
// forge rejects the submission the promise is open again, and the idle seat is nudged.
#[tokio::test]
async fn an_idle_seat_whose_open_promises_are_all_submitted_is_not_nudged() {
    use flotilla_resources::{
        convoy::engine::Actor,
        promises::{owned, PromiseOperation, PromiseState, SubmissionVerdict},
        Clock, StallRung,
    };

    let coder = coder().await;
    let pr = submit_pr(&coder).await;
    let Coder { daemon, backend, probe, clock, launch, .. } = coder;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let engine = super::convoy_engine(&daemon);

    // The coder goes idle, waiting on the forge.
    idle_coder(&daemon, &probe, &clock, &launch);
    daemon.reconcile_crew_stalls_once("flotilla").await.expect("stall judgement");
    // This fixture has no forge, so its change request's observation may stall on its own; the
    // seat is not stalled and not nudged.
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    assert!(status.stalls.about(Some(("work", "coder"))).is_none(), "{:?}", status.stalls);
    assert!(
        !status.stalls.iter().any(|stalled| matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Actor { .. }))),
        "{:?}",
        status.stalls
    );
    assert!(crew_messages(&daemon, "flotilla").await.is_empty(), "no nudge to submit what is submitted");

    // The forge closes the PR unmerged: the submission is rejected and the promise open again.
    let submission = pr.submissions.last().expect("the submission");
    let rejected = PromiseOperation::Verdict {
        id: pr.id.clone(),
        reference: submission.reference.clone(),
        submitted_at: submission.submitted_at,
        verdict: SubmissionVerdict { accepted: false, who: "forge".into(), at: clock.now(), why: "PR closed without merging".into() },
    };
    let current = convoys.get(&probe.convoy).await.expect("convoy");
    engine.promise(&current, "work", "coder", &rejected, Actor::Observer).await.expect("reject the submission");
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    assert_eq!(
        owned(&status, "work", "coder").iter().find(|promise| promise.id == pr.id).expect("the PR promise").state,
        PromiseState::Open
    );
    clock.advance(chrono::Duration::seconds(1));
    idle_coder(&daemon, &probe, &clock, &launch);
    daemon.reconcile_crew_stalls_once("flotilla").await.expect("stall judgement after the rejection");
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    let stalled = status.stalls.about(Some(("work", "coder"))).expect("the seat is stalled");
    assert_eq!(stalled.rung, StallRung::Nudge);
    assert!(crew_messages(&daemon, "flotilla")
        .await
        .iter()
        .any(|message| message.spec.body.contains("You owe deliverables for work/coder")));
}

// F4 (run 3): a submitted PR does not excuse a turn delivered after it. A coder a review rule hands
// a fix while its PR is submitted owes that work, and is nudged when it goes idle.
#[tokio::test]
async fn an_idle_seat_given_a_turn_after_its_submission_is_nudged() {
    use flotilla_resources::{
        promises::{owned, PromiseState},
        StallRung,
    };

    let coder = coder().await;
    let pr = submit_pr(&coder).await;
    deliver_review_fix(&coder).await;
    let Coder { daemon, backend, probe, clock, launch, .. } = coder;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    assert_eq!(
        owned(&status, "work", "coder").iter().find(|promise| promise.id == pr.id).expect("the PR promise").state,
        PromiseState::Submitted
    );

    clock.advance(chrono::Duration::seconds(1));
    idle_coder(&daemon, &probe, &clock, &launch);
    daemon.reconcile_crew_stalls_once("flotilla").await.expect("stall judgement");
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    let stalled = status.stalls.about(Some(("work", "coder"))).expect("the seat is stalled");
    assert_eq!(stalled.rung, StallRung::Nudge);
    assert!(crew_messages(&daemon, "flotilla")
        .await
        .iter()
        .any(|message| message.spec.body.contains("You owe deliverables for work/coder")));
}

// F4 with F7 (run 3): the turn that keeps a submitted seat armed is one after its latest
// submission, by the time the convoy's log records. A coder handed a review fix that pushes it
// and submits its PR again has handed over all it owes, and is not nudged when it goes idle. The
// promise was Submitted throughout, so its record's state time is the first submission's.
#[tokio::test]
async fn a_seat_that_resubmits_after_a_review_fix_is_not_nudged() {
    let coder = coder().await;
    let pr = submit_pr(&coder).await;
    deliver_review_fix(&coder).await;
    coder.clock.advance(chrono::Duration::seconds(1));
    coder
        .daemon
        .crew
        .promise(
            &coder.context,
            flotilla_protocol::commands::CrewPromiseOperation::Submit {
                promise: Some(pr.id.clone()),
                kind: "pr".into(),
                reference: "https://github.com/flotilla-org/flotilla/pull/2200".into(),
                metadata: BTreeMap::from([("commit".to_string(), "head-2".to_string())]),
            },
        )
        .await
        .expect("submit the fixed PR again");
    let Coder { daemon, backend, probe, clock, launch, .. } = coder;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get(&probe.convoy).await.expect("convoy");
    let recorded = daemon.convoy_engine_path().get().expect("engine").recorded_promises(&convoy, None).expect("recorded read");
    let resubmitted = recorded.expect("held")["work"]["coder"].iter().find(|promise| promise.id == pr.id).expect("the PR promise").clone();
    assert_eq!(resubmitted.submissions.len(), 2, "the fix is a second submission");
    assert!(resubmitted.submissions.iter().all(|submission| submission.submitted_at.is_some()), "{resubmitted:?}");
    let nudges = crew_messages(&daemon, "flotilla").await.len();

    clock.advance(chrono::Duration::seconds(1));
    idle_coder(&daemon, &probe, &clock, &launch);
    daemon.reconcile_crew_stalls_once("flotilla").await.expect("stall judgement");
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    assert!(
        !status.stalls.iter().any(|stalled| matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Actor { .. }))),
        "{:?}",
        status.stalls
    );
    assert_eq!(crew_messages(&daemon, "flotilla").await.len(), nudges, "no nudge after the resubmission");
}

/// The review rule fires while the coder's PR waits on the forge, as turn delivery does it: the
/// turn is delivered through the crew's actuator, its episode noted on the convoy, and the coder
/// receives it.
async fn deliver_review_fix(coder: &Coder) {
    use flotilla_resources::{Clock, TurnDeliveryEpisode, TurnDeliveryOutcome};

    let Coder { daemon, backend, probe, clock, .. } = coder;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    clock.advance(chrono::Duration::seconds(1));
    let review = flotilla_crew::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(probe.convoy.clone())
        .source(REVIEW_RULE.into())
        .vessel("work".into())
        .role("coder".into())
        .brief("Address the actionable review and push the fix.".into())
        .subject_revision("head-1".into())
        .sender(flotilla_crew::leaf_engine::Party::fleet("turn-rules"))
        .build();
    let admission = daemon.crew.deliver_turn(&review).await.expect("deliver the review fix");
    let engine_path = daemon.convoy_engine_path();
    flotilla_store::note_convoy(
        &engine_path,
        &convoys,
        &probe.convoy,
        &flotilla_resources::external_patches::record_turn_delivery(
            REVIEW_RULE.into(),
            TurnDeliveryEpisode {
                subject_revision: "head-1".into(),
                evidence_at: clock.now(),
                judged_claim_at: clock.now(),
                outcome: TurnDeliveryOutcome::MessageAccepted {
                    new_turn: admission.new_turn,
                    rung: admission.rung,
                    message: admission.message,
                    accepted_at: clock.now(),
                },
            },
            "work".into(),
            "coder".into(),
            review.brief.clone(),
        ),
    )
    .await
    .expect("note the delivery");
    let turn = crew_messages(daemon, "flotilla").await.remove(0);
    receive_turn(daemon, &turn.spec.receiver, &turn.metadata.name, "resume-staging-session", clock.now()).await;
}

// F4 (run 3): a nudge is not a turn the seat owes after its submission. A coder nudged while its
// ledger was open, which then submits its PR and keeps its ledger, is not nudged again.
#[tokio::test]
async fn a_seat_nudged_before_its_submission_is_not_nudged_after_it() {
    use flotilla_resources::{Clock, StallRung};

    let coder = coder().await;
    {
        let Coder { daemon, backend, probe, clock, launch, .. } = &coder;
        idle_coder(daemon, probe, clock, launch);
        daemon.reconcile_crew_stalls_once("flotilla").await.expect("stall judgement before the submission");
        let status = backend.clone().using::<ResourceConvoy>("flotilla").get(&probe.convoy).await.expect("convoy").status.expect("status");
        assert_eq!(status.stalls.about(Some(("work", "coder"))).expect("the seat is stalled").rung, StallRung::Nudge);
        for nudge in crew_messages(daemon, "flotilla").await {
            receive_turn(daemon, &nudge.spec.receiver, &nudge.metadata.name, "resume-staging-session", clock.now()).await;
        }
    }
    submit_pr(&coder).await;
    let Coder { daemon, backend, probe, clock, launch, .. } = coder;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let nudges = crew_messages(&daemon, "flotilla").await.len();
    assert!(convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").crew_work["work"]["coder"].task_assigned);

    clock.advance(chrono::Duration::seconds(1));
    idle_coder(&daemon, &probe, &clock, &launch);
    daemon.reconcile_crew_stalls_once("flotilla").await.expect("stall judgement after the submission");
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    assert!(
        !status.stalls.iter().any(|stalled| matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Actor { .. }))),
        "{:?}",
        status.stalls
    );
    assert_eq!(crew_messages(&daemon, "flotilla").await.len(), nudges, "no nudge after the submission");
}

/// The review rule `coder` declares; turn delivery records its episodes under this source.
const REVIEW_RULE: &str = "review-fix";

struct Coder {
    daemon: Arc<InProcessDaemon>,
    backend: flotilla_store::ResourceBackend,
    probe: Arc<super::support::SessionStagingProbe>,
    clock: Arc<flotilla_store_testkit::VirtualClock>,
    launch: String,
    context: flotilla_protocol::CrewCommandContext,
}

/// The coder submits its PR and its ledger is kept: every promise it holds is kept or submitted.
async fn submit_pr(coder: &Coder) -> flotilla_resources::promises::Promise {
    use flotilla_resources::promises::{owned, PromiseKind, PromiseState};

    let convoys = coder.backend.clone().using::<ResourceConvoy>("flotilla");
    coder
        .daemon
        .crew
        .promise(
            &coder.context,
            flotilla_protocol::commands::CrewPromiseOperation::Submit {
                promise: None,
                kind: "pr".into(),
                reference: "https://github.com/flotilla-org/flotilla/pull/2200".into(),
                metadata: Default::default(),
            },
        )
        .await
        .expect("submit the PR");
    let current = convoys.get(&coder.probe.convoy).await.expect("convoy");
    super::convoy_engine(&coder.daemon)
        .verdict(&current, "template/decision-ledger", true, "operator", "ledger seen")
        .await
        .expect("ledger kept");
    let status = convoys.get(&coder.probe.convoy).await.expect("convoy").status.expect("status");
    let pr = owned(&status, "work", "coder").iter().find(|promise| promise.kind == PromiseKind::Pr).expect("the PR promise").clone();
    assert_eq!(pr.state, PromiseState::Submitted);
    pr
}

/// A coder at work in a convoy with a standard exit and a review rule targeting it. It owes its
/// decision ledger, and its idle nudges have no grace.
async fn coder() -> Coder {
    use flotilla_store_testkit::VirtualClock;

    let clock = Arc::new(VirtualClock::new(Utc::now() + chrono::Duration::hours(1)));
    let (daemon, backend, probe) =
        super::support::resume_staging_fixture_shaped(engine_home_backend(), clock.clone(), false, |spec, snapshot| {
            spec.repositories = vec![flotilla_resources::ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla".to_string())
                .repo_ref(flotilla_resources::RepositoryKey("flotilla".to_string()))
                .source_ref("feature".to_string())
                .target_ref("main".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()];
            snapshot.exit = Some(flotilla_resources::ExitDeclaration::standard_table());
            snapshot.stall_nudges.insert(
                "work/coder".to_string(),
                flotilla_resources::StallNudgePolicy { max_per_episode: 2, max_refusals: None, idle_grace_seconds: Some(0) },
            );
            snapshot.turn_delivery.insert(
                REVIEW_RULE.to_string(),
                flotilla_resources::TurnDeliveryRule::builder()
                    .on(flotilla_resources::parse_leaf_template("$cr.review.actionable-at-head == true").expect("review leaf"))
                    .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".to_string()).role("coder".to_string()).build())
                    .brief("Address the actionable review and push the fix.".to_string())
                    .hold(flotilla_resources::HoldAct::State)
                    .build(),
            );
        })
        .await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let vessel = ensure_vessel(&probe.engine, &probe.convoy, "work");
    probe.engine.vessels.ready(&vessel, "resume-env");
    let launch = probe.engine.terminals.record("resume-staging-session").name;
    probe.engine.terminals.ready(&launch, "resume-staging-pool-session", &Default::default());
    let context = flotilla_protocol::CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some(probe.convoy.clone()),
        vessel_ref: Some(vessel),
        role: Some("coder".to_string()),
    };
    Coder { daemon, backend, probe, clock, launch, context }
}

/// The coder's terminal and its agent hook both report it idle now.
fn idle_coder(
    daemon: &Arc<InProcessDaemon>,
    probe: &super::support::SessionStagingProbe,
    clock: &flotilla_store_testkit::VirtualClock,
    launch: &str,
) {
    use flotilla_resources::Clock;
    use flotilla_state_machines::machines::agent_session::Activity;

    let now = clock.now();
    observe_attention(
        &probe.engine,
        launch,
        TerminalAttention { state: TerminalAttentionState::Idle, as_of: now, source: TerminalAttentionSource::Hook },
    );
    super::support::agent_sessions(daemon)
        .observe(launch, Activity::Idle, "Stop".into(), super::support::engine_at(now))
        .expect("hook activity");
}

/// The coder's inbox in launch `convoy` of project flotilla.
fn crew_inbox(convoy: &str) -> String {
    let project = flotilla_address::Address::project("flotilla").expect("project");
    flotilla_resources::convoy::engine::seat_address(&project, convoy, "coder").expect("seat").to_string()
}

// A system reply to a controller's turn is correlated with its receiver intent and has a
// creatable home on this daemon.
#[tokio::test]
async fn controller_turn_has_a_creatable_system_reply() {
    use flotilla_resources::Message;

    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let request = flotilla_crew::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(probe.convoy.clone())
        .source("controller".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("controller turn".into())
        .subject_revision("controller-head".into())
        .sender(flotilla_crew::leaf_engine::Party::fleet("turn-rules"))
        .build();
    let admission = daemon.crew.deliver_turn(&request).await.expect("controller intent");
    // The crew's reply is spelled as the crew reads the original.
    let original = crew_messages(&daemon, "flotilla")
        .await
        .into_iter()
        .find(|message| message.metadata.name == admission.message.name)
        .expect("receiver intent");
    let crew = crew_inbox(&probe.convoy);
    let reply = serde_json::json!({
        "apiVersion":"flotilla.work/v1", "kind":"Message", "metadata":{"name":"controller-reply"},
        "spec":{"sender":crew,"inbox":"fleet:lab/role:turn-rules","relation":"peer",
                "body":"acknowledged","in_reply_to":original.metadata.name}
    });
    assert_eq!(
        daemon.message_creation_origin("flotilla", &reply).await.expect("correlated system reply has a home"),
        Some(daemon.node_id().clone())
    );
    assert_eq!(original.spec.sender, "system:turn-rules");
    daemon.apply_intent_document("flotilla", reply).await.expect("create correlated system reply");
    assert!(backend.using::<Message>("flotilla").get("controller-reply").await.is_ok());
}

/// The Message rows loaded and converted in routing a crew's reply to turn rules, with `count`
/// other messages to the crew.
async fn read_by_routing_a_reply(count: usize) -> (u64, u64) {
    let (daemon, _backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let request = flotilla_crew::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(probe.convoy.clone())
        .source("checks".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("checks settled".into())
        .subject_revision("head".into())
        .sender(flotilla_crew::leaf_engine::Party::fleet("turn-rules"))
        .build();
    let admission = daemon.crew.deliver_turn(&request).await.expect("turn");
    let primitives = super::support::install_engine_messages(&daemon);
    let crew = flotilla_resources::crew_role_address("flotilla", &probe.convoy, "work", "coder");
    for index in 0..count {
        let intent = flotilla_engine_messages_testkit::intent(
            "system:turn-rules",
            &crew,
            flotilla_resources::MessageRelation::System,
            format!("other {index}"),
        );
        primitives.admit(&format!("other-{index}"), &intent).await.expect("another message");
    }
    let reply = serde_json::json!({
        "apiVersion":"flotilla.work/v1", "kind":"Message", "metadata":{"name":"reply"},
        "spec":{"sender":crew_inbox(&probe.convoy),"inbox":"fleet:lab/role:turn-rules","relation":"peer","body":"done","in_reply_to":admission.message.name}
    });
    let (home, reads) = flotilla_store::load_metrics::measure(daemon.message_creation_origin("flotilla", &reply)).await;
    assert_eq!(home.expect("the reply has a home"), Some(daemon.node_id().clone()), "the convoy's home");
    reads.get("Message").map_or((0, 0), |counts| (counts.decoded, counts.converted))
}

// Behaviour: routing a reply to a fleet role reads the original by its identity without
// converting it, whatever else its receiver holds (#3108).
#[tokio::test]
async fn routing_a_reply_reads_its_original_without_converting_it() {
    assert_eq!(read_by_routing_a_reply(5).await, (1, 0), "five other messages");
    assert_eq!(read_by_routing_a_reply(60).await, (1, 0), "sixty other messages");
}
