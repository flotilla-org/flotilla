use flotilla_resources::ResumeIntent;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::{CrewCommandContext, PrincipalRef, ResourceRef};
use flotilla_resources::{
    convoy::engine::{EngineConvoyPath, EngineConvoys},
    external_patches as convoy_external_patches, Convoy as ResourceConvoy, ConvoyPhase, CrewWorkPhase, HoldAct, WorkflowSnapshot,
};
use flotilla_store::ResourceBackend;
use tokio::sync::RwLock;

use super::{fixture, prelude_fixture, shaped_fixture, Crewed, Held};
use crate::crew_ops::{CrewService, CrewSupervisionRequest, CrewTurnDeliveryActuator, ResolvedCrewContext};
use crate::ports::WorkCredentialReconciler;

/// A second vessel `review`, crewed by agent `reviewer` owing a decision ledger when `owes`.
fn with_review(owes: bool) -> impl FnOnce(&mut WorkflowSnapshot) {
    move |snapshot| {
        let mut review = snapshot.vessels[0].clone();
        review.name = "review".into();
        review.crew[0].role = "reviewer".into();
        review.crew[0].promises = if owes { vec![flotilla_resources::TemplatePromise::decision_ledger("reviewer")] } else { Vec::new() };
        snapshot.vessels.push(review);
    }
}

/// Agent `member` beside the coder in `work`.
fn with_member(snapshot: &mut WorkflowSnapshot, member: &str) {
    let mut crew = snapshot.vessels[0].crew[0].clone();
    crew.role = member.into();
    crew.promises = Vec::new();
    snapshot.vessels[0].crew.push(crew);
}

// Every combination of receiver home, publication outcome, receiver phase and repeat count.
#[tokio::test]
async fn cross_vessel_handoff_publishes_typed_messages() {
    for remote in [false, true] {
        for publication_fails in [false, true] {
            for receiver_phase in [CrewWorkPhase::Done, CrewWorkPhase::Pending] {
                for repeats in 1..=3 {
                    cross_vessel_handoff_case(remote, publication_fails, receiver_phase, repeats).await;
                }
            }
        }
    }
}

async fn cross_vessel_handoff_case(remote: bool, publication_fails: bool, receiver_phase: CrewWorkPhase, repeats: usize) {
    use flotilla_engine_messages_testkit::EngineMessagesHome;
    use flotilla_resources::{MessageReference, MessageRelation};

    use crate::leaf_engine::ResourceIntentPublisher;
    // Stand-in for the cross-host resource mutation endpoint; admission is the receiver home's.
    struct ReceiverHome {
        messages: EngineMessagesHome,
        fail: bool,
    }
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverHome {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            if self.fail {
                return Err("receiver publication unavailable".into());
            }
            assert_eq!(document["kind"], "Message");
            let intent: flotilla_messaging::Intent = serde_json::from_value(document["spec"].clone()).expect("Message intent");
            let name = document["metadata"]["name"].as_str().expect("message name");
            let admitted = self.messages.primitives().admit(name, &intent).await.map_err(|error| error.to_string())?;
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, admitted.record))
        }
    }
    let (crew, backend, probe, held) = shaped_fixture(CrewWorkPhase::Working, with_review(receiver_phase == CrewWorkPhase::Done)).await;
    probe.fail.store(false, Ordering::SeqCst);
    let launch = held.launch.clone();
    if receiver_phase == CrewWorkPhase::Done {
        held.engine.keep(&backend, &launch, "review", "reviewer").await;
    }
    assert_eq!(held.convoy(&backend).await.status.expect("status").crew_work["review"]["reviewer"].phase, receiver_phase);
    // A remote receiver home keeps its own Message store; it reads the same declarations.
    let messages = if remote {
        EngineMessagesHome::open("remote", "flotilla", &backend)
    } else {
        EngineMessagesHome::over(Arc::clone(held.messages().service()), "flotilla", &backend)
    };
    let home = Arc::new(ReceiverHome { messages, fail: publication_fails });
    let publisher: Arc<dyn ResourceIntentPublisher> = home.clone();
    if remote || publication_fails {
        crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
    }
    let context = CrewCommandContext::builder().convoy(launch.clone()).vessel_ref(held.vessel.clone()).role("coder".into()).build();
    let convoy = held.convoy(&backend).await;
    let carry = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", &launch),
        revision: convoy.metadata.resource_version.clone(),
    };
    // What the handoff carries, typed: the convoy at its revision, which the view spells as `carry`.
    let carried = flotilla_address::Reference {
        address: flotilla_resources::convoy::engine::convoy_reference_address(&launch, Some("flotilla")).expect("convoy address"),
        revision: flotilla_address::Revision::new(convoy.metadata.resource_version.clone()).expect("revision"),
    };
    let reviewer = held.receiver("review", "reviewer");
    let short = format!("{launch}/review/reviewer");
    if publication_fails {
        let before = held.convoy(&backend).await.status;
        for _ in 0..repeats {
            crew.handoff_with(&context, &short, "review this", vec![carried.clone()], &[]).await.expect_err("receiver publication fails");
            assert_eq!(held.convoy(&backend).await.status, before);
            assert!(home.messages.views().await.expect("inbox").is_empty());
        }
        return;
    }
    for index in 0..repeats {
        let target = if index % 2 == 0 { short.as_str() } else { reviewer.as_str() };
        crew.handoff_with(&context, target, "review this", vec![carried.clone()], &[]).await.expect("handoff");
        // The handoff delivers the reviewer its task: it is wanted running, and its vessel starts it.
        assert!(held.convoy(&backend).await.status.expect("status").crew_work["review"]["reviewer"].task_assigned);
        // The crew's view of a local Message spells its receiver by the vessel that holds it.
        let records: Vec<_> = home.messages.views().await.expect("receiver inbox").into_iter().map(|read| read.object).collect();
        assert_eq!(records.len(), index + 1, "each handoff is distinct even when its body repeats");
        for record in records {
            assert_eq!(record.spec.sender, held.receiver("work", "coder"));
            assert_eq!(record.spec.receiver, reviewer);
            assert_eq!(record.spec.relation, MessageRelation::Peer);
            assert_eq!(record.spec.references, vec![carry.clone()]);
            assert_eq!(record.spec.body, "review this");
        }
    }
    for invalid in [format!("{launch}/missing/reviewer"), "other/review/reviewer".into(), "principal:operator".into(), "coder".into()] {
        crew.handoff(&context, &invalid, "review").await.expect_err("invalid crew target");
    }
    crew.handoff(&context, &short, "").await.expect_err("empty handoff");
    assert_eq!(home.messages.views().await.expect("inbox").len(), repeats);
}

// b0's relaunch: the reviewer's session of a superseded vessel launch gave up (its environment
// was not registered after a restart) beside its successor's live session. A handoff reaches the
// live one. With only the given-up session, the handoff is refused: its crew cannot be revived.
#[tokio::test]
async fn a_handoff_passes_a_given_up_launch_for_its_relaunch() {
    use flotilla_provisioning_testkit::TerminalsFixture;
    use flotilla_resources::vessel_records::VesselStep;

    // Stages the reviewer's credentials: the fixture's probe stages only for the coder's resume.
    struct Stages;
    #[async_trait]
    impl WorkCredentialReconciler for Stages {
        async fn reconcile(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let (crew, _backend, _probe, held) = shaped_fixture(CrewWorkPhase::Working, with_review(false)).await;
    *crew.work_credential_reconciler.write().await = Some(Arc::new(Stages));
    let engine = &held.engine;
    let first = engine.vessel(&held.launch, "review", "resume-env");
    let gave_up = engine.engine.terminals.request(&TerminalsFixture::agent(&first, "reviewer", "resume-env"));
    engine.engine.terminals.give_up(&gave_up, "provider registry unavailable for environment resume-env");
    let context = CrewCommandContext::builder().convoy(held.launch.clone()).vessel_ref(held.vessel.clone()).role("coder".into()).build();
    let target = format!("{}/review/reviewer", held.launch);
    let error = crew.handoff(&context, &target, "review this").await.expect_err("a lone given-up holder");
    assert!(error.contains("failed provisioning and cannot be revived"), "{error}");

    engine.engine.vessels.step(&first, VesselStep::Lost { reason: "its environment disappeared".into() });
    let relaunched = engine.vessel(&held.launch, "review", "resume-env");
    assert_ne!(relaunched, first);
    engine.launch_session(&TerminalsFixture::agent(&relaunched, "reviewer", "resume-env"));
    crew.handoff(&context, &target, "review this").await.expect("the relaunch's live session holds the role");
}

// Behaviour: a handoff no holder will come for is refused, and its sender's seat stalls with the
// refusal as its report (source Crew, reason Other), so the sender's supervision hears of it.
#[tokio::test]
async fn a_handoff_no_holder_will_come_for_stalls_its_sender() {
    use flotilla_provisioning_testkit::TerminalsFixture;

    let (crew, backend, _probe, held) = shaped_fixture(CrewWorkPhase::Working, with_review(false)).await;
    let engine = &held.engine;
    let reviewer = engine.vessel(&held.launch, "review", "resume-env");
    let gave_up = engine.engine.terminals.request(&TerminalsFixture::agent(&reviewer, "reviewer", "resume-env"));
    engine.engine.terminals.give_up(&gave_up, "provider registry unavailable for environment resume-env");
    let context = CrewCommandContext::builder().convoy(held.launch.clone()).vessel_ref(held.vessel.clone()).role("coder".into()).build();
    let target = format!("{}/review/reviewer", held.launch);
    let error = crew.handoff(&context, &target, "review this").await.expect_err("no holder will come");
    assert!(error.contains("failed provisioning and cannot be revived"), "{error}");

    let status = held.convoy(&backend).await.status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled);
    let stalled = status.stalls.about(Some(("work", "coder"))).cloned().expect("the sender's own stall");
    assert_eq!(stalled.source, flotilla_resources::StallEvidenceSource::Crew);
    assert_eq!(stalled.reason, Some(flotilla_protocol::StallReason::Other));
    assert!(stalled.leaves.iter().any(|leaf| leaf.field_path == ".crew.coder.phase"), "{stalled:?}");
    assert!(stalled.evidence.contains("cannot be delivered") && stalled.evidence.contains(&error), "{}", stalled.evidence);
}

// Behaviour: a system turn no holder will come for goes to the operator: a turn-rules turn to a
// crew whose work is done and that holds no session relaunches nothing, so it is refused, and the
// operator is told once however often the rule fires.
#[tokio::test]
async fn a_system_turn_no_holder_will_come_for_tells_the_operator() {
    let (crew, backend, _probe, held) = shaped_fixture(CrewWorkPhase::Working, with_review(true)).await;
    held.engine.keep(&backend, &held.launch, "review", "reviewer").await;
    assert_eq!(held.convoy(&backend).await.status.expect("status").crew_work["review"]["reviewer"].phase, CrewWorkPhase::Done);
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(held.launch.clone())
        .source("review".into())
        .vessel("review".into())
        .role("reviewer".into())
        .brief("address review".into())
        .subject_revision("head".into())
        .sender(crate::leaf_engine::Party::fleet("turn-rules"))
        .build();
    for _ in 0..2 {
        let error = crew.deliver_turn(&request).await.expect_err("no holder will come");
        assert!(error.contains("relaunches nothing"), "{error}");
    }
    let told: Vec<_> = held
        .messages()
        .views()
        .await
        .expect("inbox")
        .into_iter()
        .map(|read| read.object.spec)
        .filter(|message| message.body.contains("cannot be delivered"))
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].receiver.contains("operator"), "{}", told[0].receiver);
    assert!(told[0].body.contains("review/reviewer"), "{}", told[0].body);
    let status = held.convoy(&backend).await.status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working, "a system sender stalls no seat");
}

// An authenticated operator can send supervisor guidance to crew that has no stall.
#[tokio::test]
async fn operator_supervision_messages_working_crew() {
    let (crew, backend, _probe, held) = fixture(CrewWorkPhase::Working).await;
    let principal = PrincipalRef { namespace: "flotilla".into(), name: "operator-alice".into() };
    crew.supervise(CrewSupervisionRequest {
        namespace: "flotilla",
        convoy_name: &held.launch,
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
    // The engine stores a person as the role they send in; the principal goes to the trace.
    assert_eq!(messages[0].spec.sender, "project:flotilla/role:operator");
    let viewed = held.messages().views().await.expect("inbox");
    assert_eq!(viewed[0].object.spec.receiver, held.receiver("work", "coder"));
    assert_eq!(messages[0].spec.relation, flotilla_resources::MessageRelation::Supervisor);
    assert_eq!(messages[0].spec.body, "please follow up");
    assert!(held.convoy(&backend).await.status.expect("status").stalled.is_none());
}

/// Supervision by the operator alone, the last rung.
fn operator_supervision(snapshot: &mut WorkflowSnapshot) {
    snapshot.supervision = Some(vec![flotilla_resources::SupervisionTarget::Operator]);
}

// Every governor ruling is a Message reply to the exact crew's open escalation.
#[hegel::test]
#[ignore = "engine admission refuses principal: supervision addresses; see decisions 03:20"]
fn governor_rulings_reply_to_the_source_escalation(tc: hegel::TestCase) {
    use flotilla_protocol::CrewSupervisionAction;
    use flotilla_resources::{MessageRelation, StallRung, StallSupervisor};
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
        let (crew, backend, probe, held) = shaped_fixture(CrewWorkPhase::Working, operator_supervision).await;
        probe.fail.store(false, Ordering::SeqCst);
        crew.set_turn_delivery_actuator(Arc::new(CrewTurnDeliveryActuator { crew: Arc::downgrade(&crew) })).await;
        let (governor, governor_crew) = held.admit_crewed("governor", "governor", "watch", "governor", "governor-session").await;
        let convoy = held
            .stall(&backend, |stalled| {
                stalled.rung = StallRung::Governor;
                stalled.supervisor = Some(StallSupervisor { convoy: governor.clone(), vessel: "watch".into(), role: "governor".into() });
            })
            .await;
        let coder = held.receiver("work", "coder");
        let governor_address = flotilla_resources::crew_role_address("flotilla", &governor, "watch", "governor");
        let mut escalation = flotilla_messaging::Intent::new(
            flotilla_resources::convoy::engine::role_inbox(&coder).expect("the coder's seat"),
            flotilla_resources::convoy::engine::role_inbox(&governor_address).expect("the governor's seat"),
            MessageRelation::Supervisor,
            "blocked",
        );
        escalation.expectation = flotilla_messaging::IntentExpectation::Reply;
        escalation.references = vec![flotilla_messaging::IntentReference::new(
            flotilla_resources::convoy::engine::convoy_reference_address(&held.launch, Some("flotilla")).expect("the convoy's address"),
            flotilla_address::Revision::new(convoy.metadata.resource_version).expect("revision"),
        )];
        let messages = held.messages().messages();
        if escalation_visible {
            messages.admit("source-escalation", &escalation).await.expect("escalation");
        }
        // No visible escalation and no supervision index must produce an unlinked ruling,
        // rather than inventing a record ID.
        crew.supervise(CrewSupervisionRequest {
            namespace: "flotilla",
            convoy_name: &held.launch,
            vessel: "work",
            role: "coder",
            operation: action,
            message: "ruling",
            actor_crew_id: Some(&governor_crew),
            principal: None,
        })
        .await
        .expect("governor ruling");
        let records = held.messages().views().await.expect("messages");
        assert_eq!(records.len(), 1 + usize::from(escalation_visible) + usize::from(action == CrewSupervisionAction::Escalate));
        let reply = records
            .iter()
            .map(|record| &record.object)
            .find(|record| record.metadata.name != "source-escalation" && record.spec.receiver == coder)
            .expect("ruling Message");
        assert_eq!(reply.spec.sender, governor_address);
        assert_eq!(reply.spec.receiver, coder);
        assert_eq!(reply.spec.in_reply_to.as_deref(), escalation_visible.then_some("source-escalation"));
        assert_eq!(reply.spec.relation, MessageRelation::Supervisor);
        let status = held.convoy(&backend).await.status.expect("status");
        match action {
            CrewSupervisionAction::Resume => assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working),
            CrewSupervisionAction::Fail => assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Failed),
            CrewSupervisionAction::Escalate => assert_eq!(status.stalled.expect("escalated stall").rung, StallRung::Operator),
        }
    });
}

// A durable ruling survives its convoy ending after publication as a notification; Message
// delivery never applies Fail/Escalate workflow transitions itself.
#[tokio::test]
#[ignore = "engine admission refuses principal: supervision addresses; see decisions 03:20"]
async fn supervisor_decision_survives_authority_disappearing_after_publication() {
    use flotilla_messaging::EngineMessages;

    use crate::leaf_engine::ResourceIntentPublisher;
    // Boundary stand-in: the receiver admits intent, then an operator abandons the convoy before
    // the supervisor's state change can apply to it.
    struct ReceiverAndTeardown {
        backend: ResourceBackend,
        messages: Arc<dyn EngineMessages>,
        engine: Arc<dyn EngineConvoys>,
        launch: String,
    }
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverAndTeardown {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            let name = document["metadata"]["name"].as_str().expect("name").to_string();
            let intent: flotilla_messaging::Intent = serde_json::from_value(document["spec"].clone()).expect("intent");
            self.messages.admit(&name, &intent).await.map_err(|error| error.to_string())?;
            let convoy = self.backend.using::<ResourceConvoy>(namespace).get(&self.launch).await.map_err(|error| error.to_string())?;
            self.engine.abandon(&convoy, "operator", "torn down").await?;
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, name))
        }
    }
    for action in [flotilla_protocol::CrewSupervisionAction::Fail, flotilla_protocol::CrewSupervisionAction::Escalate] {
        let (crew, backend, probe, held) = shaped_fixture(CrewWorkPhase::Working, operator_supervision).await;
        held.stall(&backend, |stalled| stalled.rung = flotilla_resources::StallRung::Governor).await;
        probe.fail.store(false, Ordering::SeqCst);
        crew.set_turn_delivery_actuator(Arc::new(super::super::CrewTurnDeliveryActuator { crew: Arc::downgrade(&crew) })).await;
        let messages = held.messages().messages();
        let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(ReceiverAndTeardown {
            backend: backend.clone(),
            messages: messages.clone(),
            engine: held.engine.engine(),
            launch: held.launch.clone(),
        });
        crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
        let principal = PrincipalRef::implicit_for_namespace("flotilla");
        crew.supervise(CrewSupervisionRequest {
            namespace: "flotilla",
            convoy_name: &held.launch,
            vessel: "work",
            role: "coder",
            operation: action,
            message: "durable ruling",
            actor_crew_id: None,
            principal: Some(&principal),
        })
        .await
        .expect_err("the ruling cannot apply to an ended convoy");
        let records = held.messages().views().await.expect("decisions");
        assert_eq!(records.len(), if action == flotilla_protocol::CrewSupervisionAction::Escalate { 2 } else { 1 });
        let ruling =
            records.iter().map(|record| &record.object).find(|record| record.spec.body == "durable ruling").expect("durable ruling");
        assert_eq!(ruling.spec.receiver, held.receiver("work", "coder"));
        assert_eq!(ruling.spec.expectation, flotilla_resources::MessageExpectation::None);
    }
}

// #2758: holds persist on the convoy and admit exactly one plain supervisor
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
        let (crew, backend, _, held) = shaped_fixture(CrewWorkPhase::Done, |snapshot| {
            if local_supervisor {
                with_member(snapshot, "bosun");
            }
            if address_supervisor {
                snapshot.supervision =
                    Some(vec![flotilla_resources::SupervisionTarget::Address { address: "project:flotilla/role:attended".into() }]);
            }
        })
        .await;
        if local_supervisor {
            held.crew_session("bosun", "bosun-session");
            held.assign(&backend, "work", "bosun").await;
            assert_eq!(held.convoy(&backend).await.status.expect("status").crew_work["work"]["bosun"].phase, CrewWorkPhase::Working);
        }
        let mut request = crate::leaf_engine::CrewTurnIntent::builder()
            .namespace("flotilla".into()).convoy(held.launch.clone()).source("checks-settled".into())
            .vessel("work".into()).role("coder".into()).brief("continue".into())
            .subject_revision("head-one".into()).sender(crate::leaf_engine::Party::fleet("turn-rules")).build();
        for retry in 0..retries {
            request.subject_revision = format!("head-{retry}");
            crew.execute_turn_delivery_hold(&request, &HoldAct::State, "episode limit").await.expect("hold");
        }
        let convoy = held.convoy(&backend).await;
        assert_eq!(convoy.status.expect("status").attention.expect("attention").reason, "episode limit");
        let messages: Vec<_> =
            held.messages().views().await.expect("messages").into_iter().map(|read| read.object).collect();
        assert_eq!(messages.len(), 2, "retries cannot duplicate the supervisor signal or the crew notice");
        // #3565: the held crew is told why its wakes stopped.
        let coder = held.receiver("work", "coder");
        let notice = messages.iter().find(|message| message.spec.receiver == coder).expect("crew notice");
        assert_eq!(notice.spec.relation, flotilla_resources::MessageRelation::System);
        assert!(notice.spec.body.contains("`checks-settled` turns for you are held") && notice.spec.body.contains("episode limit"));
        let message = messages.iter().find(|message| message.spec.receiver != coder).expect("supervisor signal");
        let expected = if address_supervisor {
            "project:flotilla/role:attended".to_owned()
        } else if local_supervisor {
            held.receiver("work", "bosun")
        } else {
            "flotilla/governor".to_owned()
        };
        assert_eq!(message.spec.receiver, expected);
        assert_eq!(message.spec.relation, flotilla_resources::MessageRelation::System);
        assert_eq!(message.spec.expectation, flotilla_resources::MessageExpectation::None);
        assert!(message.spec.body.contains("episode limit"));
        assert!(matches!(message.spec.references.last(), Some(flotilla_resources::MessageReference::ControlRecord { resource, .. }) if resource.name == held.launch));
    });
}

/// The Message rows loaded and converted by a repeated hold, both its messages already sent,
/// with `count` other messages open to the held coder.
async fn read_by_a_repeated_hold(count: usize) -> (u64, u64) {
    let (crew, _backend, _, held) = fixture(CrewWorkPhase::Done).await;
    let primitives = held.messages().primitives();
    for index in 0..count {
        let intent = flotilla_messaging::Intent::new(
            flotilla_engine_messages_testkit::party("system:turn-rules"),
            flotilla_resources::convoy::engine::role_inbox(&held.receiver("work", "coder")).expect("the coder's inbox"),
            flotilla_resources::MessageRelation::System,
            format!("open {index}"),
        );
        primitives.admit(&format!("open-{index}"), &intent).await.expect("admit an open message");
    }
    let request = crate::leaf_engine::CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(held.launch.clone())
        .source("checks-settled".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("continue".into())
        .subject_revision("head".into())
        .sender(crate::leaf_engine::Party::fleet("turn-rules"))
        .build();
    crew.execute_turn_delivery_hold(&request, &HoldAct::State, "episode limit").await.expect("hold");
    let (held, reads) =
        flotilla_store::load_metrics::measure(crew.execute_turn_delivery_hold(&request, &HoldAct::State, "episode limit")).await;
    held.expect("repeated hold");
    reads.get("Message").map_or((0, 0), |counts| (counts.decoded, counts.converted))
}

// Behaviour: a repeated hold finds its crew notice and supervisor signal by their identities,
// loading and converting neither, whatever else the crew holds.
#[tokio::test]
async fn a_repeated_hold_finds_its_messages_without_converting_them() {
    assert_eq!(read_by_a_repeated_hold(5).await, (0, 0), "five other messages");
    assert_eq!(read_by_a_repeated_hold(60).await, (0, 0), "sixty other messages");
}

// All terminal convoy outcomes refuse resume and turn admission with an explicit
// continuation path. Refusal leaves the convoy and Message inbox unchanged.
#[tokio::test]
async fn terminal_convoys_refuse_resume_and_turn_delivery() {
    for phase in [ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Abandoned] {
        let (crew, backend, probe, held) = fixture(CrewWorkPhase::Done).await;
        held.engine.end(&backend, &held.launch, phase).await;
        let settled = held.convoy(&backend).await;
        assert_eq!(settled.status.as_ref().expect("status").phase, phase);
        let error =
            crew.resume("flotilla", &held.launch, "resume", Some("work"), Some("coder")).await.expect_err("terminal resume refuses");
        assert!(error.contains("--continue-pr") && error.contains("new generation"), "{error}");
        let request = crate::leaf_engine::CrewTurnIntent::builder()
            .namespace("flotilla".into())
            .convoy(held.launch.clone())
            .source("review".into())
            .vessel("work".into())
            .role("coder".into())
            .brief("address review".into())
            .subject_revision("head".into())
            .sender(crate::leaf_engine::Party::fleet("turn-rules"))
            .build();
        let error = crew.deliver_turn(&request).await.expect_err("terminal delivery refuses");
        assert!(error.contains("--continue-pr") && error.contains("new generation"), "{error}");
        let after = held.convoy(&backend).await;
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
    let (crew, backend, probe, held) = fixture(CrewWorkPhase::Working).await;
    held.engine.engine.vessels.step(&held.vessel, flotilla_resources::vessel_records::VesselStep::Lost { reason: "host reboot".into() });
    let context = ResolvedCrewContext {
        namespace: "flotilla".into(),
        convoy: held.launch.clone(),
        vessel_ref: held.vessel.clone(),
        vessel: "work".into(),
        caller_role: "coder".into(),
        caller_session: None,
    };
    let convoy = held.convoy(&backend).await;
    let view = crew.crew_state(&context, &convoy).await.expect("crew state");
    assert_eq!(view.members[0].state, "lost");
    assert_eq!(view.members[0].reason.as_deref(), Some("lost, recoverable: host reboot"));
    // The lost vessel interrupts its work: the view derives it from the vessel's record.
    let status = convoy.status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Interrupted);
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Interrupted);
    // A new daemon service has no remembered interruption. It must refuse resume from the lost
    // vessel's record.
    let restarted = CrewService::builder()
        .resource_backend(backend.clone())
        .accounts(Arc::new(flotilla_store_testkit::OldStoreAccounts::new(backend.clone())))
        .leaf_subscriptions(crew.leaf_subscriptions.clone())
        .lifecycle_reads(held.engine.reads())
        .vessel_seats(Arc::clone(&crew.vessel_seats))
        .work_credential_reconciler(RwLock::new(Some(probe.clone() as Arc<dyn WorkCredentialReconciler>)))
        .clock(crew.clock.clone())
        .provisioning_namespace(crew.provisioning_namespace.clone())
        .config(crew.config.clone())
        .host_name(crew.host_name.clone())
        .brief_artifact_writer(crew.brief_artifact_writer.clone())
        .checkout_vcs(crew.checkout_vcs.clone())
        .local_environment_id(crew.local_environment_id.clone())
        .message_path(held.engine.message_path())
        .convoy_engine(held.engine.path())
        .build();
    drop(crew);
    let before = held.convoy(&backend).await;
    let error = restarted.resume("flotilla", &held.launch, "resume", Some("work"), Some("coder")).await.expect_err("no rehydration");
    assert!(error.contains("lost, recoverable") && error.contains("host reboot") && error.contains("#2872"), "{error}");
    assert!(!error.contains("failed provisioning"), "{error}");
    let after = held.convoy(&backend).await;
    assert_eq!(after.status, before.status);
    assert_eq!(after.metadata.resource_version, before.metadata.resource_version);
    assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.is_empty());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

// Settled sessions retain their outcome, and settled crew work does not become lost merely
// because its vessel later loses backing. The crew phase is the view's, as the engine derives it
// for the seat from the seat's records.
#[tokio::test]
async fn settled_crew_on_a_lost_vessel_keeps_its_outcome() {
    for (session_ending, crew_phase, expected) in [
        (Some("exited"), CrewWorkPhase::Interrupted, "stopped"),
        (Some("failed"), CrewWorkPhase::Interrupted, "failed"),
        (None, CrewWorkPhase::Done, "active"),
        (None, CrewWorkPhase::Failed, "active"),
        (None, CrewWorkPhase::HandedBack, "active"),
        (None, CrewWorkPhase::Interrupted, "lost"),
    ] {
        let (crew, backend, _probe, held) = fixture(CrewWorkPhase::Working).await;
        held.engine
            .engine
            .vessels
            .step(&held.vessel, flotilla_resources::vessel_records::VesselStep::Lost { reason: "host reboot".into() });
        match session_ending {
            Some("exited") => held.engine.engine.terminals.exited(&held.session, "agent exited with code 0"),
            Some(_) => held.engine.engine.terminals.failed(&held.session, "agent launch rejected"),
            None => {}
        }
        let context = ResolvedCrewContext {
            namespace: "flotilla".into(),
            convoy: held.launch.clone(),
            vessel_ref: held.vessel.clone(),
            vessel: "work".into(),
            caller_role: "coder".into(),
            caller_session: None,
        };
        let mut convoy = held.convoy(&backend).await;
        convoy.status.as_mut().expect("status").crew_work.get_mut("work").expect("work").get_mut("coder").expect("coder").phase =
            crew_phase;
        let view = crew.crew_state(&context, &convoy).await.expect("view");
        assert_eq!(view.members[0].state, expected, "{session_ending:?}, {crew_phase:?}");
        assert_eq!(view.members[0].reason.is_some(), expected == "lost");
    }
}

// Escalating without a receiver must refuse and preserve the current owner's stall.
#[tokio::test]
async fn escalation_without_receiver_preserves_owner() {
    let (crew, backend, _, held) = fixture(CrewWorkPhase::Working).await;
    let before = held
        .stall(&backend, |stalled| {
            stalled.rung = flotilla_resources::StallRung::Governor;
            stalled.supervision_index = Some(1);
            stalled.supervisor =
                Some(flotilla_resources::StallSupervisor { convoy: "governor".into(), vessel: "watch".into(), role: "governor".into() });
        })
        .await
        .status;
    let result = crew
        .supervise(CrewSupervisionRequest {
            namespace: "flotilla",
            convoy_name: &held.launch,
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
    assert_eq!(held.convoy(&backend).await.status, before);
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
use flotilla_resources::{Message, MessageExpectation, MessageRelation, StallRung, StallSupervisor, SupervisionTarget};
// Boundary fake: receiver-home publication may be unreachable. Successful
// admissions go through the home's Message path, not an accepting mock.
struct Delivery {
    backend: ResourceBackend,
    messages: Arc<dyn flotilla_messaging::EngineMessages>,
    path: EngineConvoyPath,
    launch: String,
    reachable: bool,
    conflict: bool,
    /// Resume the coder at the publication boundary, as a concurrent operator would.
    resume_coder: bool,
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
        // The fixture's convoy declares no project: a person holds their role in the namespace's.
        let (messages, namespace) = (self.messages.as_ref(), request.namespace.as_str());
        let inbox = request.receiver.as_ref().expect("resolved receiver").inbox(messages, namespace, namespace).await?;
        let sender = request.sender.inbox(messages, namespace, namespace).await?;
        let name = flotilla_resources::message_record_name(inbox.as_str(), sender.as_str(), &request.source);
        let intent = crate::crew_ops::turn_intent(request, sender, inbox);
        let record = self.messages.admit(&name, &intent).await.map_err(|error| error.to_string())?;
        let convoys = self.backend.using::<ResourceConvoy>(&request.namespace);
        if request.convoy == self.launch && request.vessel == "work" && request.role == "deputy" {
            // The receiver's home delivers it the turn: its task is noted.
            flotilla_store::note_convoy(
                &self.path,
                &convoys,
                &self.launch,
                &convoy_external_patches::resume_crew_work(
                    request.vessel.clone(),
                    request.role.clone(),
                    Utc::now(),
                    request.brief.clone(),
                    Some(record.record.clone()),
                    ResumeIntent::Reopen,
                ),
            )
            .await
            .expect("local receiver activation");
        }
        // Inject a concurrent source-authority ruling at the publication
        // boundary; escalation must not overwrite it when committing ownership.
        if self.conflict {
            let source = convoys.get(&self.launch).await.expect("source authority");
            let engine = self.path.get().expect("the Convoy path");
            engine.fail_seat(&source, "work", "coder", "operator", "concurrent ruling").await.expect("concurrent ruling");
        }
        if self.resume_coder {
            flotilla_store::note_convoy(
                &self.path,
                &convoys,
                &self.launch,
                &convoy_external_patches::resume_crew_work(
                    "work".into(),
                    "coder".into(),
                    Utc::now(),
                    "concurrent resume".into(),
                    None,
                    ResumeIntent::Reopen,
                ),
            )
            .await
            .expect("concurrent resume");
        }
        Ok(CrewTurnAdmission {
            new_turn: true,
            rung: flotilla_resources::TurnDeliveryRung::WarmSession,
            message: ResourceRef::new("flotilla.work/v1", "Message", &request.namespace, record.record),
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
        // A bound next governor is a working convoy at the home; an unbound one names a convoy
        // the home has never admitted.
        let next = Crewed { name: "next", role: "governor", work: "watch", member: "governor", session: "next-session" };
        let prelude = if bound && !operator && !collocated { std::slice::from_ref(&next) } else { &[] };
        let (crew, backend, _, held) = prelude_fixture(CrewWorkPhase::Working, prelude, |snapshot, admitted| {
            let next = admitted.first().map_or("next#1", |(launch, _)| launch.as_str());
            let next = flotilla_resources::crew_role_address("flotilla", next, "watch", "governor");
            escalation_shape(snapshot, operator, collocated, bound, if operator { "principal:operator" } else { &next })
        })
        .await;
        let next = held.prelude.first().map(|(launch, _)| launch.clone());
        let receiver = if operator {
            "principal:operator".to_owned()
        } else if collocated {
            held.receiver("work", "deputy")
        } else {
            flotilla_resources::crew_role_address("flotilla", next.as_deref().unwrap_or("next#1"), "watch", "governor")
        };
        crew.set_turn_delivery_actuator(Arc::new(Delivery {
            backend: backend.clone(),
            messages: held.messages().messages(),
            path: held.engine.path(),
            launch: held.launch.clone(),
            reachable,
            conflict,
            resume_coder: false,
        }))
        .await;
        let bosun = seed_escalation_stall(&held, &backend, cursor, operator).await;
        if collocated && bound && !missing_terminal {
            held.crew_session("deputy", "deputy-session");
        }
        let before = held.convoy(&backend).await.status.expect("status").stalled;
        let result = crew
            .supervise(CrewSupervisionRequest {
                namespace: "flotilla",
                convoy_name: &held.launch,
                vessel: "work",
                role: "coder",
                operation: flotilla_protocol::CrewSupervisionAction::Escalate,
                message: "please handle this",
                actor_crew_id: Some(&bosun),
                principal: None,
            })
            .await;
        let after = held.convoy(&backend).await.status.expect("status").stalled;
        let records: Vec<_> = held.messages().views().await.expect("messages").into_iter().map(|read| read.object).collect();
        if bound && reachable && conflict {
            assert!(result.is_err(), "a concurrent ruling must refuse the ownership transfer: {result:?}");
            // The ruling failed the coder's seat and so the launch: its view shows no stall, and
            // the home's note of it is the stall as it was.
            assert_eq!(after, None);
            assert_eq!(held.engine.engine().memory().get(&held.launch).stalls.oldest().cloned(), before);
            assert_eq!(
                held.convoy(&backend).await.status.expect("status").crew_work["work"]["coder"].message.as_deref(),
                Some("concurrent ruling")
            );
            assert_eq!(records.len(), 2, "both admitted escalation and durable ruling survive the failed transfer");
            assert_ruling_unlinked(&records, &held);
            continue;
        }
        let result = result.expect("typed outcome");
        let owner = if collocated { held.launch.clone() } else { next.clone().unwrap_or_default() };
        if bound && reachable && (!collocated || !missing_terminal) {
            assert_escalation_transferred(&result, &after, &records, &receiver, operator, &owner);
            assert_ruling_unlinked(&records, &held);
            if collocated {
                assert_eq!(held.convoy(&backend).await.status.expect("status").crew_work["work"]["deputy"].phase, CrewWorkPhase::Working);
                continue;
            }
            for _ in 0..repeats {
                let duplicate = crew
                    .supervise(CrewSupervisionRequest {
                        namespace: "flotilla",
                        convoy_name: &held.launch,
                        vessel: "work",
                        role: "coder",
                        operation: flotilla_protocol::CrewSupervisionAction::Escalate,
                        message: "repeat",
                        actor_crew_id: Some(&bosun),
                        principal: None,
                    })
                    .await;
                assert!(duplicate.is_err(), "the old supervisor no longer owns the stall");
                crew.reconcile_crew_stalls_once("flotilla").await.expect("judge");
                let judged = held.convoy(&backend).await.status.expect("status").stalled.expect("still stalled");
                if operator {
                    assert!(judged.supervisor.is_none());
                    assert_eq!(judged.rung, StallRung::Operator);
                } else {
                    assert_eq!(judged.supervisor.as_ref().expect("owner").convoy, owner);
                }
                assert_eq!(backend.using::<Message>("flotilla").list().await.expect("messages").items.len(), 2);
            }
        } else {
            assert_escalation_refused(&result, &after, &before, &records, operator, collocated);
        }
    }
}

// A collocated deputy's activation clears the coder's stall, which the seat's view derives its
// Stalled phase from; a concurrent change to the coder's seat still refuses the transfer.
#[tokio::test]
async fn collocated_escalation_refuses_a_concurrent_change_to_the_seat() {
    for (conflict, resume_coder) in [(true, false), (false, true)] {
        let (crew, backend, _, held) =
            prelude_fixture(CrewWorkPhase::Working, &[], |snapshot, _| escalation_shape(snapshot, false, true, true, "")).await;
        crew.set_turn_delivery_actuator(Arc::new(Delivery {
            backend: backend.clone(),
            messages: held.messages().messages(),
            path: held.engine.path(),
            launch: held.launch.clone(),
            reachable: true,
            conflict,
            resume_coder,
        }))
        .await;
        let bosun = seed_escalation_stall(&held, &backend, true, false).await;
        held.crew_session("deputy", "deputy-session");
        let result = crew
            .supervise(CrewSupervisionRequest {
                namespace: "flotilla",
                convoy_name: &held.launch,
                vessel: "work",
                role: "coder",
                operation: flotilla_protocol::CrewSupervisionAction::Escalate,
                message: "please handle this",
                actor_crew_id: Some(&bosun),
                principal: None,
            })
            .await;
        let error = result.expect_err("a concurrent change to the coder's seat refuses the transfer");
        assert!(error.contains("stall changed during escalation"), "{error}");
        let status = held.convoy(&backend).await.status.expect("status");
        assert!(status.stalled.as_ref().is_none_or(|stall| stall.supervisor.as_ref().is_none_or(|owner| owner.role != "deputy")));
        let records: Vec<_> = held.messages().views().await.expect("messages").into_iter().map(|read| read.object).collect();
        assert_ruling_unlinked(&records, &held);
    }
}

/// The workflow an escalation scenario's convoy is admitted with: the coder settles an ordinary
/// convoy obligation, and bosun supervises it before the next target (`receiver`, a collocated
/// deputy, or the operator).
fn escalation_shape(snapshot: &mut WorkflowSnapshot, operator: bool, collocated: bool, bound: bool, receiver: &str) {
    // This scenario escalates an ordinary convoy settlement obligation.
    snapshot.exit = Some(flotilla_resources::ExitDeclaration::standard_table());
    snapshot.vessels[0].crew[0].completion_conditions = vec![flotilla_resources::CrewCompletionExpectation::artifact_exists(
        "coder",
        "decision-ledger",
        flotilla_resources::ArtifactSubjectBinding::Convoy,
    )];
    with_member(snapshot, "bosun");
    if collocated && bound {
        with_member(snapshot, "deputy");
    }
    snapshot.supervision = Some(vec![
        SupervisionTarget::ConvoyCrew { vessel: "work".into(), role: "bosun".into() },
        if operator && !bound {
            SupervisionTarget::Operator
        } else if collocated {
            SupervisionTarget::ConvoyCrew { vessel: "work".into(), role: "deputy".into() }
        } else {
            SupervisionTarget::Address { address: receiver.into() }
        },
    ]);
}

/// The coder's stall, owned by its working bosun at the bosun rung; returns the bosun's crew id.
async fn seed_escalation_stall(held: &Held, backend: &ResourceBackend, cursor: bool, operator: bool) -> String {
    let bosun = held.crew_session("bosun", "bosun-session");
    held.assign(backend, "work", "bosun").await;
    let launch = held.launch.clone();
    held.stall(backend, |stall| {
        // Mixed inferred leaves can put a non-crew leaf first. The request still
        // identifies its stalled actor, so operator escalation must retain that maker.
        if operator {
            let mut inferred = stall.leaves[0].clone();
            inferred.field_path = ".phase".into();
            stall.leaves.insert(0, inferred);
        }
        stall.rung = StallRung::Bosun;
        stall.supervision_index = cursor.then_some(0);
        stall.supervisor = Some(StallSupervisor { convoy: launch.clone(), vessel: "work".into(), role: "bosun".into() });
        stall.maker = Some(flotilla_resources::LeafMaker::Supervisor { convoy: launch, vessel: "work".into(), role: "bosun".into() });
    })
    .await;
    bosun
}

// A stall's recorded escalation that no read reaches leaves the supervisor's Resume unlinked
// rather than refused.
#[tokio::test]
async fn resume_with_an_unreadable_recorded_escalation_goes_unlinked() {
    let (crew, backend, probe, held) =
        shaped_fixture(CrewWorkPhase::Working, |snapshot| escalation_shape(snapshot, false, false, true, "principal:operator")).await;
    probe.fail.store(false, Ordering::SeqCst);
    let bosun = seed_escalation_stall(&held, &backend, true, false).await;
    held.stall(&backend, |stall| {
        stall.supervision_message =
            Some(flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "escalation-never-sent"));
    })
    .await;
    crew.supervise(CrewSupervisionRequest {
        namespace: "flotilla",
        convoy_name: &held.launch,
        vessel: "work",
        role: "coder",
        operation: flotilla_protocol::CrewSupervisionAction::Resume,
        message: "carry on",
        actor_crew_id: Some(&bosun),
        principal: None,
    })
    .await
    .expect("the ruling is admitted without its link");
    let records = held.messages().views().await.expect("messages");
    assert_ruling_unlinked(&records.into_iter().map(|record| record.object).collect::<Vec<_>>(), &held);
}

// Glue: resolution errors retain their command-error channel rather than claiming
// a receiver is absent. The shared resolver validates this address before admission.
#[tokio::test]
async fn escalation_resolution_error_is_not_a_receiver_refusal() {
    let (crew, backend, _, held) =
        shaped_fixture(CrewWorkPhase::Working, |snapshot| escalation_shape(snapshot, false, false, true, "invalid-address")).await;
    seed_escalation_stall(&held, &backend, true, false).await;
    let before = held.convoy(&backend).await;
    let error = crew
        .leaf_subscriptions
        .escalate_stall(
            "flotilla",
            &before,
            ("principal:operator", &crate::leaf_engine::Party::Person("operator".into())),
            "handle this",
            "work",
            "coder",
        )
        .await
        .err()
        .expect("lookup error must not become a typed refusal");
    assert!(error.contains("invalid"), "{error}");
    assert_eq!(held.convoy(&backend).await.status, before.status);
    assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.is_empty());
}

// A successful command names the durable recipient and transfers the owner/cursor.
fn assert_escalation_transferred(
    result: &flotilla_protocol::CommandValue,
    after: &Option<flotilla_resources::StalledCondition>,
    records: &[flotilla_resources::ResourceObject<Message>],
    receiver: &str,
    operator: bool,
    owner: &str,
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
        assert_eq!(stall.supervisor.as_ref().expect("owner").convoy, owner);
    }
    assert_eq!(stall.supervision_index, Some(1));
    assert_eq!(records.len(), 2);
    let escalation = records.iter().find(|record| record.spec.receiver == receiver).expect("next receiver Message");
    assert_eq!(escalation.spec.relation, MessageRelation::Supervisor);
    assert_eq!(escalation.spec.expectation, if operator { MessageExpectation::None } else { MessageExpectation::Reply });
}

// A legacy cursor (one an import carried) names an escalation that was never sent: the
// supervisor's ruling is admitted without a link rather than refused.
fn assert_ruling_unlinked(records: &[flotilla_resources::ResourceObject<Message>], held: &Held) {
    let ruling = records.iter().find(|record| record.spec.receiver == held.receiver("work", "coder")).expect("the ruling to the coder");
    assert_eq!(ruling.spec.in_reply_to, None);
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

// Behaviour (#3587 layer 7): a send's awaiting-reply record is a stall when it supervises a
// stalled crew, else the kind its expectation names; a send expecting nothing records nothing.
#[test]
fn awaiting_records_take_the_stall_or_the_expectations_kind() {
    use flotilla_messaging::IntentExpectation;
    let outcome =
        IntentExpectation::Outcome { condition: "project:p/convoy:c/role:coder .phase == Done".parse().expect("a canonical condition") };
    assert_eq!(super::super::awaiting_kind(&IntentExpectation::Nothing, Some("p/c/work/coder")), None);
    assert_eq!(super::super::awaiting_kind(&IntentExpectation::Reply, Some("p/c/work/coder")), Some("stall"));
    assert_eq!(super::super::awaiting_kind(&IntentExpectation::Reply, None), Some("reply"));
    assert_eq!(super::super::awaiting_kind(&outcome, None), Some("outcome"));
}

// Resume names the environment's recorded loss even before the vessel status
// catches up, including a session previously misclassified as failed.
#[hegel::test]
fn lost_environment_resume_reports_cause(tc: hegel::TestCase) {
    use flotilla_provisioning_testkit::Ending;
    use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec};
    let failed_session = tc.draw(hegel::generators::booleans());
    let provisioning_failed = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (crew, _backend, probe, held) = fixture(CrewWorkPhase::Working).await;
        let environments = &held.engine.engine.environments;
        let spec = EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
            ..Default::default()
        };
        environments.request("resume-env", &spec, None);
        // The home's probe ends the launch with what it saw: failed, or lost to an OOM kill.
        if provisioning_failed {
            environments.finish("resume-env", Ending::Failed("cgroup_oom"));
        } else {
            environments.lose("resume-env", "cgroup_oom");
        }
        if failed_session {
            held.engine.engine.terminals.failed(&held.session, "cgroup_oom");
        } else {
            held.engine.engine.terminals.lost(&held.session, "cgroup_oom");
        }
        let error = crew.resume("flotilla", &held.launch, "resume", Some("work"), Some("coder")).await.expect_err("lost backing");
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
    let (crew, _backend, _probe, held) = fixture(CrewWorkPhase::Working).await;
    held.engine.engine.terminals.failed(&held.session, "agent launch rejected");
    let error = crew.resume("flotilla", &held.launch, "resume", Some("work"), Some("coder")).await.expect_err("failed session");
    assert!(error.contains("agent launch rejected"), "{error}");
    assert!(!error.contains("failed provisioning"), "{error}");
}
