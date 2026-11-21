use flotilla_resources::terminal_records::{AgentLaunch, TerminalLaunchFacts, TerminalRecords as _};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_crew::leaf_engine::CrewTurnIntent;
use flotilla_crew::reconcilers::vessel::VesselPlanning;
use flotilla_daemon::runtime::spawn_pending_supervisor_turn_task;
use flotilla_daemon::runtime::spawn_pending_supervisor_turn_task_with_watches;
use flotilla_daemon_testkit::server::spawn_in_memory_request_topology_stateful;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::{HostName, NodeId};
use flotilla_resources::{
    ClaimExit, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, EnvironmentSpec, ExitDeclaration, FulfilmentKind,
    FulfilmentKindSpec, FulfilmentRealisation, HostDirectEnvironmentSpec, InputMeta, Message, MessageExpectation, Selector,
    TerminalSession, Vessel, VesselRequirement, WorkPhase, WorkState, WorkflowSnapshot,
};
use futures::StreamExt;
use tokio_util::task::AbortOnDropHandle;

fn meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
}

#[derive(Clone, Copy)]
enum ClosedWatch {
    Convoy,
    TerminalSession,
}

// #2287 recovery: a closed notification stream must be resubscribed, and the
// recovery scan must deliver a turn replicated while that stream was down.
#[tokio::test]
async fn remote_turn_delivery_recovers_after_either_watch_closes() {
    for input in [ClosedWatch::Convoy, ClosedWatch::TerminalSession] {
        remote_turn_scenario(Some(input)).await;
    }
}

#[tokio::test]
async fn modern_remote_nudge_uses_ordinary_message_admission() {
    remote_turn_scenario(None).await;
}

async fn remote_turn_scenario(closed_watch: Option<ClosedWatch>) {
    let home = flotilla_store_memory::engine_home_backend().with_local_root(NodeId::new("home"));
    let placement = flotilla_store_memory::engine_home_backend().with_local_root(NodeId::new("placement"));
    let home_dir = tempfile::tempdir().expect("home config");
    std::fs::write(home_dir.path().join("daemon.toml"), "machine_id = \"home\"\n").expect("home identity");
    let home_daemon = Arc::new(
        InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(home_dir.path())),
            fake_discovery(false),
            HostName::local(),
            home.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await,
    );
    let placement_dir = tempfile::tempdir().expect("placement config");
    std::fs::write(placement_dir.path().join("daemon.toml"), "machine_id = \"placement\"\n").expect("placement identity");
    let placement_daemon = Arc::new(
        InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(placement_dir.path())),
            fake_discovery(false),
            HostName::local(),
            placement.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await,
    );

    let home = home_daemon.resource_backend();
    let placement = placement_daemon.resource_backend();
    // The runtime opens each daemon's Message path at startup.
    flotilla_engine_messages_testkit::designate_fleet(&home, "flotilla").await;
    flotilla_engine_messages_testkit::designate_fleet(&placement, "flotilla").await;

    // Keep the real request endpoints alive: producers publish ordinary
    // receiver-homed mutations rather than enqueueing at the convoy authority.
    let _topology = spawn_in_memory_request_topology_stateful(home_daemon.as_ref().clone(), placement_daemon.as_ref().clone())
        .await
        .expect("cross-host Message mutation router");

    // The convoy is the home's engine launch, its coder working.
    let convoy = crate::admit_convoy(
        &home_daemon,
        flotilla_engine_convoys_testkit::convoy(
            "flotilla",
            "nudge-convoy",
            ConvoySpec::builder()
                .project_ref("flotilla".into())
                .workflow_ref("wf".to_string())
                .placement_policy("placement-policy".to_string())
                .build(),
            ConvoyStatus {
                phase: ConvoyPhase::Active,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    exit: Some(ExitDeclaration::Claim(ClaimExit)),
                    turn_delivery: Default::default(),
                    stall_nudges: Default::default(),
                    supervision: None,
                    vessels: vec![VesselRequirement::builder()
                        .name("work".to_string())
                        .crew(vec![CrewSpec::builder()
                            .role("coder".to_string())
                            .source(CrewSource::Agent {
                                selector: Selector::for_capability("coding"),
                                prompt: Some("Initial".to_string()),
                                brief_template: None,
                            })
                            .build()])
                        .build()],
                }),
                work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Running).build())]),
                ..Default::default()
            },
        ),
    )
    .await;
    home.using::<FulfilmentKind>("flotilla")
        .create(
            &meta("placement-policy"),
            &FulfilmentKindSpec::builder()
                .host_ref("placement-host".to_string())
                .pool("cleat".to_string())
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("fulfilment kind");
    // A FulfilmentKind is an engine record: it reaches the placement as its home's log does.
    flotilla_provisioning_testkit::other_home::replicate_home(
        &home_daemon.decided_store(),
        home_daemon.node_id().as_str(),
        &placement_daemon.decided_store(),
    );
    crate::engine_environments(&placement_daemon).put_ready(
        "host-direct-placement-host",
        &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec {
                host_ref: "placement-host".to_string(),
                repo_default_dir: "/workspace".to_string(),
            }),
            ..Default::default()
        },
        None,
        "/workspace",
        Default::default(),
    );
    // The placement launches the work of the convoy homed at home; its vessel reads as the launch.
    let held = crate::engine(&placement_daemon).vessels.request_for(&flotilla_resources::vessel_records::VesselLaunchRequest {
        convoy: convoy.clone(),
        convoy_home: home_daemon.node_id().as_str().to_string(),
        work: "work".to_string(),
        host: "placement-host".to_string(),
    });
    placement.using::<Vessel>("flotilla").get(&held).await.expect("placement vessel");
    // The vessel is realised and its crew seated; its session's launch runs the crew's agent at
    // the placement.
    let _vessels = flotilla_orchestration_testkit::run_vessels(
        &placement,
        "flotilla",
        VesselPlanning::new(placement.clone(), "flotilla", placement_daemon.account_records())
            .with_lifecycle_reads(placement_daemon.lifecycle_reads())
            .with_federated_dependencies(&placement, flotilla_protocol::CanonicalHostId::resolved("placement-host")),
        Arc::clone(&crate::engine(&placement_daemon).vessels.lifecycles),
        placement_daemon.lifecycle_reads(),
        placement_daemon.decided_store(),
        placement_daemon.convoy_engine_path(),
    );
    let terminals = &crate::engine(&placement_daemon).terminals;
    let requested = || terminals.terminals.of_vessel(&held).expect("vessel sessions").pop();
    tokio::time::timeout(Duration::from_secs(5), async {
        while requested().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("crew terminal requested");
    let launch = requested().expect("crew terminal requested").name;
    let agent = AgentLaunch { adapter: "codex".into(), model: None, stance: "work".into() };
    terminals.ready(&launch, "receiver-session", &TerminalLaunchFacts { agent: Some(agent), ..Default::default() });
    // The session's launch and its holder vessel reach the convoy's home as the placement's log does.
    crate::engine(&home_daemon);
    flotilla_provisioning_testkit::other_home::replicate_home(
        &placement_daemon.decided_store(),
        placement_daemon.node_id().as_str(),
        &home_daemon.decided_store(),
    );

    // #2287: replication wakes delivery and acknowledgment; no timer does.
    // Await the subscribed startup scan so delivery cannot accidentally come from startup.
    let (home_task, home_ready) = spawn_pending_supervisor_turn_task(Arc::clone(&home_daemon), "flotilla".to_string());
    let _home_task = AbortOnDropHandle::new(home_task);
    let subscriptions = Arc::new(AtomicUsize::new(0));
    let before_retry = Arc::new(StdMutex::new(None));
    let (placement_task, placement_ready) = if let Some(closed_watch) = closed_watch {
        let backend = placement.clone();
        let subscriptions = Arc::clone(&subscriptions);
        let before_retry = Arc::clone(&before_retry);
        spawn_pending_supervisor_turn_task_with_watches(Arc::clone(&placement_daemon), "flotilla".to_string(), move || {
            let backend = backend.clone();
            let before_retry = Arc::clone(&before_retry);
            let attempt = subscriptions.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 1 {
                    // Capture the real store before replacing the failed watch.
                    // Assert from the test body so failures do not hide in the spawned task.
                    let stored = backend.clone().using::<TerminalSession>("flotilla").list().await?;
                    *before_retry.lock().expect("retry observation lock") = Some(stored.items);
                }
                let convoys = backend.including_replicas::<Convoy>("flotilla").watch().await?.map(|event| event.map(|_| ())).boxed();
                let sessions =
                    backend.including_replicas::<TerminalSession>("flotilla").watch().await?.map(|event| event.map(|_| ())).boxed();
                // Stand in only for a failed resource-notification stream;
                // both stores and the delivery/acknowledgment pass remain real.
                Ok(if attempt == 0 {
                    match closed_watch {
                        ClosedWatch::Convoy => (futures::stream::empty().boxed(), sessions),
                        ClosedWatch::TerminalSession => (convoys, futures::stream::empty().boxed()),
                    }
                } else {
                    (convoys, sessions)
                })
            }
        })
    } else {
        spawn_pending_supervisor_turn_task(Arc::clone(&placement_daemon), "flotilla".to_string())
    };
    let _placement_task = AbortOnDropHandle::new(placement_task);
    tokio::time::timeout(Duration::from_secs(5), async {
        home_ready.await.expect("home turn task ready");
        placement_ready.await.expect("placement turn task ready");
    })
    .await
    .expect("turn tasks subscribe and complete their startup scans");

    let request = CrewTurnIntent::builder()
        .namespace("flotilla".to_string())
        .convoy(convoy.clone())
        .source("stall-nudge-1".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("Please continue".to_string())
        .subject_revision("stall-1".to_string())
        .sender(flotilla_crew::leaf_engine::Party::fleet("nudge"))
        .expectation(flotilla_messaging::IntentExpectation::Outcome {
            // The outcome names the launch the nudge is for.
            condition: flotilla_resources::address::leaf_condition(
                &format!("work/{convoy}/work .crew.coder.phase == Done").parse().expect("nudge outcome"),
                Some("flotilla"),
                &[],
            )
            .expect("nudge condition"),
        })
        .build();
    // The receiver admits ordinary Message mutations through the real router.
    home_daemon.deliver_standing_turn(&request).await.expect("nudge accepted at convoy home");
    // A replicated dependency wakes the standing-turn task while its other
    // stream is closed; resubscription must happen before the periodic resync.
    flotilla_provisioning_testkit::other_home::replicate_home(
        &home_daemon.decided_store(),
        home_daemon.node_id().as_str(),
        &placement_daemon.decided_store(),
    );
    if closed_watch.is_some() {
        tokio::time::timeout(Duration::from_secs(5), async {
            while subscriptions.load(Ordering::SeqCst) < 2 || before_retry.lock().expect("retry capture").is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("closed dependency watch resubscribes without a timer");
    }
    // The receiver's Messages as the crew reads them.
    let records =
        flotilla_engine_messages_testkit::views(&placement_daemon.message_primitives(), &placement_daemon.resource_backend(), "flotilla")
            .await
            .expect("receiver inbox");
    assert_eq!(records.len(), 1);
    let nudge = &records[0].object;
    assert_eq!(nudge.spec.sender, "system:nudge");
    assert_eq!(nudge.spec.receiver, format!("flotilla/{convoy}/work/coder"));
    assert!(nudge.spec.body.contains("Please continue"));
    assert!(matches!(nudge.spec.expectation, MessageExpectation::Outcome { .. }));
    assert!(home.using::<Message>("flotilla").list().await.expect("sender-local inbox").items.is_empty());
}
