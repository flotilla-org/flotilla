use flotilla_store_memory::engine_home_backend;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_protocol::{HostName, NodeId};
use flotilla_resources::{Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, TerminalSession as ResourceTerminalSession};
use flotilla_store::ResourceBackend;
use futures::{FutureExt, StreamExt};

use super::support::{crew_messages, designate_fleet, install_engine_messages, test_engine};
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;
use flotilla_resources::terminal_records::{TerminalLaunchRequest, TerminalLaunchSource};

// #2559: an idle reconciliation pass must neither write resources nor emit
// events, because its runtime caller is triggered by those same watches.
#[tokio::test]
async fn supervisor_turn_reconciliation_noop_contract() {
    // The whole finite domain: pending and terminal Message states. Every pass is asserted
    // idle, so the longest run of repeated passes covers the shorter ones as its prefixes.
    let passes = 4;
    for terminal_message in [false, true] {
        let temp = tempfile::tempdir().expect("contract directory");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"noop-contract\"\n").expect("daemon identity");
        let backend = engine_home_backend();
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("noop-contract"),
            backend.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let engine = test_engine(&daemon);
        // Keep contract resources outside the daemon's background provisioning namespace.
        // The fleet's Projects, which the Message path's roles come from, are served in `flotilla`.
        let namespace = "flotilla";
        assert_supervisor_turn_passes_are_idle(&daemon, &backend, namespace, "empty store", passes).await;

        let governor = |session: &str, vessel: Option<String>| TerminalLaunchRequest {
            session: session.to_string(),
            environment: "env".to_string(),
            vessel,
            role: "governor".to_string(),
            pool: "cleat".to_string(),
            cwd: "/workspace".to_string(),
            env: Default::default(),
            source: TerminalLaunchSource::Agent {
                capability: "governor".to_string(),
                adapter: None,
                model: None,
                brief: flotilla_resources::TerminalBrief {
                    path: "brief.md".to_string(),
                    content: "standing brief".to_string(),
                    artifact_digest: None,
                    copies: Vec::new(),
                },
            },
        };
        engine.terminals.request(&governor("unrelated", None));
        assert_supervisor_turn_passes_are_idle(&daemon, &backend, namespace, "unrelated terminal", passes).await;

        let convoy = super::support::admit(
            &daemon,
            "convoy",
            ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("governor".to_string()).build(),
            ConvoyStatus {
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![flotilla_resources::VesselRequirement::builder()
                        .name("govern".to_string())
                        .crew(vec![flotilla_engine_convoys_testkit::agent("governor")])
                        .build()],
                }),
                ..Default::default()
            },
        )
        .await;
        let vessel = engine.vessels.request(&convoy, "govern", "noop-contract");
        let session = engine.terminals.request(&governor("governor", Some(vessel)));
        engine.terminals.ready(&session, "governor-sid", &Default::default());
        let receiver = flotilla_resources::crew_role_address(namespace, &convoy, "govern", "governor");
        let intent =
            flotilla_engine_messages_testkit::intent("system:nudge", &receiver, flotilla_resources::MessageRelation::System, "supervise");
        flotilla_engine_messages_testkit::designate_fleet(&backend, namespace).await;
        let primitives = install_engine_messages(&daemon);
        primitives.admit("input", &intent).await.expect("new Message");
        if terminal_message {
            // Delivered, it awaits nothing more: it has ended.
            flotilla_engine_messages_testkit::deliver_turn(&primitives, namespace, &receiver, "input", "session", Utc::now()).await;
        }
        let state = if terminal_message { "ended Message" } else { "Message" };
        assert_supervisor_turn_passes_are_idle(&daemon, &backend, namespace, state, passes).await;
    }
}

async fn assert_supervisor_turn_passes_are_idle(
    daemon: &InProcessDaemon,
    backend: &ResourceBackend,
    namespace: &str,
    state: &str,
    passes: usize,
) {
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
    let home = engine_home_backend().with_local_root(NodeId::new("home"));
    let placement = engine_home_backend().with_local_root(NodeId::new("placement"));
    let temp = tempfile::tempdir().expect("config dir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"home\"\n").expect("machine identity");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::local(),
        home.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
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
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let convoys = home.clone().using::<ResourceConvoy>("flotilla");
    let existing_attention = flotilla_resources::ConvoyAttention {
        source: "existing-attention".to_string(),
        reason: "Existing concern".to_string(),
        raised_at: Utc::now(),
    };
    test_engine(&daemon);
    let placement_engine = test_engine(&placement_daemon);
    designate_fleet(&daemon.resource_backend()).await;
    designate_fleet(&placement).await;
    // The governor convoy is admitted at home, its work `govern` crewed by the governor, and
    // replicated to the placement host.
    let governor = super::support::admit(
        &daemon,
        "governor-convoy",
        ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("governor".to_string()).role("governor".to_string()).build(),
        ConvoyStatus {
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                cascade: None,
                stall_nudges: Default::default(),
                supervision: None,
                exit: None,
                turn_delivery: Default::default(),
                vessels: vec![flotilla_resources::VesselRequirement::builder()
                    .name("govern".to_string())
                    .crew(vec![flotilla_engine_convoys_testkit::agent("governor")])
                    .build()],
            }),
            attention: Some(existing_attention.clone()),
            ..Default::default()
        },
    )
    .await;
    flotilla_provisioning_testkit::replicate_home(
        &daemon.decided_store().expect("home store"),
        daemon.node_id().as_str(),
        &placement_daemon.decided_store().expect("placement store"),
    );
    // The governor's session runs on the placement host, held for its work's launch there.
    let vessel = placement_engine.vessels.request(&governor, "govern", "placement");
    let governor_session = placement_engine.terminals.request(&TerminalLaunchRequest {
        session: "governor-terminal".to_string(),
        environment: "remote-env".to_string(),
        vessel: Some(vessel),
        role: "governor".to_string(),
        pool: "cleat".to_string(),
        cwd: "/workspace".to_string(),
        env: Default::default(),
        source: TerminalLaunchSource::Agent {
            capability: "governor".to_string(),
            adapter: None,
            model: None,
            brief: flotilla_resources::TerminalBrief {
                path: "brief.md".to_string(),
                content: "standing brief".to_string(),
                artifact_digest: None,
                copies: Vec::new(),
            },
        },
    });
    placement_engine.terminals.ready(&governor_session, "governor-sid", &Default::default());
    let request = flotilla_crew::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy(governor.clone())
        .source("supervision-stalled-crew".to_string())
        .vessel("govern".to_string())
        .role("governor".to_string())
        .brief("Escalated from coder@work in graphql-budget@flotilla:\n\nSupervise the stalled crew".to_string())
        .subject_revision("stall-1".to_string())
        .sender(flotilla_crew::leaf_engine::Party::fleet("stall-judge"))
        .relation(flotilla_resources::MessageRelation::Supervisor)
        .build();
    // Boundary fake for publication to the owning host; receiver admission
    // still runs through the real daemon's ordinary resource mutation handler.
    struct PlacementPublisher(Arc<InProcessDaemon>);
    #[async_trait]
    impl flotilla_crew::leaf_engine::ResourceIntentPublisher for PlacementPublisher {
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
    let publisher: Arc<dyn flotilla_crew::leaf_engine::ResourceIntentPublisher> =
        Arc::new(PlacementPublisher(Arc::clone(&placement_daemon)));
    daemon.set_resource_intent_publisher(Arc::downgrade(&publisher));
    daemon.deliver_standing_turn(&request).await.expect("remote governor intent accepted");
    let messages = crew_messages(&placement_daemon, "flotilla").await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.receiver, format!("flotilla/{governor}/govern/governor"));
    assert_eq!(messages[0].spec.relation, flotilla_resources::MessageRelation::Supervisor);
    assert_eq!(messages[0].spec.body, "Escalated from coder@work in graphql-budget@flotilla:\n\nSupervise the stalled crew");
    // Admitted and not yet received: the engine's pending Message.
    assert_eq!(messages[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Deliverable);
    assert!(home.using::<flotilla_resources::Message>("flotilla").list().await.expect("sender messages").items.is_empty());
    assert_eq!(convoys.get(&governor).await.expect("governor").status.expect("status").attention, Some(existing_attention));
}
