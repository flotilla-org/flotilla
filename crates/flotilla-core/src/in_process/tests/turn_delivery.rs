use flotilla_store_memory::InMemoryBackend;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_protocol::HostName;
use flotilla_resources::{
    terminal_records::{TerminalLaunchRequest, TerminalLaunchSource},
    Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState, TerminalAttention, TerminalAttentionSource,
    TerminalAttentionState, TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase,
    TurnDeliveryRung, VesselRequirement,
};
use flotilla_store::ResourceBackend;

use super::support::{ensure_vessel, observe_attention, resume_staging_fixture, test_meta, RecordingWorkCredentials};
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
        .sender("system:turn-rules".into())
        .build();
    daemon.deliver_standing_turn(&request).await.expect("deliver fresh turn");
    let session = sessions.get("resume-staging-session").await.expect("updated session");
    // The session's next launch boots with the same brief; the turn is not written into it.
    let relaunched = terminals.record("resume-staging-session");
    assert_ne!(relaunched.name, exited.name);
    assert_eq!(relaunched.source, exited.source);
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("messages").items;
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
        .sender("system:turn-rules".into())
        .build();
    daemon.deliver_standing_turn(&request).await.expect("first intent");
    let messages = backend.using::<flotilla_resources::Message>("flotilla");
    let message = messages.list().await.expect("messages").items.remove(0);
    flotilla_store::apply_status_patch(
        &messages,
        &message.metadata.name,
        &flotilla_resources::MessageStatusPatch::Delivered {
            receiver: flotilla_resources::ResolvedMessageReceiver::builder()
                .crew_id("crew".into())
                .session("old-session".into())
                .delivered_at(Utc::now())
                .evidence("holder accepted input".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .expect("receipt");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let terminals = &probe.engine.terminals;
    let lost = terminals.record("resume-staging-session").name;
    terminals.ready(&lost, "resume-staging-pool-session", &Default::default());
    terminals.lost(&lost, "pool lost the session");
    assert_eq!(daemon.deliver_standing_turn(&request).await.expect("duplicate already accepted"), TurnDeliveryRung::FreshAgent);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(messages.list().await.expect("messages").items.len(), 1);
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
        let backend = ResourceBackend::new(InMemoryBackend::default());
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
        let credentials = Arc::new(RecordingWorkCredentials {
            backend: backend.clone(),
            delivered: tokio::sync::Mutex::new(BTreeSet::new()),
            fail_next: std::sync::atomic::AtomicBool::new(true),
        });
        daemon.set_work_credential_reconciler(credentials.clone()).await;
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys
            .create(&test_meta("turn-credential-work"), &ConvoySpec::builder().workflow_ref("implement-review".to_string()).build())
            .await
            .expect("convoy");
        convoys
            .update_status(
                &convoy.metadata.name,
                &convoy.metadata.resource_version,
                &ConvoyStatus {
                    phase: flotilla_resources::ConvoyPhase::Landing,
                    workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                        cascade: None,
                        stall_nudges: Default::default(),
                        supervision: None,
                        exit: None,
                        turn_delivery: Default::default(),
                        vessels: vec![VesselRequirement::builder()
                            .name("work".to_string())
                            .credential_refs(BTreeSet::from(["github-crew-pr".to_string()]))
                            .crew(Vec::new())
                            .build()],
                    }),
                    work: BTreeMap::from([(
                        "work".to_string(),
                        flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Complete).build(),
                    )]),
                    crew_work: BTreeMap::from([(
                        "work".to_string(),
                        BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Done).build())]),
                    )]),
                    ..Default::default()
                },
            )
            .await
            .expect("admitted claim");
        let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
        let vessel = ensure_vessel(&engine, "turn-credential-work", "work");
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
            .convoy("turn-credential-work".to_string())
            .source("conflicting".to_string())
            .vessel("work".to_string())
            .role("coder".to_string())
            .brief("rebase the PR".to_string())
            .subject_revision("new-head".to_string())
            .sender("system:turn-rules".into())
            .build();
        assert!(daemon.deliver_standing_turn(&request).await.is_err(), "failed staging must prevent delivery");
        let after_failure = convoys.get("turn-credential-work").await.expect("convoy after failed staging").status.expect("status");
        assert_eq!(after_failure.phase, flotilla_resources::ConvoyPhase::Landing);
        assert_eq!(after_failure.work["work"].phase, flotilla_resources::WorkPhase::Complete);
        assert_eq!(after_failure.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
        assert!(credentials.delivered.lock().await.is_empty());
        daemon.deliver_standing_turn(&request).await.expect("retry conflicting turn");
        let delivered = sessions.get("turn-credential-session").await.expect("delivered session");
        assert_eq!(*credentials.delivered.lock().await, BTreeSet::from(["github-crew-pr".to_string()]));
        let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("accepted messages").items;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].spec.body, "rebase the PR");
        assert_eq!(messages[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Accepted);
        if phase == ResourceTerminalSessionPhase::Stopped {
            assert_eq!(delivered.status.expect("status").phase, ResourceTerminalSessionPhase::Starting);
        }
    }
}

// Idle episodes stage credentials before delivery, stop at the declared nudge
// ceiling (including zero), and clear their stall when the convoy lands.
#[tokio::test]
async fn idle_crew_nudges_are_bounded_and_credential_staged() {
    use super::support::resume_staging_fixture_with_clock;
    use flotilla_resources::{Clock, StallRung};
    use flotilla_state_machines::machines::agent_session::Activity;
    use flotilla_store_testkit::VirtualClock;

    for limit in [0, 2] {
        let clock = Arc::new(VirtualClock::new(Utc::now() + chrono::Duration::hours(1)));
        let (daemon, backend, probe) =
            resume_staging_fixture_with_clock(ResourceBackend::new(InMemoryBackend::default()), clock.clone()).await;
        probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get(&probe.convoy).await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = flotilla_resources::ConvoyPhase::Active;
        status.workflow_snapshot.as_mut().expect("workflow").exit = Some(flotilla_resources::ExitDeclaration::standard_table());
        status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Running;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
        status.workflow_snapshot.as_mut().expect("workflow").stall_nudges.insert(
            "work/coder".to_string(),
            flotilla_resources::StallNudgePolicy { max_per_episode: limit, max_refusals: None, idle_grace_seconds: Some(0) },
        );
        convoys.update_status(&probe.convoy, &convoy.metadata.resource_version, &status).await.expect("active convoy");
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
            let messages = backend.using::<flotilla_resources::Message>("flotilla");
            let nudge = messages.list().await.expect("nudges").items.remove(0);
            assert!(nudge.spec.body.contains("You owe deliverables for work/coder"));
            assert!(matches!(nudge.spec.expectation, flotilla_resources::MessageExpectation::Outcome { .. }));
            assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
            for (cooldown_seconds, desired_rung) in [(1, StallRung::Nudge), (2, StallRung::Operator)] {
                for nudge in messages.list().await.expect("nudges").items {
                    if nudge.status.as_ref().is_some_and(|status| status.phase.is_waiting()) {
                        flotilla_store::apply_status_patch(
                            &messages,
                            &nudge.metadata.name,
                            &flotilla_resources::MessageStatusPatch::Delivered {
                                receiver: flotilla_resources::ResolvedMessageReceiver::builder()
                                    .crew_id("crew".into())
                                    .session("resume-staging-session".into())
                                    .delivered_at(clock.now())
                                    .evidence("agent accepted nudge".into())
                                    .build(),
                                at: clock.now(),
                            },
                        )
                        .await
                        .expect("consume nudge");
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
        let convoy = convoys.get(&probe.convoy).await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = flotilla_resources::ConvoyPhase::Landed;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Done;
        convoys.update_status(&probe.convoy, &convoy.metadata.resource_version, &status).await.expect("complete");
        daemon.reconcile_crew_stalls_once("flotilla").await.expect("clear stall");
        assert!(convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").stalled.is_none());
    }
}

// A system reply to a legacy controller sender is correlated with its receiver
// intent and has a creatable home on this daemon.
#[tokio::test]
async fn unknown_controller_sender_has_a_creatable_system_reply() {
    use flotilla_resources::{CrewMessageSender, Message};

    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let request = flotilla_crew::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(probe.convoy.clone())
        .source("legacy".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("legacy controller turn".into())
        .subject_revision("legacy-head".into())
        .sender(flotilla_resources::legacy_message_sender(&CrewMessageSender::Unknown).0)
        .build();
    let admission = daemon.crew.deliver_turn(&request).await.expect("controller intent");
    let original = backend.using::<Message>("flotilla").get(&admission.message.name).await.expect("receiver intent");
    let reply = serde_json::json!({
        "apiVersion":"flotilla.work/v1", "kind":"Message", "metadata":{"name":"legacy-reply"},
        "spec":{"sender":original.spec.receiver,"receiver":original.spec.sender,"relation":"peer",
                "body":"acknowledged","in_reply_to":original.metadata.name}
    });
    assert_eq!(
        daemon.message_creation_origin("flotilla", &reply).await.expect("correlated system reply has a home"),
        Some(daemon.node_id().clone())
    );
    assert_eq!(original.spec.sender, "system:legacy");
    daemon.apply_intent_document("flotilla", reply).await.expect("create correlated system reply");
    assert!(backend.using::<Message>("flotilla").get("legacy-reply").await.is_ok());
}
