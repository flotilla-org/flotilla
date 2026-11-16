use super::*;
use crate::crew_actions::{CrewActionPort, CrewActions};
use flotilla_daemon_api::action_events::ActionEvents;
use flotilla_daemon_api::event_sink::EventSink;
use flotilla_protocol::{Command, CommandAction, CommandCaller, CommandValue, CrewCommandContext, DaemonEvent, NodeId, PrincipalRef};

// Subject reference resolution is outside these lifecycle scenarios.
struct UnusedSubjects;
#[async_trait]
impl CrewActionPort for UnusedSubjects {
    async fn link_convoy_subject(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
        panic!("lifecycle handlers must not resolve subject references")
    }
    async fn unlink_convoy_subject(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
        panic!("lifecycle handlers must not resolve subject references")
    }
}

/// The coder's context in the fixture convoy's vessel.
fn context(held: &Held) -> CrewCommandContext {
    CrewCommandContext::builder().convoy(held.launch.clone()).vessel_ref(held.vessel.clone()).role("coder".into()).build()
}
fn result(events: &[DaemonEvent], id: u64, node: &NodeId) -> CommandValue {
    let own: Vec<_> = events.iter().filter(|e| matches!(e, DaemonEvent::CommandStarted { command_id, .. } | DaemonEvent::CommandFinished { command_id, .. } if *command_id == id)).collect();
    match own.as_slice() {
        [DaemonEvent::CommandStarted { node_id: started, repo_identity: before, .. }, DaemonEvent::CommandFinished { node_id: finished, repo_identity: after, result, .. }] =>
        {
            assert_eq!(started, node);
            assert_eq!(finished, node);
            assert_eq!(before, after);
            result.clone()
        }
        other => panic!("expected ordered started/finished events: {other:?}"),
    }
}

// #2975: only an operator caller with force may fail a crew. Refusals preserve
// state and propagate through CommandFinished; successful failure is persisted.
#[tokio::test]
async fn direct_crew_contract_caller_refusal_and_lifecycle_result() {
    let (crew, backend, _, held) = fixture(CrewWorkPhase::Working).await;
    let recording = Arc::new(RecordingEventSink::default());
    let sink: Arc<dyn EventSink> = recording.clone();
    let clock: Arc<dyn flotilla_resources::Clock> = Arc::new(SystemClock);
    let node = NodeId::new("handler");
    let actions = CrewActions {
        port: &UnusedSubjects,
        crew: &crew,
        resource_backend: &backend,
        clock: &clock,
        events: ActionEvents { sink: &sink, node_id: &node },
    };
    let command = Command::builder()
        .action(CommandAction::CrewFail { context: context(&held), message: "operator failure".into(), force: true })
        .build();
    let operator =
        CommandCaller { principal_ref: PrincipalRef { namespace: "flotilla".into(), name: "operator".into() }, process: None, crew: None };
    let crew_caller = CommandCaller {
        principal_ref: operator.principal_ref.clone(),
        process: None,
        crew: Some(
            flotilla_protocol::CallerCrew::builder()
                .namespace("flotilla".into())
                .convoy(held.launch.clone())
                .vessel("work".into())
                .role("coder".into())
                .crew_id(held.session.clone())
                .build(),
        ),
    };
    for (id, caller) in [(1, None), (2, Some(crew_caller))] {
        assert_eq!(actions.execute_action_crew_fail(id, &command, &caller).await.unwrap(), id);
        assert!(
            matches!(result(&recording.events(), id, &node), CommandValue::Error { message } if message.contains("requires an operator principal"))
        );
        let convoy = held.convoy(&backend).await;
        assert_eq!(convoy.status.unwrap().crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    }
    actions.execute_action_crew_fail(3, &command, &Some(operator)).await.unwrap();
    assert_eq!(result(&recording.events(), 3, &node), CommandValue::Ok);
    let convoy = held.convoy(&backend).await;
    assert_eq!(convoy.status.unwrap().crew_work["work"]["coder"].phase, CrewWorkPhase::Failed);
}

// #2975: real lifecycle refusals and routing errors propagate unchanged, while
// a successful stall stores its declared reason and preserves command ordering.
#[tokio::test]
async fn direct_crew_contract_stall_errors_and_routing_refusal() {
    let (crew, backend, _, held) = fixture(CrewWorkPhase::Working).await;
    let recording = Arc::new(RecordingEventSink::default());
    let sink: Arc<dyn EventSink> = recording.clone();
    let clock: Arc<dyn flotilla_resources::Clock> = Arc::new(SystemClock);
    let node = NodeId::new("handler");
    let actions = CrewActions {
        port: &UnusedSubjects,
        crew: &crew,
        resource_backend: &backend,
        clock: &clock,
        events: ActionEvents { sink: &sink, node_id: &node },
    };
    for (id, context, message, expected) in [
        (1, context(&held), " ", Some("crew stall requires a non-empty message")),
        (
            2,
            CrewCommandContext::builder()
                .namespace("elsewhere".into())
                .convoy(held.launch.clone())
                .vessel_ref(held.vessel.clone())
                .role("coder".into())
                .build(),
            "blocked",
            Some("crew namespace `elsewhere` is not served by this daemon"),
        ),
        (3, context(&held), "blocked", None),
    ] {
        let command = Command::builder()
            .action(CommandAction::CrewStall {
                context,
                reason: flotilla_protocol::StallReason::Infra,
                proposed_disposition: None,
                message: message.into(),
            })
            .build();
        assert_eq!(actions.execute_action_crew_stall(id, &command).await.unwrap(), id);
        match expected {
            Some(expected) => assert_eq!(result(&recording.events(), id, &node), CommandValue::Error { message: expected.into() }),
            None => assert_eq!(result(&recording.events(), id, &node), CommandValue::Ok),
        }
        // The stall's supervision is sent with its awaiting-reply record: the coder reads
        // Stalled once it is.
        crew.reconcile_crew_stalls_once("flotilla").await.expect("stall pass");
        let status = held.convoy(&backend).await.status.unwrap();
        assert_eq!(
            status.crew_work["work"]["coder"].phase,
            if expected.is_some() { CrewWorkPhase::Working } else { CrewWorkPhase::Stalled }
        );
        // With no supervision declared the stall goes up the default ladder, whose missing
        // governor the evidence names: why the stall reached the operator.
        let declared = status.stalled.as_ref().map(|stalled| (stalled.evidence.as_str(), stalled.reason, stalled.rung));
        let evidence = "blocked; no live governor for project flotilla (no matching convoy in replica view)";
        assert_eq!(
            declared,
            expected.is_none().then_some((evidence, Some(flotilla_protocol::StallReason::Infra), flotilla_protocol::StallRung::Operator))
        );
    }
}

// Glue: the supervision handler must expose the typed missing-receiver refusal
// in CommandFinished and must not record a successful lifecycle mutation.
#[tokio::test]
async fn escalation_refusal_reaches_command_finished() {
    let (crew, backend, _, held) = fixture(CrewWorkPhase::Working).await;
    let status = held
        .stall(&backend, |stalled| {
            stalled.rung = flotilla_protocol::StallRung::Governor;
            stalled.supervision_index = Some(1);
            stalled.supervisor =
                Some(flotilla_resources::StallSupervisor { convoy: "governor".into(), vessel: "watch".into(), role: "governor".into() });
        })
        .await
        .status;
    let recording = Arc::new(RecordingEventSink::default());
    let sink: Arc<dyn EventSink> = recording.clone();
    let clock: Arc<dyn flotilla_resources::Clock> = Arc::new(SystemClock);
    let node = NodeId::new("handler");
    let actions = CrewActions {
        port: &UnusedSubjects,
        crew: &crew,
        resource_backend: &backend,
        clock: &clock,
        events: ActionEvents { sink: &sink, node_id: &node },
    };
    let command = Command::builder()
        .action(CommandAction::CrewSupervise {
            namespace: Some("flotilla".into()),
            convoy: held.launch.clone(),
            vessel: "work".into(),
            role: "coder".into(),
            operation: flotilla_protocol::CrewSupervisionAction::Escalate,
            message: "raise this".into(),
            actor_crew_id: None,
        })
        .build();
    actions
        .execute_action_crew_supervise(1, &command, &None, &Some(PrincipalRef::implicit_for_namespace("flotilla")))
        .await
        .expect("command dispatch");
    assert!(matches!(
        result(&recording.events(), 1, &node),
        CommandValue::CrewEscalationRefused { rung: flotilla_protocol::StallRung::Operator, .. }
    ));
    assert_eq!(held.convoy(&backend).await.status, status);
}
