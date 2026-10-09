use super::*;

#[tokio::test]
async fn turn_admission_returns_the_canonical_suppressed_message() {
    use flotilla_resources::{Message, MessageExpectation, MessageReference, MessageStatusPatch, ResolvedMessageReceiver};

    use crate::leaf_engine::CrewTurnIntent;
    let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
    probe.fail.store(false, Ordering::SeqCst);
    let reference = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "crew"),
        revision: "revision".into(),
    };
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("first".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("reply to this subject".into())
        .subject_revision("revision".into())
        .sender("system:turn-rules".into())
        .references(vec![reference.clone()])
        .message_subject(reference)
        .expectation(MessageExpectation::Reply)
        .build();
    let first = crew.deliver_turn(&request).await.expect("first admission");
    flotilla_resources::apply_status_patch(
        &backend.using::<Message>("flotilla"),
        &first.message.name,
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew-id".into())
                .session("session".into())
                .delivered_at(Utc::now())
                .evidence("transport acceptance".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .expect("open receipt");
    let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status;
    let staging_calls = probe.calls.load(Ordering::SeqCst);
    let mut successor = request.clone();
    successor.source = "second".into();
    successor.brief = "duplicate turn".into();
    let suppressed = crew.deliver_turn(&successor).await.expect("suppression admission");
    assert!(first.new_turn);
    assert!(!suppressed.new_turn);
    assert_eq!(suppressed.message, first.message);
    assert_eq!(
        backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status,
        before,
        "suppression must not reopen workflow state for an absent new turn"
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), staging_calls);
    assert_eq!(backend.using::<Message>("flotilla").list().await.expect("inbox").items.len(), 1);
}

// A queued follow-up continues workflow only after Message acceptance evidence;
// the authority watch consumes its reference without creating another input.
#[tokio::test]
async fn follow_up_reference_releases_from_message_evidence() {
    use flotilla_resources::{Message, MessageStatusPatch, ResolvedMessageReceiver};
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    crew.resume("flotilla", "crew", "continue after this turn", Some("work"), Some("coder")).await.unwrap();
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let pending = convoys.get("crew").await.unwrap().status.unwrap().crew_work["work"]["coder"].pending_follow_up.clone().unwrap();
    crew.reconcile_pending_supervisor_turns_once("flotilla").await.unwrap();
    assert_eq!(convoys.get("crew").await.unwrap().status.unwrap().crew_work["work"]["coder"].pending_follow_up.as_ref(), Some(&pending));
    flotilla_resources::apply_status_patch(
        &backend.using::<Message>("flotilla"),
        &pending.name,
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew-id".into())
                .session("session".into())
                .delivered_at(Utc::now())
                .evidence("transport acceptance".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .unwrap();
    crew.reconcile_pending_supervisor_turns_once("flotilla").await.unwrap();
    let state = convoys.get("crew").await.unwrap().status.unwrap().crew_work["work"]["coder"].clone();
    assert!(state.pending_follow_up.is_none());
    assert_eq!(state.resume_brief_id.as_deref(), Some(pending.name.as_str()));
    assert_eq!(state.message.as_deref(), Some("continue after this turn"));
    assert_eq!(backend.using::<Message>("flotilla").list().await.unwrap().items.len(), 1);
}

// Follow-up references resolve their target namespace, even when a same-name
// Message in the Convoy namespace has different delivery evidence and intent.
#[tokio::test]
async fn follow_up_reference_uses_target_namespace() {
    use flotilla_resources::{Message, MessageStatusPatch, ResolvedMessageReceiver};
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    crew.resume("flotilla", "crew", "local decoy", Some("work"), Some("coder")).await.expect("queue local follow-up");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get("crew").await.expect("convoy");
    let mut status = convoy.status.expect("crew status");
    let pending = status
        .crew_work
        .get_mut("work")
        .expect("vessel")
        .get_mut("coder")
        .expect("coder")
        .pending_follow_up
        .as_mut()
        .expect("queued reference");
    pending.namespace = "target".into();
    let name = pending.name.clone();
    convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("retarget follow-up");
    let local = backend.using::<Message>("flotilla").get(&name).await.expect("local decoy");
    let mut intent = local.spec;
    intent.body = "target continuation".into();
    let messages = backend.using::<Message>("target");
    messages.create(&InputMeta::builder().name(name.clone()).build(), &intent).await.expect("target message");
    flotilla_resources::apply_status_patch(
        &messages,
        &name,
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew-id".into())
                .session("session".into())
                .delivered_at(Utc::now())
                .evidence("transport acceptance".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .expect("target delivery evidence");
    crew.reconcile_pending_supervisor_turns_once("flotilla").await.expect("continue from target");
    let state = convoys.get("crew").await.expect("continued convoy").status.expect("continued status").crew_work["work"]["coder"].clone();
    assert!(state.pending_follow_up.is_none());
    assert_eq!(state.message.as_deref(), Some("target continuation"));
}

// Receiver admission can suppress a turn before its Message has replicated to
// the producer. The acknowledgement must restore speculative workflow activation.
#[tokio::test]
async fn remote_suppression_restores_workflow_before_replication() {
    use flotilla_resources::{
        Message, MessageExpectation, MessageInbox, MessageReference, MessageRelation, MessageSpec, MessageStatusPatch,
        ResolvedMessageReceiver,
    };

    use crate::leaf_engine::{CrewTurnIntent, ResourceIntentPublisher};
    // Boundary stand-in for the ordinary cross-host resource mutation endpoint.
    struct ReceiverPublisher(MessageInbox);
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverPublisher {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            assert_eq!(document["kind"], "Message");
            let spec: MessageSpec = serde_json::from_value(document["spec"].clone()).unwrap();
            let admission = self
                .0
                .accept(&InputMeta::builder().name(document["metadata"]["name"].as_str().unwrap().into()).build(), &spec, Utc::now())
                .await
                .unwrap();
            let record = match admission {
                flotilla_resources::MessageAdmission::Accepted(record) => record,
                flotilla_resources::MessageAdmission::Suppressed { predecessor } => predecessor,
            };
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, record.metadata.name))
        }
    }
    let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
    probe.fail.store(false, Ordering::SeqCst);
    let remote = ResourceBackend::InMemory(Default::default());
    let inbox = MessageInbox::new(remote.clone(), "flotilla");
    let subject = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "crew"),
        revision: "revision".into(),
    };
    let existing = MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver("flotilla/crew/work/coder".into())
        .relation(MessageRelation::System)
        .body("prior open request".into())
        .references(vec![subject.clone()])
        .subject(subject.clone())
        .expectation(MessageExpectation::Reply)
        .build();
    inbox.accept(&InputMeta::builder().name("canonical".into()).build(), &existing, Utc::now()).await.unwrap();
    flotilla_resources::apply_status_patch(
        &remote.using::<Message>("flotilla"),
        "canonical",
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew-id".into())
                .session("session".into())
                .delivered_at(Utc::now())
                .evidence("accepted at receiver".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .unwrap();
    let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(ReceiverPublisher(inbox));
    crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
    let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status;
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("second".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("duplicate request".into())
        .subject_revision("revision".into())
        .sender("system:turn-rules".into())
        .references(vec![subject.clone()])
        .message_subject(subject)
        .expectation(MessageExpectation::Reply)
        .build();
    let admission = crew.deliver_turn(&request).await.unwrap();
    assert!(!admission.new_turn);
    assert_eq!(admission.message.name, "canonical");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status, before);
    assert_eq!(remote.using::<Message>("flotilla").list().await.unwrap().items.len(), 1);
    assert!(backend.using::<Message>("flotilla").list().await.unwrap().items.is_empty());
}

// The receiver's ordinary resource mutation result is canonical even before
// replication. Suppression must preserve settled workflow state in both cases.
#[tokio::test]
async fn turn_admission_uses_canonical_receiver_result_without_reopening_work() {
    use flotilla_resources::{
        Message, MessageExpectation, MessageInbox, MessageReference, MessageRelation, MessageSpec, MessageStatusPatch,
        ResolvedMessageReceiver,
    };

    use crate::leaf_engine::{CrewTurnIntent, ResourceIntentPublisher};
    struct ReceiverPublisher(MessageInbox);
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverPublisher {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            assert_eq!(document["kind"], "Message");
            let spec = serde_json::from_value(document["spec"].clone()).unwrap();
            let admission = self
                .0
                .accept(&InputMeta::builder().name(document["metadata"]["name"].as_str().unwrap().into()).build(), &spec, Utc::now())
                .await
                .unwrap();
            let record = match admission {
                flotilla_resources::MessageAdmission::Accepted(record) => record,
                flotilla_resources::MessageAdmission::Suppressed { predecessor } => predecessor,
            };
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, record.metadata.name))
        }
    }
    for visible in [true, false] {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
        probe.fail.store(false, Ordering::SeqCst);
        let receiver = if visible { backend.clone() } else { ResourceBackend::InMemory(Default::default()) };
        let inbox = MessageInbox::new(receiver.clone(), "flotilla");
        let subject =
            MessageReference::ChangeRequest { service: "github".into(), scope: "org/repo".into(), number: 1, revision: "head".into() };
        let intent = MessageSpec::builder()
            .sender("system:turn-rules".into())
            .receiver("flotilla/crew/work/coder".into())
            .relation(MessageRelation::System)
            .body("prior open request".into())
            .references(vec![subject.clone()])
            .subject(subject.clone())
            .expectation(MessageExpectation::Reply)
            .build();
        inbox.accept(&InputMeta::builder().name("canonical".into()).build(), &intent, Utc::now()).await.unwrap();
        flotilla_resources::apply_status_patch(
            &receiver.using::<Message>("flotilla"),
            "canonical",
            &MessageStatusPatch::Delivered {
                receiver: ResolvedMessageReceiver::builder()
                    .crew_id("crew-id".into())
                    .session("session".into())
                    .delivered_at(Utc::now())
                    .evidence("receiver receipt".into())
                    .build(),
                at: Utc::now(),
            },
        )
        .await
        .unwrap();
        let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(ReceiverPublisher(inbox));
        if !visible {
            crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
        }
        let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status;
        let staging = probe.calls.load(Ordering::SeqCst);
        let request = CrewTurnIntent::builder()
            .namespace("flotilla".into())
            .convoy("crew".into())
            .source("review".into())
            .vessel("work".into())
            .role("coder".into())
            .brief("duplicate request".into())
            .subject_revision("head".into())
            .references(vec![subject.clone()])
            .message_subject(subject)
            .sender("system:turn-rules".into())
            .build();
        let admission = crew.deliver_turn(&request).await.unwrap();
        assert!(!admission.new_turn);
        assert_eq!(admission.message.name, "canonical");
        assert_eq!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status, before);
        assert_eq!(receiver.using::<Message>("flotilla").list().await.unwrap().items.len(), 1);
        if visible {
            assert_eq!(probe.calls.load(Ordering::SeqCst), staging);
        } else {
            assert!(probe.calls.load(Ordering::SeqCst) > staging, "the remote case must exercise speculative activation");
            assert!(backend.using::<Message>("flotilla").list().await.unwrap().items.is_empty());
        }
    }
}

#[tokio::test]
async fn unknown_controller_sender_has_a_creatable_system_reply() {
    use flotilla_resources::Message;

    use crate::{in_process::InProcessDaemon, leaf_engine::CrewTurnIntent};

    let (crew, backend, probe, config) = fixture(CrewWorkPhase::Done).await;
    probe.fail.store(false, Ordering::SeqCst);
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("legacy".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("legacy controller turn".into())
        .subject_revision("legacy-head".into())
        .sender(flotilla_resources::legacy_message_sender(&CrewMessageSender::Unknown).0)
        .build();
    let admission = crew.deliver_turn(&request).await.expect("controller intent");
    let original = backend.using::<Message>("flotilla").get(&admission.message.name).await.expect("receiver intent");
    std::fs::write(config.path().join("daemon.toml"), "machine_id = \"legacy-message-reply-test\"\n").expect("reply daemon identity");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(config.path())),
        fake_discovery(false),
        HostName::new("host"),
        backend.clone(),
    )
    .await;
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

// Failed durable admission must not reopen settled work. Exercise an external
// publication error and a receiver collision through the real crew service.
#[tokio::test]
async fn turn_publication_errors_restore_the_owned_activation() {
    use crate::leaf_engine::{CrewTurnIntent, ResourceIntentPublisher};
    struct FailedPublisher;
    #[async_trait]
    impl ResourceIntentPublisher for FailedPublisher {
        async fn publish(self: Arc<Self>, _: &str, _: serde_json::Value) -> Result<ResourceRef, String> {
            Err("receiver publication unavailable".into())
        }
    }
    struct AdmissionCollision {
        probe: Arc<StagingProbe>,
        backend: ResourceBackend,
    }
    #[async_trait]
    impl WorkCredentialReconciler for AdmissionCollision {
        async fn reconcile(&self, namespace: &str, environment: &str) -> Result<(), String> {
            self.probe.reconcile(namespace, environment).await?;
            let receiver = "flotilla/crew/work/coder";
            let sender = "system:legacy";
            let name = flotilla_resources::message_record_name(receiver, sender, "turn-delivery:failure:failure-head");
            let spec = flotilla_resources::MessageSpec::builder()
                .sender(sender.into())
                .receiver(receiver.into())
                .relation(flotilla_resources::MessageRelation::System)
                .body("concurrent intent".into())
                .build();
            flotilla_resources::MessageInbox::new(self.backend.clone(), namespace)
                .accept(&InputMeta::builder().name(name).build(), &spec, Utc::now())
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
    }
    for external in [false, true] {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
        probe.fail.store(false, Ordering::SeqCst);
        let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(FailedPublisher);
        if external {
            crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
        } else {
            *crew.work_credential_reconciler.write().await =
                Some(Arc::new(AdmissionCollision { probe: probe.clone(), backend: backend.clone() }));
        }
        let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("before admission").status;
        let request = CrewTurnIntent::builder()
            .namespace("flotilla".into())
            .convoy("crew".into())
            .source("failure".into())
            .vessel("work".into())
            .role("coder".into())
            .subject_revision("failure-head".into())
            .brief("continue".into())
            .sender(flotilla_resources::legacy_message_sender(&CrewMessageSender::Unknown).0)
            .build();
        let error = crew.deliver_turn(&request).await.expect_err("durable admission fails");
        assert!(error.contains(if external { "receiver publication unavailable" } else { "different intent" }), "{error}");
        assert!(probe.calls.load(Ordering::SeqCst) > 0, "exercise activation before admission");
        assert_eq!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("after failed admission").status, before);
        let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("receiver inbox").items;
        assert_eq!(records.len(), usize::from(!external));
        if !external {
            assert_eq!(records[0].spec.body, "concurrent intent");
        }
    }
}

#[tokio::test]
async fn resume_and_withdraw_wait_for_follow_up_replication() {
    for withdraw in [false, true] {
        let (crew, backend, _, _config) = fixture(CrewWorkPhase::Done).await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        apply_resource_status_patch(
            &convoys,
            "crew",
            &ConvoyStatusPatch::QueueMessageFollowUp {
                vessel: "work".into(),
                role: "coder".into(),
                message: Some(ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "not-replicated")),
            },
        )
        .await
        .expect("pending continuation");
        let before = convoys.get("crew").await.expect("before command").status;
        let error = if withdraw {
            crew.withdraw_pending_brief("flotilla", "crew").await.expect_err("withdraw awaits replication")
        } else {
            crew.resume("flotilla", "crew", "replacement", None, None).await.expect_err("resume awaits replication")
        };
        assert!(error.contains("follow-up Message awaits replication"), "{error}");
        assert_eq!(convoys.get("crew").await.expect("unchanged workflow").status, before);
        assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.is_empty());
    }
}

#[tokio::test]
async fn replica_withdraw_preserves_receiver_submission_on_race() {
    use crate::leaf_engine::ResourceIntentPublisher;
    struct SubmissionRace {
        receiver: ResourceBackend,
    }
    #[async_trait]
    impl ResourceIntentPublisher for SubmissionRace {
        async fn publish(self: Arc<Self>, _: &str, _: serde_json::Value) -> Result<ResourceRef, String> {
            panic!("withdraw never publishes intent");
        }
        async fn patch_status(
            self: Arc<Self>,
            namespace: &str,
            kind: &str,
            name: &str,
            status: serde_json::Value,
            expected: &str,
        ) -> Result<(), String> {
            let messages = self.receiver.using::<flotilla_resources::Message>(namespace);
            let current = messages.get(name).await.expect("receiver intent");
            let mut next = current.status.expect("receiver status");
            next.submission = Some(
                flotilla_resources::MessageSubmission::builder()
                    .batch_id("racing-batch".into())
                    .crew_id("actual-crew".into())
                    .session("actual-session".into())
                    .started_at(Utc::now())
                    .members(vec![name.into()])
                    .build(),
            );
            messages.update_status(name, &current.metadata.resource_version, &next).await.expect("receiver begins submission");
            flotilla_resources::patch_resource_status_if_version(&self.receiver, namespace, kind, name, status, expected)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    }
    let (crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    let receiver = ResourceBackend::InMemory(InMemoryBackend::default());
    let spec = flotilla_resources::MessageSpec::builder()
        .sender("principal:operator".into())
        .receiver("flotilla/crew/work/coder".into())
        .relation(flotilla_resources::MessageRelation::Supervisor)
        .body("retain this brief".into())
        .build();
    flotilla_resources::MessageInbox::new(receiver.clone(), "flotilla")
        .accept(&InputMeta::builder().name("racing-message".into()).build(), &spec, Utc::now())
        .await
        .expect("receiver admission");
    backend
        .replica_writer::<flotilla_resources::Message>(flotilla_protocol::NodeId::new("receiver"), "flotilla")
        .replace(&receiver.using::<flotilla_resources::Message>("flotilla").list().await.expect("receiver snapshot"), Utc::now())
        .await
        .expect("lagging sender replica");
    let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(SubmissionRace { receiver: receiver.clone() });
    crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
    let error = crew.withdraw_pending_brief("flotilla", "crew").await.expect_err("stale withdrawal refused");
    assert!(error.contains("status changed"), "{error}");
    assert!(error.contains("retain this brief"), "partial failure retains the body: {error}");
    let retained = receiver.using::<flotilla_resources::Message>("flotilla").get("racing-message").await.expect("retained intent");
    let status = retained.status.expect("receiver evidence");
    assert!(!status.phase.is_terminal());
    assert_eq!(status.submission.expect("submission preserved").session, "actual-session");
}

#[tokio::test]
async fn fresh_idle_admission_keeps_completion_and_delivery_evidence_receiver_owned() {
    for completion_pending in [false, true] {
        for degraded in [false, true] {
            let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
            probe.fail.store(false, Ordering::SeqCst);
            let sessions = backend.using::<ResourceTerminalSession>("flotilla");
            let holder = sessions.get("session").await.expect("holder");
            let now = Utc::now();
            let status = flotilla_resources::TerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                session_id: Some("actual-session".into()),
                attention: Some(flotilla_resources::TerminalAttention {
                    state: TerminalAttentionState::Idle,
                    as_of: now,
                    source: flotilla_resources::TerminalAttentionSource::Screen,
                }),
                completion_pending: completion_pending.then(|| CrewCompletionPending {
                    message: Some("previous completion".into()),
                    disposition: None,
                    decision_ledger_ref: None,
                    force: false,
                    principal_ref: None,
                    attempted_at: now,
                    authority: "crew".into(),
                    last_error: "awaiting settlement".into(),
                }),
                degraded: degraded.then(|| flotilla_resources::TerminalSessionDegradedCondition {
                    reason: flotilla_resources::TERMINAL_DELIVERY_UNCONFIRMED_REASON.into(),
                    message: "held earlier input".into(),
                    message_id: Some("earlier".into()),
                    consecutive_failures: 1,
                    observed_at: now,
                }),
                ..Default::default()
            };
            sessions
                .update_status("session", &holder.metadata.resource_version, &status)
                .await
                .expect("fresh idle with legacy obligations");
            crew.resume("flotilla", "crew", "next admitted brief", None, None).await.expect("admit at idle boundary");
            assert_eq!(sessions.get("session").await.expect("unchanged receiver evidence").status, Some(status));
            let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
            assert_eq!(records.len(), 1);
            assert!(records[0].status.as_ref().expect("admission status").resolved_receiver.is_none());
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1, "workflow resumes but does not deliver");
        }
    }
}

// A controller replay must keep the original Message identity and settled
// workflow state even after terminal audit retention has removed its body.
#[tokio::test]
async fn turn_admission_replays_compacted_message_without_reopening_work() {
    use flotilla_resources::{Message, MessageInbox, MessagePhase, MessageRelation, MessageSpec};

    use crate::leaf_engine::CrewTurnIntent;
    let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
    probe.fail.store(false, Ordering::SeqCst);
    let intent = MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver("flotilla/crew/work/coder".into())
        .relation(MessageRelation::System)
        .body("review".into())
        .build();
    let name = flotilla_resources::message_record_name(&intent.receiver, &intent.sender, "turn-delivery:review:revision");
    let inbox = MessageInbox::new(backend.clone(), "flotilla").with_audit_retention_days(1);
    inbox.accept(&InputMeta::builder().name(name.clone()).build(), &intent, Utc::now()).await.unwrap();
    let messages = backend.using::<Message>("flotilla");
    let record = messages.get(&name).await.unwrap();
    let mut status = record.status.unwrap();
    status.phase = MessagePhase::Expired;
    status.since = Utc::now() - chrono::Duration::days(2);
    messages.update_status(&name, &record.metadata.resource_version, &status).await.unwrap();
    assert_eq!(inbox.compact_audit(Utc::now()).await.unwrap(), 1);
    let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status;
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("review".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("review".into())
        .subject_revision("revision".into())
        .sender("system:turn-rules".into())
        .build();
    let replay = crew.deliver_turn(&request).await.unwrap();
    assert!(!replay.new_turn);
    assert_eq!(replay.message.name, name);
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status, before);
}

// A live capability source is queried on every orientation, and changes create
// superseding system Messages without delivering duplicate launch observations.
