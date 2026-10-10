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
use flotilla_store::{apply_status_patch as apply_resource_status_patch, InMemoryBackend, ResourceBackend};
use tokio::sync::RwLock;

use super::fixture;
use crate::in_process::crew_ops::{CrewService, CrewSupervisionRequest, ResolvedCrewContext};
use crate::in_process::{Vessel, WorkCredentialReconciler};

#[hegel::test]
fn cross_vessel_handoff_publishes_typed_messages(tc: hegel::TestCase) {
    use flotilla_resources::{Message, MessageReference, MessageRelation, MessageSpec, VesselSpec};
    use flotilla_store::MessageInbox;
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
                flotilla_store::MessageAdmission::Accepted(record) => record,
                flotilla_store::MessageAdmission::Suppressed { predecessor } => predecessor,
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
        let home = if remote { ResourceBackend::InMemory(Default::default()) } else { backend.clone() };
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
        crew.reconcile_pending_supervisor_turns_once("flotilla").await.expect("adoption");
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
    use flotilla_protocol::CrewSupervisionAction;
    use flotilla_resources::{Message, MessageExpectation, MessageReference, MessageRelation, MessageSpec, StallRung, StallSupervisor};
    use flotilla_store::MessageInbox;
    use hegel::generators as gs;
    let escalation_visible = tc.draw(gs::booleans());
    // Exercise resume, conversion to failed, and escalation, including their distinct workflow effects.
    let action = match tc.draw(gs::integers::<usize>().min_value(0).max_value(2)) {
        0 => CrewSupervisionAction::Resume,
        1 => CrewSupervisionAction::Fail,
        _ => CrewSupervisionAction::Escalate,
    };
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
        probe.fail.store(false, Ordering::SeqCst);
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
        assert_eq!(records.len(), 1 + usize::from(escalation_visible));
        let reply = records.iter().find(|record| record.metadata.name != "source-escalation").expect("ruling Message");
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
    use flotilla_resources::{Message, MessageSpec};
    use flotilla_store::MessageInbox;

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
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].spec.body, "durable ruling");
        assert_eq!(records[0].spec.receiver, "flotilla/crew/work/coder");
        assert_eq!(records[0].spec.expectation, flotilla_resources::MessageExpectation::None);
    }
}

// #2758: holds persist convoy/session facts and admit exactly one plain supervisor
// Message across retries, with no forge collaborator available. Cover local and project
// supervisors, no PR binding, and retries carrying newer head identities.
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
        assert_eq!(messages.items.len(), 1, "retries cannot duplicate supervisor signals");
        let message = &messages.items[0];
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
            flotilla_store::MessageInbox::new(self.source.clone(), namespace)
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
    let receiver = ResourceBackend::InMemory(InMemoryBackend::default());
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
    assert_eq!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.len(), 1);
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
        .checkout_providers(crew.checkout_providers.clone())
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
        status.crew_work.get_mut("work").unwrap().get_mut("coder").unwrap().phase = crew_phase;
        let convoy = convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("crew outcome");
        let view = restarted.crew_state(&context, &convoy).await.expect("view");
        assert_eq!(view.members[0].state, expected, "{session_phase:?}, {crew_phase:?}");
        assert_eq!(view.members[0].reason.is_some(), expected == "lost");
    }
}
