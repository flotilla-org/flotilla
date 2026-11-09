use flotilla_resources::terminal_records::{AgentLaunch, TerminalLaunchFacts, TerminalRecords as _};
use flotilla_store_sqlite::SqliteBackend;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_crew::leaf_engine::CrewTurnIntent;
use flotilla_crew::reconcilers::vessel::VesselReconciler;
use flotilla_daemon::runtime::spawn_pending_supervisor_turn_task;
use flotilla_daemon::runtime::spawn_pending_supervisor_turn_task_with_watches;
use flotilla_daemon_testkit::server::spawn_in_memory_request_topology_stateful;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::{HostName, NodeId};
use flotilla_resources::{
    ClaimExit, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, EnvironmentSpec,
    ExitDeclaration, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, HostDirectEnvironmentSpec, InputMeta, Message,
    MessageExpectation, Resource, Selector, TerminalSession, Vessel, VesselRequirement, VesselSpec, WorkPhase, WorkState, WorkflowSnapshot,
    ACTUATOR_SOURCE_ROOT_ANNOTATION,
};
use flotilla_store::{controller::Reconciler, ResourceBackend};
use futures::StreamExt;
use tokio_util::task::AbortOnDropHandle;

fn meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
}

async fn replicate<T: Resource>(source: &ResourceBackend, destination: &ResourceBackend, source_root: &NodeId) {
    let objects = source.clone().using::<T>("flotilla").list().await.expect("source list");
    destination.replica_writer::<T>(source_root.clone(), "flotilla").replace(&objects, Utc::now()).await.expect("replicate resources");
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
    let home = ResourceBackend::new(SqliteBackend::open_in_memory().expect("home store")).with_local_root(NodeId::new("home"));
    let placement =
        ResourceBackend::new(SqliteBackend::open_in_memory().expect("placement store")).with_local_root(NodeId::new("placement"));
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

    // Keep the real request endpoints alive: producers publish ordinary
    // receiver-homed mutations rather than enqueueing at the convoy authority.
    let _topology = spawn_in_memory_request_topology_stateful(home_daemon.as_ref().clone(), placement_daemon.as_ref().clone())
        .await
        .expect("cross-host Message mutation router");

    let convoys = home.clone().using::<Convoy>("flotilla");
    let created =
        convoys.create(&meta("nudge-convoy"), &ConvoySpec::builder().workflow_ref("wf".to_string()).build()).await.expect("convoy");
    convoys
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus {
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
                crew_work: BTreeMap::from([(
                    "work".to_string(),
                    BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("working crew");
    replicate::<Convoy>(&home, &placement, home_daemon.node_id()).await;
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
        &home_daemon.decided_store().expect("home decided store"),
        home_daemon.node_id().as_str(),
        &placement_daemon.decided_store().expect("placement decided store"),
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
    let vessel = placement
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder()
                .name("nudge-convoy-work".to_string())
                .annotations(BTreeMap::from([(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), home_daemon.node_id().to_string())]))
                .build(),
            &VesselSpec {
                convoy_ref: "nudge-convoy".to_string(),
                vessel_name: "work".to_string(),
                fulfilment_kind_ref: "placement-policy".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("placement vessel");
    let reconciler = VesselReconciler::new(placement.clone(), "flotilla")
        .with_lifecycle_reads(placement_daemon.lifecycle_reads())
        .with_federated_dependencies(&placement, flotilla_protocol::CanonicalHostId::resolved("placement-host"));
    // The vessel requests its crew's session; its launch runs the crew's agent at the placement.
    reconciler.prepare(&vessel).await.expect("vessel dependencies");
    let terminals = &crate::engine(&placement_daemon).terminals;
    let launch = terminals.terminals.of_vessel("nudge-convoy-work").expect("vessel sessions").pop().expect("crew terminal requested").name;
    let agent = AgentLaunch { adapter: "codex".into(), model: None, stance: "work".into() };
    terminals.ready(&launch, "receiver-session", &TerminalLaunchFacts { agent: Some(agent), ..Default::default() });
    let convoy = convoys.get("nudge-convoy").await.expect("working convoy");
    let mut status = convoy.status.expect("working status");
    status.crew_work.get_mut("work").expect("work crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Stalled;
    convoys.update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &status).await.expect("stall crew");
    // The session's launch reaches the convoy's home as the placement's log does, and its
    // holder vessel as the placement's records replicate.
    crate::engine(&home_daemon);
    flotilla_provisioning_testkit::other_home::replicate_home(
        &placement_daemon.decided_store().expect("placement decided store"),
        placement_daemon.node_id().as_str(),
        &home_daemon.decided_store().expect("home decided store"),
    );
    replicate::<Vessel>(&placement, &home, placement_daemon.node_id()).await;

    // #2287: replication wakes delivery and acknowledgment well before the 300s resync.
    // Await the subscribed startup scan so delivery cannot accidentally come from startup.
    let (home_task, home_ready) =
        spawn_pending_supervisor_turn_task(Arc::clone(&home_daemon), "flotilla".to_string(), Duration::from_secs(300));
    let _home_task = AbortOnDropHandle::new(home_task);
    let subscriptions = Arc::new(AtomicUsize::new(0));
    let before_retry = Arc::new(StdMutex::new(None));
    let (placement_task, placement_ready) = if let Some(closed_watch) = closed_watch {
        let backend = placement.clone();
        let subscriptions = Arc::clone(&subscriptions);
        let before_retry = Arc::clone(&before_retry);
        spawn_pending_supervisor_turn_task_with_watches(
            Arc::clone(&placement_daemon),
            "flotilla".to_string(),
            Duration::from_secs(300),
            move || {
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
            },
        )
    } else {
        spawn_pending_supervisor_turn_task(Arc::clone(&placement_daemon), "flotilla".to_string(), Duration::from_secs(300))
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
        .convoy("nudge-convoy".to_string())
        .source("stall-nudge-1".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("Please continue".to_string())
        .subject_revision("stall-1".to_string())
        .sender("system:nudge".into())
        .expectation(MessageExpectation::Outcome {
            condition: "work/nudge-convoy/work .crew.coder.phase == Done".parse().expect("nudge outcome"),
        })
        .build();
    // The receiver admits ordinary Message mutations through the real router.
    home_daemon.deliver_standing_turn(&request).await.expect("nudge accepted at convoy home");
    // A replicated dependency wakes the standing-turn task while its other
    // stream is closed; resubscription must happen before the periodic resync.
    replicate::<Convoy>(&home, &placement, home_daemon.node_id()).await;
    if closed_watch.is_some() {
        tokio::time::timeout(Duration::from_secs(5), async {
            while subscriptions.load(Ordering::SeqCst) < 2 || before_retry.lock().expect("retry capture").is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("closed dependency watch resubscribes before the 300-second resync");
    }
    let records = placement.using::<Message>("flotilla").list().await.expect("receiver inbox").items;
    assert_eq!(records.len(), 1);
    let nudge = &records[0];
    assert_eq!(nudge.spec.sender, "system:nudge");
    assert_eq!(nudge.spec.receiver, "flotilla/nudge-convoy/work/coder");
    assert!(nudge.spec.body.contains("Please continue"));
    assert!(matches!(nudge.spec.expectation, MessageExpectation::Outcome { .. }));
    assert!(home.using::<Message>("flotilla").list().await.expect("sender-local inbox").items.is_empty());
}
