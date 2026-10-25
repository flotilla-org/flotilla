//! Two homes on the engine path (#3317), linked by the receiver-home router
//! and the change stream between their decided stores: a send from host A is
//! delivered on host B, the reply reaches A's role inbox by its subject, and
//! `convoy explain` on A shows B's messages from A's replica.

use std::{collections::BTreeMap, sync::Arc};

use chrono::Utc;
use flotilla_core::{in_process::InProcessDaemon, primitives::MessagePrimitives};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_inbox::InboxBackend;
use flotilla_resources::{
    Convoy, ConvoySpec, CrewSessionStatus, FleetDesignation, FleetDesignationSpec, InputMeta, MessageExpectation, MessageReference,
    MessageRelation, MessageSpec, Project, ProjectSpec, ResourceProvenance, Selector, SystemClock, TerminalBrief, TerminalCrewContext,
    TerminalSession, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus, CONVOY_LABEL, FLEET_DESIGNATION_NAME, ROLE_LABEL,
    VESSEL_LABEL,
};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::{Follower, Service};
use flotilla_state_machines::{
    machines::message::{Deliver, MessageState, Settle, Settlement},
    Transport,
};

const NS: &str = "flotilla";

/// The engine path at one daemon: its decided store homed at its node id.
async fn engine(daemon: &Arc<InProcessDaemon>, dir: &tempfile::TempDir) -> (Arc<Service>, Arc<MessagePrimitives>) {
    let backend = daemon.resource_backend();
    backend
        .clone()
        .using::<Project>(NS)
        .create(
            &InputMeta::builder().name("fleet".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("Fleet".to_string())
                .default_workflow_ref("w".to_string())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("fleet project");
    backend
        .definitions::<FleetDesignation>(NS)
        .create(
            &InputMeta::builder().name(FLEET_DESIGNATION_NAME.to_string()).build(),
            &FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
        )
        .await
        .expect("fleet designation");
    let home = daemon.node_id().as_str().to_owned();
    let service = Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), &home, flotilla_engine_store::registry().expect("registry")).expect("store"),
    );
    daemon.install_decided_store(Arc::clone(&service)).expect("decided store");
    let primitives =
        Arc::new(MessagePrimitives::open(&home, Arc::clone(&service), backend, Arc::new(SystemClock)).expect("message primitives"));
    daemon.install_message_primitives(Arc::clone(&primitives)).expect("message path");
    (service, primitives)
}

/// A home read served by another home's store, as its record socket serves one.
struct StoreReads {
    home: Arc<Service>,
    served: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl flotilla_core::primitives::ports::HomeReads for StoreReads {
    async fn find(
        &self,
        kind: flotilla_schema::KindCode,
        name: &str,
    ) -> Result<Option<flotilla_schema_sqlite::stream::ReadResult>, String> {
        self.served.lock().expect("served").push(name.to_owned());
        let bytes = self.home.query(kind, name, Timestamp::from_datetime(&Utc::now())).map_err(|error| error.to_string())?;
        match flotilla_schema_sqlite::stream::Frame::decode(&bytes).map_err(|error| error.to_string())? {
            flotilla_schema_sqlite::stream::Frame::Read { value, served_at, .. } => Ok(Some(flotilla_schema_sqlite::stream::ReadResult {
                value,
                remote: true,
                age_micros: Timestamp::from_datetime(&Utc::now()).since(served_at).max(0),
            })),
            _ => Err("not a read frame".into()),
        }
    }
}

/// One round of `from`'s stream applied at `to`, as a follower does.
fn pump(from: &Service, from_home: &str, to: &Service, cursor: &mut Option<u64>) {
    let mut frames = Vec::new();
    let next = from
        .serve(*cursor, Timestamp::from_datetime(&Utc::now()), |frame| {
            frames.push(frame.to_vec());
            Ok(())
        })
        .expect("serve");
    to.with_store(|store| {
        Follower::new(store).receive(from_home, |sink| {
            for frame in &frames {
                sink(frame)?;
            }
            Ok(())
        })
    })
    .expect("follow");
    *cursor = Some(next);
}

fn crew_session(convoy: &str) -> TerminalSessionSpec {
    TerminalSessionSpec {
        env_ref: "test-env".into(),
        role: "coder".into(),
        cwd: "/workspace".into(),
        env: Default::default(),
        pool: "cleat".into(),
        source: TerminalSessionSource::Agent {
            selector: Selector::for_capability("coding"),
            brief: TerminalBrief { path: "brief.md".into(), content: "brief".into(), artifact_digest: None, copies: Vec::new() },
            context: Box::new(TerminalCrewContext { namespace: NS.into(), convoy: convoy.into(), vessel_ref: "work".into() }),
            message: None,
        },
    }
}

fn convoy_ref(name: &str) -> MessageReference {
    MessageReference::ControlRecord {
        resource: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", NS, name),
        revision: "1".into(),
    }
}

// Behaviour (#3317): the receiver's home holds the message; the other home
// reads it from its replica, or at its home on a miss, marked remote with its
// age; a reply to a fleet role is routed by subject.
pub(super) async fn messages_are_homed_at_the_receiver_and_replies_route_by_subject(operations: &[(usize, bool, bool)]) {
    let topology = flotilla_daemon_testkit::server::spawn_in_memory_request_topology_stateful(
        super::empty_daemon_named("home-a").await,
        super::empty_daemon_named("home-b").await,
    )
    .await
    .expect("two-home router");
    let (a, b) = (&topology.leader, &topology.follower);
    let (dir_a, dir_b) = (tempfile::tempdir().expect("dir"), tempfile::tempdir().expect("dir"));
    let ((store_a, engine_a), (store_b, engine_b)) = (engine(a, &dir_a).await, engine(b, &dir_b).await);
    let (home_a, home_b) = (a.node_id().as_str().to_owned(), b.node_id().as_str().to_owned());

    // Convoy X, the subject, is homed at A; the crew it asks about lives at B.
    let convoy = |project: &str| ConvoySpec::builder().workflow_ref("w".into()).project_ref(project.into()).build();
    a.resource_backend().using::<Convoy>(NS).create(&InputMeta::builder().name("subject-x".into()).build(), &convoy(NS)).await.expect("X");
    b.resource_backend().using::<Convoy>(NS).create(&InputMeta::builder().name("crew-b".into()).build(), &convoy(NS)).await.expect("crew");
    let sessions = b.resource_backend().using::<TerminalSession>(NS);
    let labels =
        BTreeMap::from([(CONVOY_LABEL.into(), "crew-b".into()), (VESSEL_LABEL.into(), "work".into()), (ROLE_LABEL.into(), "coder".into())]);
    let session = sessions
        .create(&InputMeta::builder().name("crew-b-session".into()).labels(labels).build(), &crew_session("crew-b"))
        .await
        .expect("crew session");
    sessions
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &TerminalSessionStatus {
                phase: flotilla_resources::TerminalSessionPhase::Running,
                session_id: Some("sess-b".into()),
                crew: Some(CrewSessionStatus { id: "crew-b-id".into(), adapter: "codex".into(), model: None, stance: "coder".into() }),
                ..Default::default()
            },
        )
        .await
        .expect("running");
    // The old-store replicas each router reads to find a home.
    let replicate = |from: &Arc<InProcessDaemon>, to: &Arc<InProcessDaemon>| {
        let (from, to) = (Arc::clone(from), Arc::clone(to));
        async move {
            to.resource_backend()
                .replica_writer::<Convoy>(from.node_id().clone(), NS)
                .replace(&from.resource_backend().using::<Convoy>(NS).list().await.expect("convoys"), Utc::now())
                .await
                .expect("convoy replica");
            to.resource_backend()
                .replica_writer::<TerminalSession>(from.node_id().clone(), NS)
                .replace(&from.resource_backend().using::<TerminalSession>(NS).list().await.expect("sessions"), Utc::now())
                .await
                .expect("session replica");
        }
    };
    replicate(b, a).await;
    replicate(a, b).await;

    // A sends as turn-rules to B's crew, about convoy X.
    let request = flotilla_resources::message_record_name("flotilla/crew-b/work/coder", "system:turn-rules", "rule:checks@1");
    let intent = MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver("flotilla/crew-b/work/coder".into())
        .relation(MessageRelation::System)
        .body("checks settled; please review".into())
        .references(vec![convoy_ref("subject-x")])
        .subject(convoy_ref("subject-x"))
        .expectation(MessageExpectation::Reply)
        .build();
    a.publish_message(NS, &request, &intent).await.expect("routed send");
    let homed = |engine: &MessagePrimitives, record: &str| {
        engine.messages().expect("messages").into_iter().find(|(_, message)| message.name().ends_with(&format!("/message:{record}")))
    };
    let (home, sent) = homed(&engine_b, &request).expect("the message is created at the receiver's home");
    assert_eq!(home, home_b);
    assert_eq!(sent.spec().sender.as_str(), "fleet:lab/role:turn-rules");
    assert!(homed(&engine_a, &request).is_none(), "the sender's home authors nothing");

    // B delivers it to the crew's AgentSession, receipt first.
    let consumer = engine_b.register("crew-b-session", Timestamp(1)).await.expect("consumer");
    let inbox_b = engine_b.inbox();
    inbox_b.bind(sent.spec().inbox.clone(), consumer.clone()).await.expect("bind");
    use futures::StreamExt;
    for _ in 0..8 {
        let mut pending = inbox_b.pending(&sent.spec().inbox, &consumer, &flotilla_inbox::Rules(Vec::new())).await.expect("pending");
        assert!(pending.next().await.expect("pending message").is_ok());
    }
    inbox_b.receive(&consumer, sent.name(), Transport::Turn).await.expect("receipt");
    inbox_b.deliver(sent.name(), Deliver { receiver: consumer.clone(), transport: Transport::Turn }).await.expect("deliver");
    assert!(matches!(homed(&engine_b, &request).expect("sent").1.state(), MessageState::Awaiting { .. }));
    // A's replica has not caught up: the reply's admission at A reads the
    // original at B, its home, marked remote with its age.
    let reads = Arc::new(StoreReads { home: Arc::clone(&store_b), served: Default::default() });
    engine_a.install_home_reads(reads.clone()).expect("home reads");
    let reply = flotilla_resources::message_record_name("system:turn-rules", "flotilla/crew-b/work/coder", "reply@1");
    let answer = MessageSpec::builder()
        .sender("flotilla/crew-b/work/coder".into())
        .receiver("system:turn-rules".into())
        .relation(MessageRelation::Peer)
        .body("reviewed".into())
        .in_reply_to(request.clone())
        .build();
    b.publish_message(NS, &reply, &answer).await.expect("routed reply");
    // Routing by subject and the admission each read the original at B.
    let served = reads.served.lock().expect("served").clone();
    assert!(!served.is_empty() && served.iter().all(|name| name == sent.name()), "the misses were read at B: {served:?}");
    let (reply_home, replied) = homed(&engine_a, &reply).expect("the reply is homed at X's home");
    assert_eq!(reply_home, home_a);
    assert_eq!(replied.spec().inbox.as_str(), "fleet:lab/role:turn-rules");
    assert_eq!(replied.spec().in_reply_to.as_ref().map(|original| original.home.as_str()), Some(home_b.as_str()));
    assert_eq!(*replied.state(), MessageState::Pending, "nobody is bound to turn-rules yet");
    let read = engine_a.read_message(&request, Some(sent.spec().inbox.clone())).await.expect("routed read");
    assert!(read.remote && read.age_micros >= 0 && read.home == home_b, "{read:?}");

    let (mut b_to_a, mut a_to_b) = (None, None);
    pump(&store_b, &home_b, &store_a, &mut b_to_a);

    // A reads B's message from its replica, delivered, in convoy explain.
    let (replica_home, replica) = homed(&engine_a, &request).expect("A's replica holds B's message");
    assert_eq!((replica_home.as_str(), replica.state()), (home_b.as_str(), &MessageState::Awaiting { receiver: consumer.clone() }));
    let view = engine_a.view(NS, &request).await.expect("view").expect("present");
    assert!(matches!(view.provenance, ResourceProvenance::Replica { ref origin_root, .. } if origin_root.as_str() == home_b));
    let explained = a.explain_convoy_internal(Some(NS), "crew-b").await.expect("explain on A");
    let shown = explained.messages.iter().find(|message| message.name == request).expect("convoy explain shows B's message");
    assert_eq!((shown.phase.as_str(), shown.session.as_deref()), ("delivered", Some("sess-b")));

    let local = engine_a.read_message(&request, None).await.expect("replica read");
    assert!(!local.remote && local.home == home_b, "after following B, A reads its replica: {local:?}");

    // B observes the reply through its replica and settles its message.
    pump(&store_a, &home_a, &store_b, &mut a_to_b);
    assert!(homed(&engine_b, &reply).is_some(), "B follows A's log");
    engine_b.world().update(|world| world.reply.put(&flotilla_state_machines::Ref::new(sent.name()), ()));
    inbox_b.settle(sent.name(), Settle { by: Settlement::Answered }).await.expect("answered");
    assert!(matches!(homed(&engine_b, &request).expect("sent").1.state(), MessageState::Settled { .. }));
    use flotilla_core::primitives::sweep::OpenMessages;
    use flotilla_state_machines::machines::{awaiting_reply::AwaitingReplyLabel, message::MessageRef};
    let mut observer = OpenMessages::default();
    for (step, &(action, late_record, restart)) in operations.iter().enumerate() {
        let name = flotilla_resources::message_record_name(&intent.receiver, &intent.sender, &format!("cancel-{step}"));
        let mut cancel = intent.clone();
        cancel.deadline = (action == 2).then(|| Utc::now() - chrono::Duration::seconds(1));
        // Exercise receiver admission through the router, then resume the sender's
        // record write separately to model a delayed or retried send continuation.
        let mut events = a.subscribe();
        let command = flotilla_protocol::Command::builder()
            .action(flotilla_protocol::CommandAction::ResourceApply {
                namespace: NS.into(),
                document: serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"Message", "metadata":{"name":name}, "spec":cancel}),
            })
            .build();
        let id = topology.client.execute(command).await.expect("routed cancellable admission");
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let flotilla_protocol::DaemonEvent::CommandFinished { command_id, node_id, result, .. } =
                    events.recv().await.expect("admission event")
                {
                    if command_id == id {
                        assert_eq!(node_id, *b.node_id());
                        break result;
                    }
                }
            }
        })
        .await
        .expect("receiver admission finishes");
        assert!(matches!(result, flotilla_protocol::CommandValue::ResourceObject(_)), "{result:?}");

        let (_, message) = homed(&engine_b, &name).expect("receiver admitted");
        let reference = MessageRef::new(message.name(), &home_b);
        assert!(engine_a.awaiting().get(&reference).is_none(), "sender continuation has not written its record yet");
        if !late_record {
            engine_a.await_reply(NS, &name, &cancel, &home_b, "reply", None).await.expect("sender record");
        }
        pump(&store_b, &home_b, &store_a, &mut b_to_a);
        engine_a.sweep(NS, Timestamp::from_datetime(&Utc::now()), &mut observer).await.expect("live observation");
        match action {
            0 => {
                let next = format!("{name}-successor");
                let mut successor = cancel.clone();
                successor.expectation = MessageExpectation::None;
                successor.supersedes = Some(name.clone());
                a.publish_message(NS, &next, &successor).await.expect("routed supersession");
            }
            1 => engine_b.withdraw(NS, &name, "crew-b").await.expect("receiver withdrawal"),
            _ => {
                engine_b.sweep(NS, Timestamp::from_datetime(&Utc::now()), &mut OpenMessages::default()).await.expect("receiver expiry");
            }
        }
        assert!(matches!(homed(&engine_b, &name).expect("receiver message").1.state(), MessageState::Dropped { .. }));
        if !late_record {
            assert_eq!(
                engine_a.awaiting().get(&reference).expect("sender record").label(),
                AwaitingReplyLabel::Open,
                "no remote write before replication"
            );
        }
        pump(&store_b, &home_b, &store_a, &mut b_to_a);
        if late_record {
            engine_a.sweep(NS, Timestamp::from_datetime(&Utc::now()), &mut observer).await.expect("drop arrives before the record");
            engine_a.await_reply(NS, &name, &cancel, &home_b, "reply", None).await.expect("late sender record");
            assert_eq!(engine_a.awaiting().get(&reference).expect("late record").label(), AwaitingReplyLabel::Withdrawn);
        }
        if restart {
            observer = OpenMessages::default();
        }
        for _ in 0..2 {
            engine_a.sweep(NS, Timestamp::from_datetime(&Utc::now()), &mut observer).await.expect("sender observes receiver drop");
            assert_eq!(engine_a.awaiting().get(&reference).expect("sender record").label(), AwaitingReplyLabel::Withdrawn);
            assert!(!engine_a.await_reply(NS, &name, &cancel, &home_b, "reply", None).await.expect("repeat send"));
            assert_eq!(engine_a.awaiting().get(&reference).expect("repeated record").label(), AwaitingReplyLabel::Withdrawn);
            assert_eq!(
                engine_a.awaiting().get(&MessageRef::new(sent.name(), &home_b)).expect("answered record").label(),
                AwaitingReplyLabel::Answered
            );
        }
    }
}
