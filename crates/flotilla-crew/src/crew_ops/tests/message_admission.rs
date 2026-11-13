use crate::crew_ops::MessageAttribution;
use flotilla_resources::ResumeIntent;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::ResourceRef;
use flotilla_resources::terminal_records::TerminalRecords as _;
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoyStatusPatch, CrewMessageSender, CrewWorkPhase, InputMeta, TerminalAttentionState,
    TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase,
};
use flotilla_store::{note_convoy, ResourceBackend};

use super::{fixture, StagingProbe};
use crate::ports::WorkCredentialReconciler;

#[tokio::test]
async fn turn_admission_returns_the_canonical_suppressed_message() {
    use flotilla_resources::{Message, MessageExpectation, MessageReference};

    use crate::leaf_engine::CrewTurnIntent;
    let (crew, backend, probe, held) = fixture(CrewWorkPhase::Working).await;
    probe.fail.store(false, Ordering::SeqCst);
    let reference = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", held.launch.clone()),
        revision: "revision".into(),
    };
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(held.launch.clone())
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
    let coder = flotilla_resources::crew_role_address("flotilla", &held.launch, "work", "coder");
    held.messages().deliver_turn(&coder, &first.message.name, "session").await;
    let before = held.convoy(&backend).await.status;
    let staging_calls = probe.calls.load(Ordering::SeqCst);
    let mut successor = request.clone();
    successor.source = "second".into();
    successor.brief = "duplicate turn".into();
    let suppressed = crew.deliver_turn(&successor).await.expect("suppression admission");
    assert!(first.new_turn);
    assert!(!suppressed.new_turn);
    assert_eq!(suppressed.message, first.message);
    assert_eq!(held.convoy(&backend).await.status, before, "suppression must not reopen workflow state for an absent new turn");
    assert_eq!(probe.calls.load(Ordering::SeqCst), staging_calls);
    assert_eq!(backend.using::<Message>("flotilla").list().await.expect("inbox").items.len(), 1);
}

// A queued follow-up continues workflow only after Message acceptance evidence;
// the authority watch consumes its reference without creating another input.
#[tokio::test]
async fn follow_up_reference_releases_from_message_evidence() {
    use flotilla_resources::Message;
    let (crew, backend, _probe, held) = fixture(CrewWorkPhase::Working).await;
    crew.resume("flotilla", &held.launch, "continue after this turn", Some("work"), Some("coder")).await.unwrap();
    let coder = || async { held.convoy(&backend).await.status.unwrap().crew_work["work"]["coder"].clone() };
    let pending = coder().await.pending_follow_up.unwrap();
    crew.reconcile_pending_supervisor_turns_once("flotilla").await.unwrap();
    assert_eq!(coder().await.pending_follow_up.as_ref(), Some(&pending));
    let receiver = flotilla_resources::crew_role_address("flotilla", &held.launch, "work", "coder");
    held.messages().deliver_turn(&receiver, &pending.name, "session").await;
    crew.reconcile_pending_supervisor_turns_once("flotilla").await.unwrap();
    let state = coder().await;
    assert!(state.pending_follow_up.is_none());
    assert_eq!(state.resume_brief_id.as_deref(), Some(pending.name.as_str()));
    // The crew's current brief is the Message its resume names.
    let messages = backend.using::<Message>("flotilla").list().await.unwrap().items;
    assert_eq!(messages.len(), 1);
    assert_eq!((messages[0].metadata.name.as_str(), messages[0].spec.body.as_str()), (pending.name.as_str(), "continue after this turn"));
}

// Receiver admission can suppress a turn before its Message has replicated to
// the producer. The acknowledgement must restore speculative workflow activation.
#[tokio::test]
async fn remote_suppression_restores_workflow_before_replication() {
    use flotilla_messaging::MessageInbox;
    use flotilla_resources::{
        Message, MessageExpectation, MessageReference, MessageRelation, MessageSpec, MessageStatusPatch, ResolvedMessageReceiver,
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
                flotilla_messaging::MessageAdmission::Accepted(record) => record,
                flotilla_messaging::MessageAdmission::Suppressed { predecessor } => predecessor,
            };
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, record.metadata.name))
        }
    }
    let (crew, backend, probe, held) = fixture(CrewWorkPhase::Done).await;
    probe.fail.store(false, Ordering::SeqCst);
    let remote = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let inbox = MessageInbox::new(remote.clone(), "flotilla");
    let subject = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", held.launch.clone()),
        revision: "revision".into(),
    };
    let existing = MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver(held.receiver("work", "coder"))
        .relation(MessageRelation::System)
        .body("prior open request".into())
        .references(vec![subject.clone()])
        .subject(subject.clone())
        .expectation(MessageExpectation::Reply)
        .build();
    inbox.accept(&InputMeta::builder().name("canonical".into()).build(), &existing, Utc::now()).await.unwrap();
    flotilla_store::apply_status_patch(
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
    let before = held.convoy(&backend).await.status;
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(held.launch.clone())
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
    assert_eq!(held.convoy(&backend).await.status, before);
    assert_eq!(remote.using::<Message>("flotilla").list().await.unwrap().items.len(), 1);
    assert!(backend.using::<Message>("flotilla").list().await.unwrap().items.is_empty());
}

// Failed durable admission must not reopen settled work: a receiver collision, a different intent
// already admitted under the turn's name, through the real crew service.
#[tokio::test]
async fn turn_admission_errors_restore_the_owned_activation() {
    use crate::leaf_engine::CrewTurnIntent;
    struct AdmissionCollision {
        probe: Arc<StagingProbe>,
        backend: ResourceBackend,
        receiver: String,
    }
    #[async_trait]
    impl WorkCredentialReconciler for AdmissionCollision {
        async fn reconcile(&self, namespace: &str, environment: &str) -> Result<(), String> {
            self.probe.reconcile(namespace, environment).await?;
            let receiver = self.receiver.as_str();
            let sender = "system:legacy";
            let name = flotilla_resources::message_record_name(receiver, sender, "turn-delivery:failure:failure-head");
            let spec = flotilla_resources::MessageSpec::builder()
                .sender(sender.into())
                .receiver(receiver.into())
                .relation(flotilla_resources::MessageRelation::System)
                .body("concurrent intent".into())
                .build();
            flotilla_messaging::MessageInbox::new(self.backend.clone(), namespace)
                .accept(&InputMeta::builder().name(name).build(), &spec, Utc::now())
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
    }
    let (crew, backend, probe, held) = fixture(CrewWorkPhase::Done).await;
    probe.fail.store(false, Ordering::SeqCst);
    *crew.work_credential_reconciler.write().await =
        Some(Arc::new(AdmissionCollision { probe: probe.clone(), backend: backend.clone(), receiver: held.receiver("work", "coder") }));
    let before = held.convoy(&backend).await.status;
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(held.launch.clone())
        .source("failure".into())
        .vessel("work".into())
        .role("coder".into())
        .subject_revision("failure-head".into())
        .brief("continue".into())
        .sender(flotilla_resources::legacy_message_sender(&CrewMessageSender::Unknown).0)
        .build();
    let error = crew.deliver_turn(&request).await.expect_err("durable admission fails");
    assert!(error.contains("different intent"), "{error}");
    assert!(probe.calls.load(Ordering::SeqCst) > 0, "exercise activation before admission");
    assert_eq!(held.convoy(&backend).await.status, before, "after failed admission");
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("receiver inbox").items;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].spec.body, "concurrent intent");
}

#[tokio::test]
async fn resume_and_withdraw_wait_for_follow_up_replication() {
    for withdraw in [false, true] {
        let (crew, backend, _, held) = fixture(CrewWorkPhase::Done).await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        note_convoy(
            &held.engine.path(),
            &convoys,
            &held.launch,
            &ConvoyStatusPatch::QueueMessageFollowUp {
                vessel: "work".into(),
                role: "coder".into(),
                message: Some(ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "not-replicated")),
            },
        )
        .await
        .expect("pending continuation");
        let before = held.convoy(&backend).await.status;
        let error = if withdraw {
            crew.withdraw_pending_brief("flotilla", &held.launch).await.expect_err("withdraw awaits replication")
        } else {
            crew.convoy_resume_with_sender_internal(
                "flotilla",
                &held.launch,
                "replacement",
                (None, None),
                MessageAttribution::operator(None),
                ResumeIntent::Reopen,
            )
            .await
            .expect_err("resume awaits replication")
        };
        assert!(error.contains("follow-up Message awaits replication"), "{error}");
        assert_eq!(held.convoy(&backend).await.status, before, "unchanged workflow");
        assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.is_empty());
    }
}

#[tokio::test]
async fn fresh_idle_admission_keeps_delivery_evidence_receiver_owned() {
    for unconfirmed in [false, true] {
        let (crew, backend, probe, held) = fixture(CrewWorkPhase::Working).await;
        probe.fail.store(false, Ordering::SeqCst);
        // The session's home observes it freshly idle, holding a typed turn unconfirmed or not.
        let now = Utc::now();
        held.engine.engine.terminals.terminals.observations().update(&held.session, |observation| {
            observation.attention = Some(flotilla_resources::TerminalAttention {
                state: TerminalAttentionState::Idle,
                as_of: now,
                source: flotilla_resources::TerminalAttentionSource::Screen,
            });
            observation.delivery_unconfirmed_since = unconfirmed.then_some(now);
        });
        let sessions = backend.using::<ResourceTerminalSession>("flotilla");
        let status = sessions.get("session").await.expect("fresh idle with legacy obligations").status;
        assert_eq!(status.as_ref().map(|status| status.phase), Some(ResourceTerminalSessionPhase::Running));
        crew.resume("flotilla", &held.launch, "next admitted brief", None, None).await.expect("admit at idle boundary");
        assert_eq!(sessions.get("session").await.expect("unchanged receiver evidence").status, status);
        let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
        assert_eq!(records.len(), 1);
        assert!(records[0].status.as_ref().expect("admission status").resolved_receiver.is_none());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1, "workflow resumes but does not deliver");
    }
}

// A live capability source is queried on every orientation, and changes create
// superseding system Messages without delivering duplicate launch observations.
