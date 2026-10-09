use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use flotilla_protocol::HostName;
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState, InMemoryBackend, InputMeta, ResourceBackend,
    Selector, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec,
    TerminalSessionStatus as ResourceTerminalSessionStatus, TurnDeliveryRung, VesselRequirement, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};

use super::support::{resume_staging_fixture, test_meta, RecordingWorkCredentials};
use crate::config::ConfigStore;
use crate::in_process::{input_meta_from_resource, InProcessDaemon};
use crate::testkits::discovery::fake_discovery;

#[tokio::test]
async fn turn_delivery_accepts_intent_when_terminal_changes_during_staging() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    probe.invalidate_next.store(true, std::sync::atomic::Ordering::SeqCst);
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy("resume-staging".to_string())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("continue".to_string())
        .subject_revision("new-head".to_string())
        .sender("system:turn-rules".into())
        .build();
    daemon.deliver_standing_turn(&request).await.expect("durable intent accepted after concurrent terminal edit");
    let status = backend.using::<ResourceConvoy>("flotilla").get("resume-staging").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("messages").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.body, "continue");
    let session = backend.using::<ResourceTerminalSession>("flotilla").get("resume-staging-session").await.expect("session");
    assert!(matches!(&session.spec.source, TerminalSessionSource::Agent { brief, .. } if brief.content.ends_with(" (concurrent edit)")));
}

// A fresh turn is durable intent independent of the boot brief; it waits for
// the replacement holder's readiness and transport acceptance evidence.
#[tokio::test]
async fn fresh_turn_stores_intent_independently_of_boot_brief() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("resume-staging-session").await.expect("session");
    let mut spec = session.spec.clone();
    let TerminalSessionSource::Agent { brief, .. } = &mut spec.source else { panic!("agent session") };
    brief.artifact_digest = Some("a".repeat(64));
    brief.content.clear();
    let session = sessions
        .update(&input_meta_from_resource(&session), &session.metadata.resource_version, &spec)
        .await
        .expect("digest-backed session");
    sessions
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Stopped, ..Default::default() },
        )
        .await
        .expect("stopped session");
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy("resume-staging".to_string())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("fresh turn".to_string())
        .subject_revision("next-head".to_string())
        .sender("system:turn-rules".into())
        .build();
    daemon.deliver_standing_turn(&request).await.expect("deliver fresh turn");
    let session = sessions.get("resume-staging-session").await.expect("updated session");
    let TerminalSessionSource::Agent { brief, message, .. } = session.spec.source else { panic!("agent session") };
    assert!(brief.content.is_empty());
    assert_eq!(brief.artifact_digest, Some("a".repeat(64)));
    assert!(message.is_none());
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("messages").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.body, "fresh turn");
    assert_eq!(messages[0].spec.sender, "system:turn-rules");
    assert_eq!(messages[0].spec.receiver, "flotilla/resume-staging/work/coder");
    assert_eq!(session.status.expect("status").phase, ResourceTerminalSessionPhase::Starting);
}

#[tokio::test]
async fn repeated_standing_turn_does_not_restart_a_lost_session_after_delivery() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy("resume-staging".to_string())
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
    flotilla_resources::apply_status_patch(
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
    let session = sessions.get("resume-staging-session").await.expect("session");
    sessions
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Lost, ..Default::default() },
        )
        .await
        .expect("lost after delivery");
    assert_eq!(daemon.deliver_standing_turn(&request).await.expect("duplicate already accepted"), TurnDeliveryRung::FreshAgent);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(messages.list().await.expect("messages").items.len(), 1);
    assert_eq!(
        sessions.get("resume-staging-session").await.expect("session").status.expect("status").phase,
        ResourceTerminalSessionPhase::Lost
    );
}

#[tokio::test]
async fn turn_delivery_reopens_work_and_stages_credentials_before_queuing_every_rung() {
    for phase in [ResourceTerminalSessionPhase::Running, ResourceTerminalSessionPhase::Starting, ResourceTerminalSessionPhase::Stopped] {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"turn-credential-test\"\n").expect("daemon config");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("test-host"),
            backend.clone(),
        )
        .await;
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
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name("turn-credential-session".to_string())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.to_string(), "turn-credential-work".to_string()),
                        (VESSEL_LABEL.to_string(), "work".to_string()),
                        (ROLE_LABEL.to_string(), "coder".to_string()),
                    ]))
                    .build(),
                &ResourceTerminalSessionSpec {
                    env_ref: "credential-env".to_string(),
                    role: "coder".to_string(),
                    source: TerminalSessionSource::Agent {
                        selector: Selector { capability: "code".to_string(), adapter: None, model: None },
                        brief: flotilla_resources::TerminalBrief {
                            artifact_digest: None,
                            path: "brief.md".to_string(),
                            content: "original".to_string(),
                            copies: vec![],
                        },
                        context: Box::new(flotilla_resources::TerminalCrewContext {
                            namespace: "flotilla".to_string(),
                            convoy: "turn-credential-work".to_string(),
                            vessel_ref: "turn-credential-vessel".to_string(),
                        }),
                        message: None,
                    },
                    cwd: "/repo".to_string(),
                    env: Default::default(),
                    pool: "passthrough".to_string(),
                },
            )
            .await
            .expect("session");
        sessions
            .update_status(
                &session.metadata.name,
                &session.metadata.resource_version,
                &ResourceTerminalSessionStatus { phase, ..Default::default() },
            )
            .await
            .expect("session phase");
        let request = crate::leaf_engine::CrewTurnIntent::builder()
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
        let undelivered = sessions.get("turn-credential-session").await.expect("undelivered session");
        let TerminalSessionSource::Agent { message, .. } = undelivered.spec.source else { panic!("agent session expected") };
        assert!(message.is_none());
        daemon.deliver_standing_turn(&request).await.expect("retry conflicting turn");
        let delivered = sessions.get("turn-credential-session").await.expect("delivered session");
        assert_eq!(*credentials.delivered.lock().await, BTreeSet::from(["github-crew-pr".to_string()]));
        let TerminalSessionSource::Agent { message, .. } = delivered.spec.source else { panic!("agent session expected") };
        assert!(message.is_none());
        let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("accepted messages").items;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].spec.body, "rebase the PR");
        assert_eq!(messages[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Accepted);
        if phase == ResourceTerminalSessionPhase::Stopped {
            assert_eq!(delivered.status.expect("status").phase, ResourceTerminalSessionPhase::Starting);
        }
    }
}

#[tokio::test]
async fn idle_crew_nudges_are_bounded_and_credential_staged() {
    use flotilla_resources::StallRung;

    for limit in [0, 2] {
        let (daemon, backend, probe) = resume_staging_fixture().await;
        probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("resume-staging").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = flotilla_resources::ConvoyPhase::Active;
        status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Running;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
        status.workflow_snapshot.as_mut().expect("workflow").stall_nudges.insert(
            "work/coder".to_string(),
            flotilla_resources::StallNudgePolicy { max_per_episode: limit, max_refusals: None, idle_grace_seconds: Some(0) },
        );
        convoys.update_status("resume-staging", &convoy.metadata.resource_version, &status).await.expect("active convoy");
        let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
        let session = sessions.get("resume-staging-session").await.expect("session");
        let mut session_status = ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() };
        session_status.attention = Some(TerminalAttention {
            state: TerminalAttentionState::Idle,
            as_of: chrono::Utc::now(),
            source: TerminalAttentionSource::Hook,
        });
        sessions.update_status("resume-staging-session", &session.metadata.resource_version, &session_status).await.expect("idle session");
        let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
        let watcher = daemon.reconciler_wake_watch();
        let task = tokio::spawn(watcher.spawn(backend.clone(), "flotilla".to_string(), tx));
        let expected_rung = if limit == 0 { StallRung::Operator } else { StallRung::Nudge };
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let stalled = convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled;
                if stalled.as_ref().is_some_and(|stall| stall.rung == expected_rung && stall.nudge_history.len() == limit.min(1) as usize) {
                    break stalled.expect("stalled");
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first stall judgement");
        assert_eq!(first.nudge_history.len(), limit.min(1) as usize);
        let session = sessions.get("resume-staging-session").await.expect("session");
        let TerminalSessionSource::Agent { message, .. } = session.spec.source else { panic!("agent") };
        if limit == 0 {
            assert!(message.is_none());
            assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
        } else {
            assert!(message.is_none());
            let messages = backend.using::<flotilla_resources::Message>("flotilla");
            let nudge = messages.list().await.expect("nudges").items.remove(0);
            assert!(nudge.spec.body.contains("You owe a settlement claim for work/coder"));
            assert!(matches!(nudge.spec.expectation, flotilla_resources::MessageExpectation::Outcome { .. }));
            assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
            for (offset, desired_rung) in [(1, StallRung::Nudge), (2, StallRung::Operator)] {
                for nudge in messages.list().await.expect("nudges").items {
                    if nudge.status.as_ref().is_some_and(|status| status.phase.is_waiting()) {
                        flotilla_resources::apply_status_patch(
                            &messages,
                            &nudge.metadata.name,
                            &flotilla_resources::MessageStatusPatch::Delivered {
                                receiver: flotilla_resources::ResolvedMessageReceiver::builder()
                                    .crew_id("crew".into())
                                    .session("resume-staging-session".into())
                                    .delivered_at(Utc::now())
                                    .evidence("agent accepted nudge".into())
                                    .build(),
                                at: Utc::now(),
                            },
                        )
                        .await
                        .expect("consume nudge");
                    }
                }
                let session = sessions.get("resume-staging-session").await.expect("session");
                session_status.attention.as_mut().expect("attention").as_of = chrono::Utc::now() + chrono::Duration::seconds(offset);
                sessions
                    .update_status("resume-staging-session", &session.metadata.resource_version, &session_status)
                    .await
                    .expect("idle again");
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let stalled =
                            convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled.expect("stalled");
                        if stalled.nudge_history.len() == 2 && stalled.rung == desired_rung {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("next idle episode judgement");
            }
            let stalled = convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled.expect("stalled");
            assert_eq!(stalled.nudge_history.len(), 2);
        }
        let convoy = convoys.get("resume-staging").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = flotilla_resources::ConvoyPhase::Landed;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Done;
        convoys.update_status("resume-staging", &convoy.metadata.resource_version, &status).await.expect("complete");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stall cleared");
        task.abort();
    }
}
