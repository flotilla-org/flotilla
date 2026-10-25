use flotilla_resources::ResumeIntent;
use flotilla_store_memory::InMemoryBackend;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::{CrewCommandContext, PrincipalRef, ResourceRef};
use flotilla_resources::{
    external_patches as convoy_external_patches, Convoy as ResourceConvoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewWorkPhase,
    CrewWorkState, HoldAct, InputMeta, TerminalSession as ResourceTerminalSession, TerminalSessionPhase, TerminalSessionStatus,
    CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};
use flotilla_store::{apply_status_patch as apply_resource_status_patch, ResourceBackend};
use tokio::sync::RwLock;

use super::fixture;
use crate::crew_ops::{queue_pending_crew_message, CrewService, CrewSupervisionRequest, CrewTurnDeliveryActuator, ResolvedCrewContext};
use crate::ports::WorkCredentialReconciler;
use flotilla_resources::{CrewMessageSender, Selector, TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec, Vessel};

#[hegel::test]
fn cross_vessel_handoff_publishes_typed_messages(tc: hegel::TestCase) {
    use flotilla_messaging::MessageInbox;
    use flotilla_resources::{Message, MessageReference, MessageRelation, MessageSpec, VesselSpec};
    use hegel::generators as gs;

    use crate::leaf_engine::ResourceIntentPublisher;
    let remote = tc.draw(gs::booleans());
    let publication_fails = tc.draw(gs::booleans());
    let receiver_phase = if tc.draw(gs::booleans()) { CrewWorkPhase::Done } else { CrewWorkPhase::Pending };
    let repeats = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    // Stand-in for the cross-host resource mutation endpoint; admission uses the real inbox.
    struct ReceiverHome {
        inbox: MessageInbox,
        fail: bool,
    }
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverHome {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            if self.fail {
                return Err("receiver publication unavailable".into());
            }
            assert_eq!(document["kind"], "Message");
            let spec: MessageSpec = serde_json::from_value(document["spec"].clone()).expect("Message intent");
            let admission = self
                .inbox
                .accept(
                    &InputMeta::builder().name(document["metadata"]["name"].as_str().expect("message name").into()).build(),
                    &spec,
                    Utc::now(),
                )
                .await
                .map_err(|error| error.to_string())?;
            let record = match admission {
                flotilla_messaging::MessageAdmission::Accepted(record) => record,
                flotilla_messaging::MessageAdmission::Suppressed { predecessor } => predecessor,
            };
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, record.metadata.name))
        }
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
        probe.fail.store(false, Ordering::SeqCst);
        backend
            .using::<Vessel>("flotilla")
            .create(
                &InputMeta::builder().name("vessel".into()).build(),
                &VesselSpec {
                    convoy_ref: "crew".into(),
                    vessel_name: "work".into(),
                    placement_policy_ref: "test".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("caller vessel");
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("crew").await.expect("convoy");
        let mut status = convoy.status.clone().expect("status");
        let mut receiver_vessel = status.workflow_snapshot.as_ref().expect("snapshot").vessels[0].clone();
        receiver_vessel.name = "review".into();
        receiver_vessel.crew[0].role = "reviewer".into();
        status.workflow_snapshot.as_mut().expect("snapshot").vessels.push(receiver_vessel);
        status
            .crew_work
            .insert("review".into(), BTreeMap::from([("reviewer".into(), CrewWorkState::builder().phase(receiver_phase).build())]));
        convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("second vessel");
        let home = if remote { ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()) } else { backend.clone() };
        let publisher: Arc<dyn ResourceIntentPublisher> =
            Arc::new(ReceiverHome { inbox: MessageInbox::new(home.clone(), "flotilla"), fail: publication_fails });
        if remote || publication_fails {
            crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
        }
        let context = CrewCommandContext::builder().convoy("crew".into()).vessel_ref("vessel".into()).role("coder".into()).build();
        let carry = MessageReference::ControlRecord {
            resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "crew"),
            revision: convoy.metadata.resource_version.clone(),
        };
        if publication_fails {
            let before = convoys.get("crew").await.expect("before handoff").status;
            for _ in 0..repeats {
                crew.handoff_with_carries(&context, "crew/review/reviewer", "review this", vec![carry.clone()])
                    .await
                    .expect_err("receiver publication fails");
                assert_eq!(convoys.get("crew").await.expect("restored authority").status, before);
                assert!(home.using::<Message>("flotilla").list().await.expect("inbox").items.is_empty());
            }
            return;
        }
        for index in 0..repeats {
            let target = if index % 2 == 0 { "crew/review/reviewer" } else { "flotilla/crew/review/reviewer" };
            crew.handoff_with_carries(&context, target, "review this", vec![carry.clone()]).await.expect("handoff");
            assert_eq!(
                convoys.get("crew").await.expect("activated convoy").status.expect("status").crew_work["review"]["reviewer"].phase,
                CrewWorkPhase::Working
            );
            let records = home.using::<Message>("flotilla").list().await.expect("receiver inbox").items;
            assert_eq!(records.len(), index + 1, "each handoff is distinct even when its body repeats");
            for record in records {
                assert_eq!(record.spec.sender, "flotilla/crew/work/coder");
                assert_eq!(record.spec.receiver, "flotilla/crew/review/reviewer");
                assert_eq!(record.spec.relation, MessageRelation::Peer);
                assert_eq!(record.spec.references, vec![carry.clone()]);
                assert_eq!(record.spec.body, "review this");
            }
        }
        for invalid in ["crew/missing/reviewer", "other/review/reviewer", "principal:operator", "coder"] {
            crew.handoff(&context, invalid, "review").await.expect_err("invalid crew target");
        }
        crew.handoff(&context, "crew/review/reviewer", "").await.expect_err("empty handoff");
        assert_eq!(home.using::<Message>("flotilla").list().await.expect("inbox").items.len(), repeats);
    });
}

// An authenticated operator can send supervisor guidance to crew that has no stall.
#[tokio::test]
async fn operator_supervision_messages_working_crew() {
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    let principal = PrincipalRef { namespace: "flotilla".into(), name: "operator-alice".into() };
    crew.supervise(CrewSupervisionRequest {
        namespace: "flotilla",
        convoy_name: "crew",
        vessel: "work",
        role: "coder",
        operation: flotilla_protocol::CrewSupervisionAction::Resume,
        message: "please follow up",
        actor_crew_id: None,
        principal: Some(&principal),
    })
    .await
    .expect("operator guidance");
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.sender, "principal:operator-alice");
    assert_eq!(messages[0].spec.receiver, "flotilla/crew/work/coder");
    assert_eq!(messages[0].spec.relation, flotilla_resources::MessageRelation::Supervisor);
    assert_eq!(messages[0].spec.body, "please follow up");
    assert!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy").status.expect("status").stalled.is_none());
}

// Legacy PendingBrief adoption is replay-safe and retains both attribution and continuation intent.
#[tokio::test]
async fn legacy_pending_brief_is_adopted_once_before_withdrawal() {
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get("crew").await.expect("convoy");
    let mut status = convoy.status.expect("status");
    // Decode the exact previous-generation record rather than invoking a retired producer.
    status.turn_deliveries.entry("operator".into()).or_default().pending_brief = Some(
        serde_json::from_value(serde_json::json!({
            "vessel": "work", "role": "coder", "content": "legacy follow-up", "queued_at": "2026-10-01T00:00:00Z",
            "sender": {"kind": "operator-resume", "principal": {"namespace": "flotilla", "name": "alice"}}
        }))
        .expect("legacy PendingBrief"),
    );
    convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("stored legacy queue");
    for _ in 0..2 {
        crew.adopt_legacy_convoy_queues_once("flotilla", None).await.expect("adoption");
        crew.reconcile_pending_supervisor_turns_once("flotilla").await.expect("follow-up");
    }
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.sender, "principal:alice");
    assert_eq!(messages[0].spec.body, "legacy follow-up");
    let status = convoys.get("crew").await.expect("convoy").status.expect("status");
    assert!(status.pending_brief().is_none());
    assert_eq!(status.crew_work["work"]["coder"].pending_follow_up.as_ref().expect("continuation").name, messages[0].metadata.name);
    assert_eq!(crew.withdraw_pending_brief("flotilla", "crew").await.expect("withdraw adopted Message"), Some("legacy follow-up".into()));
}

// Every governor ruling is a Message reply to the exact crew's open escalation.
#[hegel::test]
fn governor_rulings_reply_to_the_source_escalation(tc: hegel::TestCase) {
    use flotilla_messaging::MessageInbox;
    use flotilla_protocol::CrewSupervisionAction;
    use flotilla_resources::{Message, MessageExpectation, MessageReference, MessageRelation, MessageSpec, StallRung, StallSupervisor};
    use hegel::generators as gs;
    let escalation_visible = tc.draw(gs::booleans());
    // All three rulings reply to the source escalation. Escalate additionally
    // admits the next recipient using the real turn-delivery actuator and inbox.
    let action = match tc.draw(gs::integers::<usize>().min_value(0).max_value(2)) {
        0 => CrewSupervisionAction::Resume,
        1 => CrewSupervisionAction::Fail,
        _ => CrewSupervisionAction::Escalate,
    };
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
        probe.fail.store(false, Ordering::SeqCst);
        crew.set_turn_delivery_actuator(Arc::new(CrewTurnDeliveryActuator { crew: Arc::downgrade(&crew) })).await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        apply_resource_status_patch(
            &convoys,
            "crew",
            &convoy_external_patches::mark_crew_stalled(
                "crew".into(),
                "work".into(),
                "coder".into(),
                Utc::now(),
                flotilla_protocol::StallReason::Scope,
                None,
                "blocked".into(),
            ),
        )
        .await
        .expect("declare stall");
        let convoy = convoys.get("crew").await.expect("source convoy");
        let mut status = convoy.status.expect("source status");
        status.workflow_snapshot.as_mut().expect("workflow").supervision =
            Some(vec![flotilla_resources::SupervisionTarget::Address { address: "principal:operator".into() }]);
        let stalled = status.stalled.as_mut().expect("stall");
        stalled.rung = StallRung::Governor;
        stalled.supervisor = Some(StallSupervisor { convoy: "governor".into(), vessel: "watch".into(), role: "governor".into() });
        convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("supervisor ownership");
        convoys
            .create(
                &InputMeta::builder().name("governor".into()).build(),
                &ConvoySpec::builder().workflow_ref("workflow".into()).role("governor".into()).build(),
            )
            .await
            .expect("governor convoy");
        let sessions = backend.using::<ResourceTerminalSession>("flotilla");
        let source_session = sessions.get("session").await.expect("source session");
        let mut spec = source_session.spec;
        spec.role = "governor".into();
        let governor_session = sessions
            .create(
                &InputMeta::builder()
                    .name("governor-session".into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "governor".into()),
                        (VESSEL_LABEL.into(), "watch".into()),
                        (ROLE_LABEL.into(), "governor".into()),
                    ]))
                    .build(),
                &spec,
            )
            .await
            .expect("governor session");
        sessions
            .update_status(
                "governor-session",
                &governor_session.metadata.resource_version,
                &flotilla_resources::TerminalSessionStatus {
                    crew: Some(flotilla_resources::CrewSessionStatus {
                        id: "governor-id".into(),
                        adapter: "codex".into(),
                        model: None,
                        stance: "trusted-implicit".into(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("supervisor identity");
        let escalation = MessageSpec::builder()
            .sender("flotilla/crew/work/coder".into())
            .receiver("flotilla/governor/watch/governor".into())
            .relation(MessageRelation::Supervisor)
            .body("blocked".into())
            .expectation(MessageExpectation::Reply)
            .references(vec![MessageReference::ControlRecord {
                resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "crew"),
                revision: convoy.metadata.resource_version,
            }])
            .build();
        if escalation_visible {
            MessageInbox::new(backend.clone(), "flotilla")
                .accept(&InputMeta::builder().name("source-escalation".into()).build(), &escalation, Utc::now())
                .await
                .expect("escalation");
        }
        // No visible escalation and no supervision index must produce an unlinked ruling,
        // rather than inventing a record ID.
        crew.supervise(CrewSupervisionRequest {
            namespace: "flotilla",
            convoy_name: "crew",
            vessel: "work",
            role: "coder",
            operation: action,
            message: "ruling",
            actor_crew_id: Some("governor-id"),
            principal: None,
        })
        .await
        .expect("governor ruling");
        let records = backend.using::<Message>("flotilla").list().await.expect("messages").items;
        assert_eq!(records.len(), 1 + usize::from(escalation_visible) + usize::from(action == CrewSupervisionAction::Escalate));
        let reply = records
            .iter()
            .find(|record| record.metadata.name != "source-escalation" && record.spec.receiver == "flotilla/crew/work/coder")
            .expect("ruling Message");
        assert_eq!(reply.spec.sender, "flotilla/governor/watch/governor");
        assert_eq!(reply.spec.receiver, "flotilla/crew/work/coder");
        assert_eq!(reply.spec.in_reply_to.as_deref(), escalation_visible.then_some("source-escalation"));
        assert_eq!(reply.spec.relation, MessageRelation::Supervisor);
        let status = convoys.get("crew").await.expect("convoy").status.expect("status");
        match action {
            CrewSupervisionAction::Resume => assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working),
            CrewSupervisionAction::Fail => assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Failed),
            CrewSupervisionAction::Escalate => assert_eq!(status.stalled.expect("escalated stall").rung, StallRung::Operator),
        }
    });
}

// A durable ruling survives an authority mutation failure as a notification;
// Message delivery never applies Fail/Escalate workflow transitions itself.
#[tokio::test]
async fn supervisor_decision_survives_authority_disappearing_after_publication() {
    use flotilla_messaging::MessageInbox;
    use flotilla_resources::{Message, MessageSpec};

    use crate::leaf_engine::ResourceIntentPublisher;
    // Boundary stand-in: the receiver admits intent, then a concurrent authority
    // teardown removes the convoy before the supervisor's state patch can read it.
    struct ReceiverAndTeardown(ResourceBackend);
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverAndTeardown {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            let name = document["metadata"]["name"].as_str().expect("name").to_string();
            let spec: MessageSpec = serde_json::from_value(document["spec"].clone()).expect("intent");
            MessageInbox::new(self.0.clone(), namespace)
                .accept(&InputMeta::builder().name(name.clone()).build(), &spec, Utc::now())
                .await
                .map_err(|error| error.to_string())?;
            self.0.using::<ResourceConvoy>(namespace).delete("crew").await.map_err(|error| error.to_string())?;
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, name))
        }
    }
    for action in [flotilla_protocol::CrewSupervisionAction::Fail, flotilla_protocol::CrewSupervisionAction::Escalate] {
        let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
        apply_resource_status_patch(
            &backend.using::<ResourceConvoy>("flotilla"),
            "crew",
            &convoy_external_patches::mark_crew_stalled(
                "crew".into(),
                "work".into(),
                "coder".into(),
                Utc::now(),
                flotilla_protocol::StallReason::Scope,
                None,
                "blocked".into(),
            ),
        )
        .await
        .expect("stall");
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("crew").await.expect("source");
        let mut status = convoy.status.expect("status");
        status.stalled.as_mut().expect("stall").rung = flotilla_resources::StallRung::Governor;
        convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("governor rung");
        let convoy = convoys.get("crew").await.expect("source");
        let mut status = convoy.status.expect("status");
        status.workflow_snapshot.as_mut().expect("workflow").supervision =
            Some(vec![flotilla_resources::SupervisionTarget::Address { address: "principal:operator".into() }]);
        convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("named operator");
        _probe.fail.store(false, Ordering::SeqCst);
        crew.set_turn_delivery_actuator(Arc::new(super::super::CrewTurnDeliveryActuator { crew: Arc::downgrade(&crew) })).await;
        let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(ReceiverAndTeardown(backend.clone()));
        crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
        let principal = PrincipalRef::implicit_for_namespace("flotilla");
        crew.supervise(CrewSupervisionRequest {
            namespace: "flotilla",
            convoy_name: "crew",
            vessel: "work",
            role: "coder",
            operation: action,
            message: "durable ruling",
            actor_crew_id: None,
            principal: Some(&principal),
        })
        .await
        .expect_err("authority patch cannot read removed convoy");
        let records = backend.using::<Message>("flotilla").list().await.expect("decisions").items;
        assert_eq!(records.len(), if action == flotilla_protocol::CrewSupervisionAction::Escalate { 2 } else { 1 });
        let ruling = records.iter().find(|record| record.spec.body == "durable ruling").expect("durable ruling");
        assert_eq!(ruling.spec.receiver, "flotilla/crew/work/coder");
        assert_eq!(ruling.spec.expectation, flotilla_resources::MessageExpectation::None);
    }
}

// #2758: holds persist convoy/session facts and admit exactly one plain supervisor
// Message, plus one notice to the held crew (#3565), across retries, with no forge
// collaborator available. Cover local and project supervisors, no PR binding, and
// retries carrying newer head identities.
#[hegel::test]
fn hold_state_and_single_supervisor_message(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let local_supervisor = tc.draw(gs::booleans());
    let address_supervisor = tc.draw(gs::booleans());
    let retries = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (crew, backend, _, _dir) = fixture(CrewWorkPhase::Done).await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        if local_supervisor {
            let current = convoys.get("crew").await.expect("convoy");
            let mut status = current.status.expect("status");
            status.crew_work.get_mut("work").expect("work").insert("bosun".into(),
                CrewWorkState::builder().phase(CrewWorkPhase::Working).build());
            convoys.update_status("crew", &current.metadata.resource_version, &status).await.expect("supervisor");
        }
        if address_supervisor {
            let current = convoys.get("crew").await.expect("convoy");
            let mut status = current.status.expect("status");
            status.workflow_snapshot.as_mut().expect("workflow").supervision = Some(vec![
                flotilla_resources::SupervisionTarget::Address { address: "flotilla/attended".into() },
            ]);
            convoys.update_status("crew", &current.metadata.resource_version, &status).await.expect("address policy");
        }
        let mut request = crate::leaf_engine::CrewTurnIntent::builder()
            .namespace("flotilla".into()).convoy("crew".into()).source("checks-settled".into())
            .vessel("work".into()).role("coder".into()).brief("continue".into())
            .subject_revision("head-one".into()).sender("system:turn-rules".into()).build();
        for retry in 0..retries {
            request.subject_revision = format!("head-{retry}");
            crew.execute_turn_delivery_hold(&request, &HoldAct::State, "episode limit").await.expect("hold");
        }
        let convoy = convoys.get("crew").await.expect("convoy");
        assert_eq!(convoy.status.expect("status").attention.expect("attention").reason, "episode limit");
        let session = backend.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
        assert_eq!(session.status.expect("status").turn_delivery_hold.expect("hold").reason, "episode limit");
        let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("messages");
        assert_eq!(messages.items.len(), 2, "retries cannot duplicate the supervisor signal or the crew notice");
        // #3565: the held crew is told why its wakes stopped.
        let notice = messages.items.iter().find(|message| message.spec.receiver == "flotilla/crew/work/coder").expect("crew notice");
        assert_eq!(notice.spec.relation, flotilla_resources::MessageRelation::System);
        assert!(notice.spec.body.contains("`checks-settled` turns for you are held") && notice.spec.body.contains("episode limit"));
        let message = messages.items.iter().find(|message| message.spec.receiver != "flotilla/crew/work/coder").expect("supervisor signal");
        assert_eq!(message.spec.receiver, if address_supervisor { "flotilla/attended" } else if local_supervisor { "flotilla/crew/work/bosun" } else { "flotilla/governor" });
        assert_eq!(message.spec.relation, flotilla_resources::MessageRelation::System);
        assert_eq!(message.spec.expectation, flotilla_resources::MessageExpectation::None);
        assert!(message.spec.body.contains("episode limit"));
        assert!(matches!(message.spec.references.last(), Some(flotilla_resources::MessageReference::ControlRecord { resource, .. }) if resource.name == "crew"));
    });
}

// #2758: the convoy keeps its hold through a receiver-home outage; session state
// is patched at its authority before the single supervisor signal is published.
#[tokio::test]
async fn remote_session_hold_retries_through_existing_mutation_router() {
    use crate::leaf_engine::{CrewTurnIntent, ResourceIntentPublisher};
    // Stand-in for the inter-host command channel; both endpoint stores and inbox
    // admission are real in-memory collaborators, including version validation.
    struct Router {
        source: ResourceBackend,
        receiver: ResourceBackend,
        fail: AtomicBool,
    }
    #[async_trait]
    impl ResourceIntentPublisher for Router {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            let spec: flotilla_resources::MessageSpec = serde_json::from_value(document["spec"].clone()).expect("Message intent");
            let name = document["metadata"]["name"].as_str().expect("Message name");
            flotilla_messaging::MessageInbox::new(self.source.clone(), namespace)
                .accept(&InputMeta::builder().name(name.into()).build(), &spec, Utc::now())
                .await
                .map_err(|error| error.to_string())?;
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, name))
        }
        async fn patch_status(
            self: Arc<Self>,
            namespace: &str,
            kind: &str,
            name: &str,
            status: serde_json::Value,
            expected: &str,
        ) -> Result<(), String> {
            assert_eq!(kind, "TerminalSession");
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err("receiver unavailable".into());
            }
            flotilla_store::patch_resource_status_if_version(&self.receiver, namespace, kind, name, status, expected)
                .await
                .map_err(|error| error.to_string())?;
            self.source
                .replica_writer::<ResourceTerminalSession>(flotilla_protocol::NodeId::new("receiver"), namespace)
                .replace(
                    &self.receiver.using::<ResourceTerminalSession>(namespace).list().await.map_err(|error| error.to_string())?,
                    Utc::now(),
                )
                .await
                .map_err(|error| error.to_string())
        }
    }
    let (crew, backend, _, _dir) = fixture(CrewWorkPhase::Done).await;
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let source = sessions.get("session").await.expect("source session");
    let receiver = ResourceBackend::new(InMemoryBackend::default());
    let remote = receiver
        .using::<ResourceTerminalSession>("flotilla")
        .create(&InputMeta::from(&source.metadata), &source.spec)
        .await
        .expect("remote session");
    receiver
        .using::<ResourceTerminalSession>("flotilla")
        .update_status("session", &remote.metadata.resource_version, &source.status.clone().unwrap_or_default())
        .await
        .expect("remote status");
    sessions.delete("session").await.expect("retire local record");
    backend
        .replica_writer::<ResourceTerminalSession>(flotilla_protocol::NodeId::new("receiver"), "flotilla")
        .replace(&receiver.using::<ResourceTerminalSession>("flotilla").list().await.expect("remote snapshot"), Utc::now())
        .await
        .expect("replica");
    let router: Arc<dyn ResourceIntentPublisher> =
        Arc::new(Router { source: backend.clone(), receiver: receiver.clone(), fail: AtomicBool::new(true) });
    crew.set_resource_intent_publisher(Arc::downgrade(&router));
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("checks-settled".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("continue".into())
        .subject_revision("head".into())
        .sender("system:turn-rules".into())
        .build();
    assert_eq!(crew.execute_turn_delivery_hold(&request, &HoldAct::State, "limit").await.expect_err("outage"), "receiver unavailable");
    let held = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy");
    let raised_at = held.status.expect("status").turn_delivery_holds()[0].raised_at;
    assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.is_empty());
    for _ in 0..2 {
        crew.execute_turn_delivery_hold(&request, &HoldAct::State, "limit").await.expect("retry hold");
    }
    let session = receiver.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
    assert_eq!(session.status.expect("status").turn_delivery_hold.expect("hold").raised_at, raised_at);
    // One supervisor signal and one crew notice, however many retries.
    assert_eq!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.len(), 2);
}

// All terminal convoy outcomes refuse resume and turn admission with an explicit
// continuation path. Refusal leaves the convoy and Message inbox unchanged.
#[tokio::test]
async fn terminal_convoys_refuse_resume_and_turn_delivery() {
    for phase in [ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Abandoned] {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("crew").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = phase;
        status.finished_at = Some(Utc::now());
        let settled = convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("settle");
        let error = crew.resume("flotilla", "crew", "resume", Some("work"), Some("coder")).await.expect_err("terminal resume refuses");
        assert!(error.contains("--continue-pr") && error.contains("new generation"), "{error}");
        let request = crate::leaf_engine::CrewTurnIntent::builder()
            .namespace("flotilla".into())
            .convoy("crew".into())
            .source("review".into())
            .vessel("work".into())
            .role("coder".into())
            .brief("address review".into())
            .subject_revision("head".into())
            .sender("system:turn-rules".into())
            .build();
        let error = crew.deliver_turn(&request).await.expect_err("terminal delivery refuses");
        assert!(error.contains("--continue-pr") && error.contains("new generation"), "{error}");
        let after = convoys.get("crew").await.expect("convoy");
        assert_eq!(after.status, settled.status);
        assert_eq!(after.metadata.resource_version, settled.metadata.resource_version);
        assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.is_empty());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    }
}

// Lost backing is recoverable work, not a provisioning failure. Resume explains
// the unavailable rehydration path and never queues input or stages credentials.
#[tokio::test]
async fn lost_vessel_resume_explains_rehydration() {
    let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
    let vessels = backend.using::<flotilla_resources::Vessel>("flotilla");
    let vessel = vessels
        .create(
            &InputMeta::builder().name("crew-work".into()).build(),
            &flotilla_resources::VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    vessels
        .update_status(
            "crew-work",
            &vessel.metadata.resource_version,
            &flotilla_resources::VesselStatus {
                phase: flotilla_resources::VesselPhase::Lost,
                message: Some("host reboot".into()),
                ..Default::default()
            },
        )
        .await
        .expect("loss");
    let convoy = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy");
    let view = crew
        .crew_state(
            &ResolvedCrewContext {
                namespace: "flotilla".into(),
                convoy: "crew".into(),
                vessel_ref: "crew-work".into(),
                vessel: "work".into(),
                caller_role: "coder".into(),
                caller_session: None,
            },
            &convoy,
        )
        .await
        .expect("crew state");
    assert_eq!(view.members[0].state, "lost");
    assert_eq!(view.members[0].reason.as_deref(), Some("lost, recoverable: host reboot"));
    // A new daemon service has no remembered interruption. It must refuse
    // resume from the persisted Lost record after Work has already rolled up.
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let mut status = convoy.status.expect("status");
    status.phase = ConvoyPhase::Interrupted;
    status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Interrupted;
    status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Interrupted;
    let status: ConvoyStatus = serde_json::from_value(serde_json::to_value(status).expect("persist")).expect("restart decode");
    convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("interruption roll-up");
    let restarted = CrewService::builder()
        .resource_backend(backend.clone())
        .leaf_subscriptions(crew.leaf_subscriptions.clone())
        .work_credential_reconciler(RwLock::new(Some(probe.clone() as Arc<dyn WorkCredentialReconciler>)))
        .clock(crew.clock.clone())
        .provisioning_namespace(crew.provisioning_namespace.clone())
        .config(crew.config.clone())
        .host_name(crew.host_name.clone())
        .brief_artifact_writer(crew.brief_artifact_writer.clone())
        .checkout_vcs(crew.checkout_vcs.clone())
        .local_environment_id(crew.local_environment_id.clone())
        .build();
    drop(crew);
    let before = convoys.get("crew").await.expect("convoy");
    let error = restarted.resume("flotilla", "crew", "resume", Some("work"), Some("coder")).await.expect_err("no rehydration");
    assert!(error.contains("lost, recoverable") && error.contains("host reboot") && error.contains("#2872"), "{error}");
    assert!(!error.contains("failed provisioning"), "{error}");
    let after = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy");
    assert_eq!(after.status, before.status);
    assert_eq!(after.metadata.resource_version, before.metadata.resource_version);
    assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.is_empty());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);

    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let original = sessions.get("session").await.expect("session");
    let mut session = sessions
        .create(
            &InputMeta::builder()
                .name("mapped-session".into())
                .labels(BTreeMap::from([(VESSEL_REF_LABEL.into(), "crew-work".into())]))
                .build(),
            &original.spec,
        )
        .await
        .expect("mapped session");
    let context = ResolvedCrewContext {
        namespace: "flotilla".into(),
        convoy: "crew".into(),
        vessel_ref: "crew-work".into(),
        vessel: "work".into(),
        caller_role: "coder".into(),
        caller_session: None,
    };
    // Settled sessions retain their outcome, and completed crew work does not
    // become lost merely because its vessel later loses backing.
    for (session_phase, crew_phase, expected) in [
        (TerminalSessionPhase::Stopped, CrewWorkPhase::Interrupted, "stopped"),
        (TerminalSessionPhase::Failed, CrewWorkPhase::Interrupted, "failed"),
        (TerminalSessionPhase::Running, CrewWorkPhase::Done, "active"),
        (TerminalSessionPhase::Running, CrewWorkPhase::Failed, "active"),
        (TerminalSessionPhase::Running, CrewWorkPhase::HandedBack, "active"),
        (TerminalSessionPhase::Running, CrewWorkPhase::Interrupted, "lost"),
    ] {
        session = sessions
            .update_status(
                "mapped-session",
                &session.metadata.resource_version,
                &TerminalSessionStatus { phase: session_phase, ..Default::default() },
            )
            .await
            .expect("session outcome");
        let convoy = convoys.get("crew").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = ConvoyPhase::Active;
        status.crew_work.get_mut("work").unwrap().get_mut("coder").unwrap().phase = crew_phase;
        let convoy = convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("crew outcome");
        let view = restarted.crew_state(&context, &convoy).await.expect("view");
        assert_eq!(view.members[0].state, expected, "{session_phase:?}, {crew_phase:?}");
        assert_eq!(view.members[0].reason.is_some(), expected == "lost");
    }
}

// Escalating without a receiver must refuse and preserve the current owner's stall.
#[tokio::test]
async fn escalation_without_receiver_preserves_owner() {
    let (crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    apply_resource_status_patch(
        &convoys,
        "crew",
        &convoy_external_patches::mark_crew_stalled(
            "crew".into(),
            "work".into(),
            "coder".into(),
            Utc::now(),
            flotilla_protocol::StallReason::Scope,
            None,
            "blocked".into(),
        ),
    )
    .await
    .expect("stall");
    let convoy = convoys.get("crew").await.expect("convoy");
    let mut status = convoy.status.expect("status");
    let stalled = status.stalled.as_mut().expect("stall");
    stalled.rung = flotilla_resources::StallRung::Governor;
    stalled.supervision_index = Some(1);
    stalled.supervisor =
        Some(flotilla_resources::StallSupervisor { convoy: "governor".into(), vessel: "watch".into(), role: "governor".into() });
    convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("owner");
    let before = convoys.get("crew").await.expect("convoy").status;
    let result = crew
        .supervise(CrewSupervisionRequest {
            namespace: "flotilla",
            convoy_name: "crew",
            vessel: "work",
            role: "coder",
            operation: flotilla_protocol::CrewSupervisionAction::Escalate,
            message: "raise this",
            actor_crew_id: None,
            principal: Some(&PrincipalRef { namespace: "flotilla".into(), name: "alice".into() }),
        })
        .await;
    assert!(
        matches!(result, Ok(flotilla_protocol::CommandValue::CrewEscalationRefused { rung: flotilla_resources::StallRung::Operator, .. })),
        "missing receiver must refuse: {result:?}"
    );
    assert_eq!(convoys.get("crew").await.expect("convoy").status, before);
}

// Escalation either transfers the obligation to a bound receiver, or refuses
// without changing ownership. Repeated judgments must not return it to the caller.
#[hegel::test]
fn escalation_admission_and_ownership(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Span absent/present bindings, delivery failure/success, legacy cursors,
    // and duplicate escalation plus subsequent judge passes.
    let bound = tc.draw(gs::booleans());
    let reachable = tc.draw(gs::booleans());
    let cursor = tc.draw(gs::booleans());
    let operator = tc.draw(gs::booleans());
    let conflict = tc.draw(gs::booleans());
    let collocated = tc.draw(gs::booleans());
    let missing_terminal = tc.draw(gs::booleans());
    let repeats = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(run_escalation_scenarios(
        bound,
        reachable,
        cursor,
        operator,
        conflict,
        collocated,
        missing_terminal,
        repeats,
    ));
}

use crate::leaf_engine::{CrewTurnAdmission, CrewTurnIntent, TurnDeliveryActuator};
use flotilla_resources::{Message, MessageExpectation, MessageRelation, MessageSpec, StallRung, StallSupervisor, SupervisionTarget};
// Boundary fake: receiver-home publication may be unreachable. Successful
// admissions go through the real durable inbox, not an accepting mock.
struct Delivery {
    backend: ResourceBackend,
    reachable: bool,
    conflict: bool,
}
#[async_trait]
impl TurnDeliveryActuator for Delivery {
    async fn hold(&self, _: &CrewTurnIntent, _: &HoldAct, _: &str) -> Result<(), String> {
        panic!("escalation must not hold a receiver")
    }
    async fn deliver(&self, request: &CrewTurnIntent) -> Result<CrewTurnAdmission, String> {
        if !self.reachable {
            return Err("receiver home offline".into());
        }
        let receiver = request.receiver.as_ref().expect("resolved address");
        let intent = MessageSpec::builder()
            .sender(request.sender.clone())
            .receiver(receiver.clone())
            .relation(request.relation)
            .expectation(request.expectation.clone())
            .references(request.references.clone())
            .body(request.brief.clone())
            .build();
        let name = flotilla_resources::message_record_name(receiver, &request.sender, &request.source);
        let admission = flotilla_messaging::MessageInbox::new(self.backend.clone(), &request.namespace)
            .accept(&InputMeta::builder().name(name).build(), &intent, Utc::now())
            .await
            .map_err(|error| error.to_string())?;
        let record = match admission {
            flotilla_messaging::MessageAdmission::Accepted(record) => record,
            flotilla_messaging::MessageAdmission::Suppressed { predecessor } => predecessor,
        };
        if request.convoy == "crew" && request.vessel == "work" && request.role == "deputy" {
            apply_resource_status_patch(
                &self.backend.using::<ResourceConvoy>(&request.namespace),
                "crew",
                &convoy_external_patches::resume_crew_work(
                    request.vessel.clone(),
                    request.role.clone(),
                    Utc::now(),
                    request.brief.clone(),
                    Some(record.metadata.name.clone()),
                    ResumeIntent::Reopen,
                ),
            )
            .await
            .expect("local receiver activation");
        }
        // Inject a concurrent source-authority ruling at the publication
        // boundary; escalation must not overwrite it when committing ownership.
        if self.conflict {
            let convoys = self.backend.using::<ResourceConvoy>(&request.namespace);
            let source = convoys.get("crew").await.expect("source authority");
            let mut status = source.status.expect("status");
            status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").message = Some("concurrent ruling".into());
            convoys.update_status("crew", &source.metadata.resource_version, &status).await.expect("concurrent ruling");
        }
        Ok(CrewTurnAdmission {
            new_turn: true,
            rung: flotilla_resources::TurnDeliveryRung::WarmSession,
            message: ResourceRef::new("flotilla.work/v1", "Message", &request.namespace, record.metadata.name),
        })
    }
}
#[allow(clippy::too_many_arguments)]
async fn run_escalation_scenarios(
    bound: bool,
    reachable: bool,
    cursor: bool,
    operator: bool,
    conflict: bool,
    collocated: bool,
    missing_terminal: bool,
    repeats: usize,
) {
    // Pin every admission outcome, in addition to generated legacy cursors
    // and repeat counts, so the small development profile cannot miss one.
    for (bound, reachable, cursor, operator, conflict, collocated, missing_terminal) in [
        (false, true, false, false, false, false, false),
        (true, false, true, false, false, false, false),
        (true, true, false, false, false, false, false),
        (true, true, true, true, false, false, false),
        (false, true, true, true, false, false, false),
        (true, false, false, true, false, false, false),
        (true, true, true, false, true, false, false),
        (true, true, true, false, false, true, false),
        (true, true, true, false, false, true, true),
        (bound, reachable, cursor, operator, conflict, collocated && !operator && !conflict, missing_terminal),
    ] {
        let receiver = if operator {
            "principal:operator"
        } else if collocated {
            "flotilla/crew/work/deputy"
        } else {
            "flotilla/next/watch/governor"
        };
        let (crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
        crew.set_turn_delivery_actuator(Arc::new(Delivery { backend: backend.clone(), reachable, conflict })).await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        seed_escalation_stall(&backend, cursor, operator, collocated, bound, receiver).await;
        if bound && !operator && !collocated {
            seed_next_receiver(&backend).await;
        }
        if collocated && bound && !missing_terminal {
            seed_collocated_receiver(&backend).await;
        }
        let sessions = backend.using::<ResourceTerminalSession>("flotilla");
        let mut spec = sessions.get("session").await.expect("source session").spec;
        spec.role = "bosun".into();
        let terminal = sessions
            .create(
                &InputMeta::builder()
                    .name("bosun-session".into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "crew".into()),
                        (VESSEL_LABEL.into(), "work".into()),
                        (ROLE_LABEL.into(), "bosun".into()),
                    ]))
                    .build(),
                &spec,
            )
            .await
            .expect("supervisor terminal");
        sessions
            .update_status(
                "bosun-session",
                &terminal.metadata.resource_version,
                &TerminalSessionStatus {
                    crew: Some(
                        flotilla_resources::CrewSessionStatus::builder()
                            .id("bosun-id".into())
                            .adapter("codex".into())
                            .stance("trusted-implicit".into())
                            .build(),
                    ),
                    ..Default::default()
                },
            )
            .await
            .expect("supervisor identity");
        let before = convoys.get("crew").await.expect("source").status.expect("status").stalled;
        let result = crew
            .supervise(CrewSupervisionRequest {
                namespace: "flotilla",
                convoy_name: "crew",
                vessel: "work",
                role: "coder",
                operation: flotilla_protocol::CrewSupervisionAction::Escalate,
                message: "please handle this",
                actor_crew_id: Some("bosun-id"),
                principal: None,
            })
            .await;
        let after = convoys.get("crew").await.expect("source").status.expect("status").stalled;
        let records = backend.using::<Message>("flotilla").list().await.expect("messages").items;
        if bound && reachable && conflict {
            assert!(result.is_err(), "a concurrent ruling must refuse the ownership transfer: {result:?}");
            assert_eq!(after, before);
            assert_eq!(
                convoys.get("crew").await.expect("source").status.expect("status").crew_work["work"]["coder"].message.as_deref(),
                Some("concurrent ruling")
            );
            assert_eq!(records.len(), 2, "both admitted escalation and durable ruling survive the failed transfer");
            continue;
        }
        let result = result.expect("typed outcome");
        if bound && reachable && (!collocated || !missing_terminal) {
            assert_escalation_transferred(&result, &after, &records, receiver, operator, collocated);
            if collocated {
                assert_eq!(
                    convoys.get("crew").await.expect("source").status.expect("status").crew_work["work"]["deputy"].phase,
                    CrewWorkPhase::Working
                );
                continue;
            }
            for _ in 0..repeats {
                let duplicate = crew
                    .supervise(CrewSupervisionRequest {
                        namespace: "flotilla",
                        convoy_name: "crew",
                        vessel: "work",
                        role: "coder",
                        operation: flotilla_protocol::CrewSupervisionAction::Escalate,
                        message: "repeat",
                        actor_crew_id: Some("bosun-id"),
                        principal: None,
                    })
                    .await;
                assert!(duplicate.is_err(), "the old supervisor no longer owns the stall");
                crew.reconcile_crew_stalls_once("flotilla").await.expect("judge");
                let judged = convoys.get("crew").await.expect("source").status.expect("status").stalled.expect("still stalled");
                if operator {
                    assert!(judged.supervisor.is_none());
                    assert_eq!(judged.rung, StallRung::Operator);
                } else {
                    assert_eq!(judged.supervisor.as_ref().expect("owner").convoy, if collocated { "crew" } else { "next" });
                }
                assert_eq!(backend.using::<Message>("flotilla").list().await.expect("messages").items.len(), 2);
            }
        } else {
            assert_escalation_refused(&result, &after, &before, &records, operator, collocated);
        }
    }
}

async fn seed_escalation_stall(backend: &ResourceBackend, cursor: bool, operator: bool, collocated: bool, bound: bool, receiver: &str) {
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    apply_resource_status_patch(
        &convoys,
        "crew",
        &convoy_external_patches::mark_crew_stalled(
            "crew".into(),
            "work".into(),
            "coder".into(),
            Utc::now(),
            flotilla_protocol::StallReason::Scope,
            None,
            "blocked".into(),
        ),
    )
    .await
    .expect("stall");
    let convoy = convoys.get("crew").await.expect("convoy");
    let mut status = convoy.status.expect("status");
    status.phase = ConvoyPhase::Active;
    status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Running;
    // This scenario escalates an ordinary convoy settlement obligation.
    status.workflow_snapshot.as_mut().expect("workflow").exit = Some(flotilla_resources::ExitDeclaration::standard_table());
    status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew[0].completion_conditions =
        vec![flotilla_resources::CrewCompletionExpectation::artifact_exists(
            "coder",
            "decision-ledger",
            flotilla_resources::ArtifactSubjectBinding::Convoy,
        )];
    status.crew_work.get_mut("work").expect("crew").insert("bosun".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build());
    if collocated && bound {
        status
            .crew_work
            .get_mut("work")
            .expect("crew")
            .insert("deputy".into(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build());
    }
    status.workflow_snapshot.as_mut().expect("workflow").supervision = Some(vec![
        SupervisionTarget::ConvoyCrew { vessel: "work".into(), role: "bosun".into() },
        if operator && !bound {
            SupervisionTarget::Operator
        } else if collocated {
            SupervisionTarget::ConvoyCrew { vessel: "work".into(), role: "deputy".into() }
        } else {
            SupervisionTarget::Address { address: receiver.into() }
        },
    ]);
    let stall = status.stalled.as_mut().expect("stall");
    // Mixed inferred leaves can put a non-crew leaf first. The request still
    // identifies its stalled actor, so operator escalation must retain that maker.
    if operator {
        let mut inferred = stall.leaves[0].clone();
        inferred.field_path = ".phase".into();
        stall.leaves.insert(0, inferred);
    }
    stall.rung = StallRung::Bosun;
    stall.supervision_index = cursor.then_some(0);
    stall.supervisor = Some(StallSupervisor { convoy: "crew".into(), vessel: "work".into(), role: "bosun".into() });
    stall.maker = Some(flotilla_resources::LeafMaker::Supervisor { convoy: "crew".into(), vessel: "work".into(), role: "bosun".into() });
    convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("owner");
}
async fn seed_next_receiver(backend: &ResourceBackend) {
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let next = convoys
        .create(
            &InputMeta::builder().name("next".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).role("governor".into()).build(),
        )
        .await
        .expect("next convoy");
    convoys
        .update_status(
            "next",
            &next.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                crew_work: BTreeMap::from([(
                    "watch".into(),
                    BTreeMap::from([("governor".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("next status");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let mut spec = sessions.get("session").await.expect("source session").spec;
    spec.role = "governor".into();
    let terminal = sessions
        .create(
            &InputMeta::builder()
                .name("next-session".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "next".into()),
                    (VESSEL_LABEL.into(), "watch".into()),
                    (ROLE_LABEL.into(), "governor".into()),
                ]))
                .build(),
            &spec,
        )
        .await
        .expect("next terminal");
    sessions
        .update_status(
            "next-session",
            &terminal.metadata.resource_version,
            &TerminalSessionStatus {
                phase: TerminalSessionPhase::Running,
                attention: Some(flotilla_resources::TerminalAttention {
                    state: flotilla_resources::TerminalAttentionState::Working,
                    source: flotilla_resources::TerminalAttentionSource::Hook,
                    as_of: Utc::now(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("receiver working");
}
async fn seed_collocated_receiver(backend: &ResourceBackend) {
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let mut spec = sessions.get("session").await.expect("source session").spec;
    spec.role = "deputy".into();
    let terminal = sessions
        .create(
            &InputMeta::builder()
                .name("deputy-session".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "crew".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "deputy".into()),
                ]))
                .build(),
            &spec,
        )
        .await
        .expect("local receiver binding");
    sessions
        .update_status(
            "deputy-session",
            &terminal.metadata.resource_version,
            &TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() },
        )
        .await
        .expect("local receiver");
}

// Glue: resolution errors retain their command-error channel rather than claiming
// a receiver is absent. The shared resolver validates this address before admission.
#[tokio::test]
async fn escalation_resolution_error_is_not_a_receiver_refusal() {
    let (crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    seed_escalation_stall(&backend, true, false, false, true, "invalid-address").await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let before = convoys.get("crew").await.expect("source");
    let error = crew
        .leaf_subscriptions
        .escalate_stall("flotilla", &before, "principal:operator", "handle this", "work", "coder")
        .await
        .err()
        .expect("lookup error must not become a typed refusal");
    assert!(error.contains("invalid"), "{error}");
    assert_eq!(convoys.get("crew").await.expect("source").status, before.status);
    assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.is_empty());
}

// A successful command names the durable recipient and transfers the owner/cursor.
fn assert_escalation_transferred(
    result: &flotilla_protocol::CommandValue,
    after: &Option<flotilla_resources::StalledCondition>,
    records: &[flotilla_resources::ResourceObject<Message>],
    receiver: &str,
    operator: bool,
    collocated: bool,
) {
    let flotilla_protocol::CommandValue::CrewEscalated { receiver: actual } = result else {
        panic!("successful transfer must name its receiver: {result:?}");
    };
    assert_eq!(actual, receiver);
    let stall = after.as_ref().expect("stall");
    if operator {
        assert!(stall.supervisor.is_none());
        assert_eq!(stall.rung, StallRung::Operator);
        assert_eq!(stall.maker, Some(flotilla_resources::LeafMaker::Actor { vessel: "work".into(), role: "coder".into() }));
    } else {
        assert_eq!(stall.supervisor.as_ref().expect("owner").convoy, if collocated { "crew" } else { "next" });
    }
    assert_eq!(stall.supervision_index, Some(1));
    assert_eq!(records.len(), 2);
    let escalation = records.iter().find(|record| record.spec.receiver == receiver).expect("next receiver Message");
    assert_eq!(escalation.spec.relation, MessageRelation::Supervisor);
    assert_eq!(escalation.spec.expectation, if operator { MessageExpectation::None } else { MessageExpectation::Reply });
}

// Receiver refusal preserves the existing owner and creates no notifications.
fn assert_escalation_refused(
    result: &flotilla_protocol::CommandValue,
    after: &Option<flotilla_resources::StalledCondition>,
    before: &Option<flotilla_resources::StalledCondition>,
    records: &[flotilla_resources::ResourceObject<Message>],
    operator: bool,
    collocated: bool,
) {
    let expected_rung = if operator {
        StallRung::Operator
    } else if collocated {
        StallRung::Bosun
    } else {
        StallRung::Governor
    };
    assert!(matches!(result, flotilla_protocol::CommandValue::CrewEscalationRefused { rung, .. } if *rung == expected_rung));
    assert_eq!(after, before);
    assert!(records.is_empty());
}

// Owner ruling in #3430: retained terminal convoys never resume legacy briefs.
#[tokio::test]
async fn terminal_legacy_brief_is_ignored() {
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get("crew").await.expect("convoy");
    let mut status = convoy.status.expect("status");
    status.phase = ConvoyPhase::Landed;
    status.turn_deliveries.entry("operator".into()).or_default().pending_brief = Some(
        serde_json::from_value(serde_json::json!({
            "vessel": "work", "role": "coder", "content": "ignore terminal intent", "queued_at": "2026-10-01T00:00:00Z",
            "sender": {"kind": "operator-resume", "principal": {"namespace": "flotilla", "name": "alice"}}
        }))
        .expect("legacy brief"),
    );
    convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("terminal queue");
    crew.adopt_legacy_convoy_queues_once("flotilla", None).await.expect("adoption");
    crew.reconcile_pending_supervisor_turns_once("flotilla").await.expect("reconcile");
    assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("messages").items.is_empty());
    assert_eq!(convoys.get("crew").await.expect("terminal").status, Some(status));
}

#[tokio::test]
async fn operator_brief_survives_a_racing_nudge_until_delivery() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
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
        .create(&InputMeta::builder().name("convoy".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
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

// Behaviour (#3587 layer 7): a send's awaiting-reply record is a stall when it supervises a
// stalled crew, else the kind its expectation names; a send expecting nothing records nothing.
#[test]
fn awaiting_records_take_the_stall_or_the_expectations_kind() {
    use flotilla_resources::MessageExpectation;
    let outcome = MessageExpectation::Outcome {
        condition: flotilla_protocol::Leaf {
            address: flotilla_protocol::LeafAddress::Work { convoy: "c".into(), work: "work".into() },
            field_path: ".crew.coder.phase".into(),
            operator: flotilla_protocol::LeafOperator::Equal,
            literal: "Done".into(),
        },
    };
    assert_eq!(super::super::awaiting_kind(&MessageExpectation::None, Some("p/c/work/coder")), None);
    assert_eq!(super::super::awaiting_kind(&MessageExpectation::Reply, Some("p/c/work/coder")), Some("stall"));
    assert_eq!(super::super::awaiting_kind(&MessageExpectation::Reply, None), Some("reply"));
    assert_eq!(super::super::awaiting_kind(&outcome, None), Some("outcome"));
}

// Resume names the environment's recorded loss even before the vessel status
// catches up, including a session previously misclassified as failed.
#[hegel::test]
fn lost_environment_resume_reports_cause(tc: hegel::TestCase) {
    use flotilla_resources::{
        Environment, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus, HostDirectEnvironmentSpec, VesselPhase, VesselSpec, VesselStatus,
    };
    let failed_session = tc.draw(hegel::generators::booleans());
    let provisioning_failed = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
        let environments = backend.using::<Environment>("flotilla");
        let env = environments
            .create(
                &InputMeta::builder().name("resume-env".into()).build(),
                &EnvironmentSpec {
                    cloud_sandbox: None,
                    provider_address: None,
                    docker: None,
                    host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
                },
            )
            .await
            .expect("environment");
        environments
            .update_status(
                "resume-env",
                &env.metadata.resource_version,
                &EnvironmentStatus {
                    phase: if provisioning_failed { EnvironmentPhase::Failed } else { EnvironmentPhase::Lost },
                    message: Some("cgroup_oom".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("lost environment");
        let vessels = backend.using::<Vessel>("flotilla");
        let vessel = vessels
            .create(
                &InputMeta::builder().name("crew-work".into()).build(),
                &VesselSpec {
                    convoy_ref: "crew".into(),
                    vessel_name: "work".into(),
                    placement_policy_ref: "policy".into(),
                    adopted_checkout_refs: Default::default(),
                },
            )
            .await
            .expect("vessel");
        vessels
            .update_status(
                "crew-work",
                &vessel.metadata.resource_version,
                &VesselStatus { phase: VesselPhase::Ready, environment_ref: Some("resume-env".into()), ..Default::default() },
            )
            .await
            .expect("stale vessel");
        let sessions = backend.using::<ResourceTerminalSession>("flotilla");
        let session = sessions.get("session").await.expect("session");
        sessions
            .update_status(
                "session",
                &session.metadata.resource_version,
                &TerminalSessionStatus {
                    phase: if failed_session { TerminalSessionPhase::Failed } else { TerminalSessionPhase::Lost },
                    ..Default::default()
                },
            )
            .await
            .expect("session status");
        let error = crew.resume("flotilla", "crew", "resume", Some("work"), Some("coder")).await.expect_err("lost backing");
        assert!(error.contains("cgroup_oom"), "{error}");
        if provisioning_failed {
            assert!(error.contains("failed provisioning"), "{error}");
        } else {
            assert!(error.contains("lost, recoverable") && error.contains("#2872"), "{error}");
            assert!(!error.contains("failed provisioning"), "{error}");
        }
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    });
}

// A terminal failure with no evidence of provisioning failure must not be
// described as failed provisioning (glue: refusal text from session status).
#[tokio::test]
async fn failed_session_resume_does_not_invent_provisioning_failure() {
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Interrupted).await;
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("session").await.expect("session");
    sessions
        .update_status(
            "session",
            &session.metadata.resource_version,
            &TerminalSessionStatus {
                phase: TerminalSessionPhase::Failed,
                message: Some("agent launch rejected".into()),
                ..Default::default()
            },
        )
        .await
        .expect("failed session");
    let error = crew.resume("flotilla", "crew", "resume", Some("work"), Some("coder")).await.expect_err("failed session");
    assert!(error.contains("agent launch rejected"), "{error}");
    assert!(!error.contains("failed provisioning"), "{error}");
}
