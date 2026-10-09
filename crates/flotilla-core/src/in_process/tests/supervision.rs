use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::{HostName, NodeId};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewMessageSender, CrewWorkPhase, CrewWorkState, InMemoryBackend, InputMeta,
    ResourceBackend, Selector, TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase,
    TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec, TerminalSessionStatus as ResourceTerminalSessionStatus,
    CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use futures::{FutureExt, StreamExt};

use super::support::test_meta;
use crate::config::ConfigStore;
use crate::in_process::crew_ops::queue_pending_crew_message;
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;

#[tokio::test]
async fn operator_brief_survives_a_racing_nudge_until_delivery() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("racing-messages".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "convoy".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "env".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector::for_capability("coding"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".to_string(),
                        content: "Initial".to_string(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "convoy".to_string(),
                        vessel_ref: "work".to_string(),
                    }),
                    message: None,
                },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("session");
    backend
        .using::<ResourceConvoy>("flotilla")
        .create(&test_meta("convoy"), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .expect("convoy context");
    queue_pending_crew_message(&sessions, &session, CrewMessageSender::OperatorResume { principal: None }, "Continue the work")
        .await
        .expect("operator brief queued");
    let queued = sessions.get("racing-messages").await.expect("queued operator brief");
    queue_pending_crew_message(&sessions, &queued, CrewMessageSender::FlotillaNudge, "Please settle").await.expect("nudge queued");
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("records").items;
    assert_eq!(records.len(), 2);
    assert!(records.iter().any(|message| message.spec.sender == "principal:implicit" && message.spec.body.contains("Continue the work")));
    assert!(records.iter().any(|message| message.spec.sender == "system:nudge"));

    let concurrent = sessions
        .create(&InputMeta::builder().name("concurrent-messages".into()).labels(session.metadata.labels.clone()).build(), &session.spec)
        .await
        .expect("concurrent session");
    let (operator, nudge) = tokio::join!(
        queue_pending_crew_message(&sessions, &concurrent, CrewMessageSender::OperatorResume { principal: None }, "New guidance"),
        queue_pending_crew_message(&sessions, &concurrent, CrewMessageSender::FlotillaNudge, "Please settle"),
    );
    operator.expect("operator brief survives concurrent write");
    nudge.expect("nudge does not erase concurrent brief");
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("concurrent records").items;
    assert_eq!(records.len(), 4);
    assert!(records.iter().any(|message| message.spec.body.contains("New guidance")));
}

// #2559: an idle reconciliation pass must neither write resources nor emit
// events, because its runtime caller is triggered by those same watches.
#[hegel::test]
fn supervisor_turn_reconciliation_noop_contract(tc: hegel::TestCase) {
    use hegel::generators as gs;

    // Generate repeated idle passes and pending/terminal Message states.
    // Every case uses both real storage backends with new-only terminal shapes.
    let passes = tc.draw(gs::integers::<usize>().min_value(2).max_value(4));
    let terminal_message = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        for sqlite in [false, true] {
            let temp = tempfile::tempdir().expect("contract directory");
            std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"noop-contract\"\n").expect("daemon identity");
            let backend = if sqlite {
                ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open(temp.path().join("resources.db")).expect("sqlite backend"))
            } else {
                ResourceBackend::InMemory(InMemoryBackend::default())
            };
            let daemon = InProcessDaemon::new_with_resource_backend(
                Vec::new(),
                Arc::new(ConfigStore::with_base(temp.path())),
                fake_discovery(false),
                HostName::new("noop-contract"),
                backend.clone(),
            )
            .await;
            // Keep contract resources outside the daemon's background provisioning namespace.
            let namespace = "noop-contract";
            let sessions = backend.clone().using::<ResourceTerminalSession>(namespace);
            let convoys = backend.clone().using::<ResourceConvoy>(namespace);
            assert_supervisor_turn_passes_are_idle(&daemon, &backend, namespace, "empty store", passes).await;

            let spec = ResourceTerminalSessionSpec {
                env_ref: "env".to_string(),
                role: "governor".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector::for_capability("governor"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".to_string(),
                        content: "standing brief".to_string(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: namespace.to_string(),
                        convoy: "convoy".to_string(),
                        vessel_ref: "govern".to_string(),
                    }),
                    message: None,
                },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "cleat".to_string(),
            };
            sessions.create(&test_meta("unrelated"), &spec).await.expect("unrelated terminal");
            assert_supervisor_turn_passes_are_idle(&daemon, &backend, namespace, "unrelated terminal", passes).await;

            let convoy = convoys
                .create(&test_meta("convoy"), &ConvoySpec::builder().workflow_ref("governor".to_string()).build())
                .await
                .expect("convoy");
            convoys
                .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus::default())
                .await
                .expect("pending turn");
            let session = sessions
                .create(
                    &InputMeta::builder()
                        .name("governor".to_string())
                        .labels(BTreeMap::from([
                            (CONVOY_LABEL.to_string(), "convoy".to_string()),
                            (VESSEL_LABEL.to_string(), "govern".to_string()),
                            (ROLE_LABEL.to_string(), "governor".to_string()),
                        ]))
                        .build(),
                    &spec,
                )
                .await
                .expect("queued terminal");
            sessions
                .update_status(
                    &session.metadata.name,
                    &session.metadata.resource_version,
                    &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() },
                )
                .await
                .expect("running terminal");
            let intent = flotilla_resources::MessageSpec::builder()
                .sender("system:test".into())
                .receiver("noop-contract/convoy/govern/governor".into())
                .relation(flotilla_resources::MessageRelation::System)
                .body("supervise".into())
                .build();
            let inbox = daemon.message_inbox(namespace).await;
            inbox.accept(&test_meta("input"), &intent, Utc::now()).await.expect("new Message");
            if terminal_message {
                let messages = backend.using::<flotilla_resources::Message>(namespace);
                let record = messages.get("input").await.unwrap();
                let mut status = record.status.unwrap();
                status.phase = flotilla_resources::MessagePhase::Expired;
                status.since = Utc::now();
                messages.update_status("input", &record.metadata.resource_version, &status).await.unwrap();
            }
            assert_supervisor_turn_passes_are_idle(&daemon, &backend, namespace, "new-only Message", passes).await;
        }
    });
}

async fn assert_supervisor_turn_passes_are_idle(
    daemon: &InProcessDaemon,
    backend: &ResourceBackend,
    namespace: &str,
    state: &str,
    passes: usize,
) {
    let backend_name = match backend {
        ResourceBackend::InMemory(_) => "in-memory",
        ResourceBackend::Sqlite(_) => "sqlite",
        _ => panic!("unsupported contract backend"),
    };
    let state = format!("{backend_name}: {state}");
    let sessions = backend.clone().using::<ResourceTerminalSession>(namespace);
    let convoys = backend.clone().using::<ResourceConvoy>(namespace);
    // This namespace has no concurrent writers, so list-to-subscribe cannot miss a write.
    let before_sessions = serde_json::to_value(sessions.list().await.expect("terminal baseline")).expect("serialize terminal baseline");
    let before_convoys = serde_json::to_value(convoys.list().await.expect("convoy baseline")).expect("serialize convoy baseline");
    let mut session_watch = sessions.watch(flotilla_resources::WatchStart::Now).await.expect("terminal watch");
    let mut convoy_watch = convoys.watch(flotilla_resources::WatchStart::Now).await.expect("convoy watch");
    for pass in 0..passes {
        daemon.reconcile_pending_supervisor_turns_once(namespace).await.expect("idle pass succeeds");
        // Resource objects and list versions expose even writes of unchanged values.
        assert_eq!(
            serde_json::to_value(sessions.list().await.expect("terminal after pass")).expect("serialize terminals"),
            before_sessions,
            "{state}, pass {pass}: terminal write"
        );
        assert_eq!(
            serde_json::to_value(convoys.list().await.expect("convoy after pass")).expect("serialize convoys"),
            before_convoys,
            "{state}, pass {pass}: convoy write"
        );
        // Both local backends publish before their write completes. Poll directly:
        // an idle watch must be pending, never an event, error, or closed stream.
        assert!(session_watch.next().now_or_never().is_none(), "{state}, pass {pass}: terminal watch activity");
        assert!(convoy_watch.next().now_or_never().is_none(), "{state}, pass {pass}: convoy watch activity");
    }
}

#[tokio::test]
async fn standing_governor_on_another_host_receives_a_stalled_crew_turn() {
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("home"));
    let placement = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("placement"));
    let temp = tempfile::tempdir().expect("config dir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"home\"\n").expect("machine identity");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::local(),
        home.clone(),
    )
    .await;
    let placement_config = tempfile::tempdir().expect("placement config dir");
    std::fs::write(placement_config.path().join("daemon.toml"), "machine_id = \"placement\"\n").expect("placement identity");
    let placement_daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(placement_config.path())),
        fake_discovery(false),
        HostName::local(),
        placement.clone(),
    )
    .await;
    let convoys = home.clone().using::<ResourceConvoy>("flotilla");
    let governor = convoys
        .create(
            &test_meta("governor-convoy"),
            &ConvoySpec::builder().workflow_ref("governor".to_string()).role("governor".to_string()).build(),
        )
        .await
        .expect("governor convoy");
    let existing_attention = flotilla_resources::ConvoyAttention {
        source: "existing-attention".to_string(),
        reason: "Existing concern".to_string(),
        raised_at: Utc::now(),
    };
    convoys
        .update_status(
            &governor.metadata.name,
            &governor.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                work: BTreeMap::from([(
                    "govern".to_string(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "govern".to_string(),
                    BTreeMap::from([("governor".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                attention: Some(existing_attention.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("running governor");
    let sessions = placement.clone().using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("governor-terminal".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "governor-convoy".to_string()),
                    (VESSEL_LABEL.to_string(), "govern".to_string()),
                    (ROLE_LABEL.to_string(), "governor".to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "remote-env".to_string(),
                role: "governor".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector::for_capability("governor"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".to_string(),
                        content: "standing brief".to_string(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "governor-convoy".to_string(),
                        vessel_ref: "governor-convoy-govern".to_string(),
                    }),
                    message: None,
                },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("governor terminal on placement host");
    sessions
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() },
        )
        .await
        .expect("running terminal");
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy("governor-convoy".to_string())
        .source("supervision-stalled-crew".to_string())
        .vessel("govern".to_string())
        .role("governor".to_string())
        .brief("Escalated from coder@work in graphql-budget@flotilla:\n\nSupervise the stalled crew".to_string())
        .subject_revision("stall-1".to_string())
        .sender("system:stall-judge".into())
        .relation(flotilla_resources::MessageRelation::Supervisor)
        .build();
    // Boundary fake for publication to the owning host; receiver admission
    // still runs through the real daemon's ordinary resource mutation handler.
    struct PlacementPublisher(Arc<InProcessDaemon>);
    #[async_trait]
    impl crate::leaf_engine::ResourceIntentPublisher for PlacementPublisher {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<flotilla_protocol::ResourceRef, String> {
            let admitted = self.0.apply_intent_document(namespace, document).await.map_err(|error| error.to_string())?;
            Ok(flotilla_protocol::ResourceRef::new(
                "flotilla.work/v1",
                admitted.kind,
                admitted.namespace,
                admitted.value["metadata"]["name"].as_str().expect("admitted resource name"),
            ))
        }
    }
    let publisher: Arc<dyn crate::leaf_engine::ResourceIntentPublisher> = Arc::new(PlacementPublisher(placement_daemon));
    daemon.set_resource_intent_publisher(Arc::downgrade(&publisher));
    daemon.deliver_standing_turn(&request).await.expect("remote governor intent accepted");
    let messages = placement.using::<flotilla_resources::Message>("flotilla").list().await.expect("receiver messages").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.receiver, "flotilla/governor-convoy/govern/governor");
    assert_eq!(messages[0].spec.relation, flotilla_resources::MessageRelation::Supervisor);
    assert_eq!(messages[0].spec.body, "Escalated from coder@work in graphql-budget@flotilla:\n\nSupervise the stalled crew");
    assert_eq!(messages[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Accepted);
    assert!(home.using::<flotilla_resources::Message>("flotilla").list().await.expect("sender messages").items.is_empty());
    assert_eq!(convoys.get("governor-convoy").await.expect("governor").status.expect("status").attention, Some(existing_attention));
}
