//! Crew turns on the engine's delivery path (#3108). A turn typed without a busy signal is held,
//! never retyped, until the crew's own record confirms the agent took it up: its AgentSession began
//! working after the submission, or this daemon saw a prompt submitted or a tool called after it,
//! or, for a notify-only Codex crew, the turn's end. A session's pending messages go in one turn;
//! an operator's turn supersedes held system turns; cleat's refusal leaves the turn queued; an
//! abandoned composer draft is set aside in memory and typed back after the turn; and delivery
//! passes run on events, never on a timer.

use flotilla_engine_messages::MessagePrimitives;
use flotilla_resources::{MessagePhase, MessageRelation};
use flotilla_schema::Timestamp;
use flotilla_state_machines::machines::agent_session::Activity;
use flotilla_store::load_metrics::ControllerLoadMetrics;

use super::*;
use crate::runtime::primitives::MessageDelivery;
use crate::testkits::engine::{admit_convoy, engine};

/// A running crew session that holds role `coder` of convoy `turns`, typing through `pool`, with
/// the engine's Message path open, as the runtime always opens it, and its fleet designated.
struct EngineCrew {
    _temp: TempDir,
    daemon: Arc<InProcessDaemon>,
    state: Arc<ControllerRuntimeState>,
    primitives: Arc<MessagePrimitives>,
    /// The launch the crew's session runs as: its crew id.
    launch: String,
    /// The convoy's launch.
    convoy: String,
    /// What a restarted daemon's runtime state is made from.
    config: Arc<ConfigStore>,
    registry: Arc<ProviderRegistry>,
    profile: LocalProvisioningProfile,
}

async fn engine_crew(adapter: &str, pool: Arc<dyn TerminalPool>) -> EngineCrew {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"engine-turn-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let (daemon, _) = crew_daemon(config.clone()).await;
    let mut registry = probe_local_provider_registry(&daemon, &config).await.expect("registry");
    Arc::get_mut(&mut registry).expect("exclusive registry").terminal_pools.insert(
        "turn-probe",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "turn-probe"),
        pool,
    );
    let profile = build_local_profile(&daemon, &registry).await.expect("profile");
    ensure_host_direct_environment_exists(engine_environments(&daemon).reads.clone(), &profile).await.expect("environment");
    let convoy = admit_convoy(
        &daemon,
        flotilla_engine_convoys_testkit::convoy(
            NAMESPACE,
            "turns",
            ConvoySpec::builder().workflow_ref("test".into()).project_ref(NAMESPACE.into()).build(),
            flotilla_engine_convoys_testkit::single_agent("coder"),
        ),
    )
    .await;
    // The crew's vessel holds its session, which runs its agent under pool session `agent`.
    let vessel = engine(&daemon).vessels.request(&convoy, "work", &profile.host_id);
    let terminals = &engine(&daemon).terminals;
    let launch = terminals.request(&flotilla_resources::terminal_records::TerminalLaunchRequest {
        session: "agent".into(),
        environment: profile.host_direct_environment_name(),
        vessel: Some(vessel),
        role: "coder".into(),
        pool: "turn-probe".into(),
        cwd: "/workspace".into(),
        env: Default::default(),
        source: flotilla_resources::terminal_records::TerminalLaunchSource::Agent {
            capability: "code".into(),
            adapter: Some(adapter.into()),
            model: None,
            brief: flotilla_resources::TerminalBrief {
                artifact_digest: None,
                path: ".flotilla/briefs/coder.md".into(),
                content: "Work on the issue".into(),
                copies: Vec::new(),
            },
        },
    });
    let agent =
        flotilla_resources::terminal_records::AgentLaunch { adapter: adapter.into(), model: None, stance: "trusted-implicit".into() };
    terminals.ready(
        &launch,
        "agent",
        &flotilla_resources::terminal_records::TerminalLaunchFacts { agent: Some(agent), ..Default::default() },
    );
    designate_fleet(&daemon).await;
    super::super::open_message_primitives(&daemon).await.expect("open the Message path");
    let primitives = daemon.message_primitives().expect("Message path");
    let state = runtime_state(&daemon, &config, &registry, &profile);
    EngineCrew { _temp: temp, daemon, state, primitives, launch, convoy, config, registry, profile }
}

fn runtime_state(
    daemon: &Arc<InProcessDaemon>,
    config: &Arc<ConfigStore>,
    registry: &Arc<ProviderRegistry>,
    profile: &LocalProvisioningProfile,
) -> Arc<ControllerRuntimeState> {
    Arc::new(ControllerRuntimeState::new(
        Arc::clone(daemon),
        Arc::clone(config),
        Arc::clone(registry),
        None,
        profile.host_id.clone(),
        profile.host_direct_environment_name(),
    ))
}

impl EngineCrew {
    /// Delivery as a daemon started over the same store has it: its runtime state, with no turn
    /// in flight or held, is new, and the pool remembers the turns it accepted.
    fn restarted_delivery(&self) -> MessageDelivery {
        let state = runtime_state(&self.daemon, &self.config, &self.registry, &self.profile);
        MessageDelivery::new(state, Arc::clone(&self.primitives), NAMESPACE.into(), RuntimeHealth::default())
    }

    fn delivery(&self) -> MessageDelivery {
        MessageDelivery::new(Arc::clone(&self.state), Arc::clone(&self.primitives), NAMESPACE.into(), RuntimeHealth::default())
    }

    /// The crew member turns are sent to.
    fn receiver(&self) -> String {
        flotilla_resources::crew_role_address(NAMESPACE, &self.convoy, "work", "coder")
    }

    /// Sends `body` to the crew from `sender`: a fleet role (`system:<role>`) or a person
    /// (`principal:<name>`), who sends as the role they hold in the convoy's project.
    async fn send(&self, name: &str, sender: &str, relation: MessageRelation, body: &str) {
        use flotilla_crew::leaf_engine::Party;
        let messages: &dyn flotilla_messaging::EngineMessages = self.primitives.as_ref();
        let party = match (sender.strip_prefix("system:"), sender.strip_prefix("principal:")) {
            (Some(role), _) => Party::fleet(role),
            (_, Some(principal)) => Party::Person(principal.to_owned()),
            _ => panic!("`{sender}` is no fleet role or person"),
        };
        let crew = Party::Seat { project: NAMESPACE.into(), convoy: self.convoy.clone(), role: "coder".into() };
        let sender = party.inbox(messages, NAMESPACE, NAMESPACE).await.expect("the sender's inbox");
        let inbox = crew.inbox(messages, NAMESPACE, NAMESPACE).await.expect("the crew's inbox");
        let mut intent = flotilla_messaging::Intent::new(sender, inbox, relation, body);
        intent.principal = party.principal();
        self.daemon.publish_message(NAMESPACE, name, &intent).await.expect("send the turn");
    }

    async fn phase(&self, name: &str) -> Option<MessagePhase> {
        let message = self.primitives.view_to(NAMESPACE, &self.receiver(), name).await.expect("message view")?;
        message.object.status.as_ref().map(|status| status.phase)
    }

    async fn delivered(&self, name: &str) -> bool {
        self.phase(name).await.is_some_and(|phase| phase.has_delivery_evidence())
    }

    /// The crew's session as its view reads here, with what this home observes of it.
    fn session_status(&self) -> flotilla_resources::TerminalSessionStatus {
        let reads = self.daemon.lifecycle_reads();
        let terminals = reads.terminals().expect("terminal sessions");
        let record = terminals.get(&self.launch, None).expect("read the launch").expect("the launch");
        record.read_view(NAMESPACE, None, terminals.observations().get(&self.launch).as_ref(), None).status.expect("status")
    }

    fn at(time: chrono::DateTime<Utc>) -> Timestamp {
        Timestamp::from_datetime(&time)
    }
}

/// A PTY that accepts the text but loses its reply: each turn is typed without confirmation.
fn lossy_pool() -> Arc<DeliveryProbePool> {
    let mut probe = DeliveryProbePool::new(vec![ScreenActivity::Stable]);
    probe.submission_error = true;
    probe.screens = vec!["› Ask Codex to do anything".into()];
    Arc::new(probe)
}

/// Runs `count` delivery passes half a virtual second apart.
async fn passes(delivery: &mut MessageDelivery, count: usize) {
    for _ in 0..count {
        delivery.pass().await.expect("delivery pass");
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
    }
}

/// What the scenario reports after the turn is held.
enum Evidence {
    /// The agent's AgentSession goes from Idle to Working.
    Working,
    /// The agent was already Working before the turn; this daemon sees its prompt submitted.
    PromptSubmit,
    /// A notify-only Codex crew reports the turn's end, after a screen read had already
    /// recorded it Idle.
    CodexIdle,
}

#[tokio::test(start_paused = true)]
async fn a_held_turn_is_confirmed_when_the_agent_starts_working() {
    assert!(held_turn_confirmed("claude-code", Evidence::Working).await);
}

#[tokio::test(start_paused = true)]
async fn a_held_turn_of_an_agent_already_working_is_confirmed_by_its_prompt_submit() {
    assert!(held_turn_confirmed("claude-code", Evidence::PromptSubmit).await);
}

#[tokio::test(start_paused = true)]
async fn a_held_codex_turn_is_confirmed_by_its_completion() {
    assert!(held_turn_confirmed("codex", Evidence::CodexIdle).await);
}

/// Holds one turn for crew `adapter`, reports `evidence`, and answers whether a delivery pass
/// then confirmed the turn. Before the evidence, passes leave it held and never retype it.
async fn held_turn_confirmed(adapter: &str, evidence: Evidence) -> bool {
    let pool = lossy_pool();
    let crew = engine_crew(adapter, pool.clone()).await;
    let agents = Arc::clone(crew.primitives.sessions());
    // Before the turn, the agent is Idle on its screen, or already Working.
    match evidence {
        Evidence::PromptSubmit => agents.observe(&crew.launch, Activity::Working, "UserPromptSubmit".into(), EngineCrew::at(Utc::now())),
        Evidence::Working | Evidence::CodexIdle => agents.observe_screen(&crew.launch, Activity::Idle, EngineCrew::at(Utc::now())),
    }
    .expect("activity before the turn");
    let mut delivery = crew.delivery();
    crew.send("held-turn", "system:turn-rules", MessageRelation::System, "wake").await;
    // Passes type the turn once, then hold it unconfirmed.
    passes(&mut delivery, 20).await;
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "a held turn is typed once");
    assert!(!crew.delivered("held-turn").await, "nothing yet confirms the held turn");
    // The session's view shows the held turn, so the crew reads as delivery unconfirmed.
    let held = crew.session_status();
    assert!(held.delivery_unconfirmed_since.is_some(), "the view shows the held turn");
    assert_eq!(flotilla_resources::crew_attention(Some(&held), None), Some(flotilla_protocol::CrewAttention::DeliveryUnconfirmed));
    let after = Utc::now() + chrono::Duration::milliseconds(1);
    match evidence {
        Evidence::Working => {
            agents.observe_screen(&crew.launch, Activity::Working, EngineCrew::at(after)).expect("working");
        }
        Evidence::PromptSubmit => crew.daemon.crew_progress().prompt_submit("agent", after),
        Evidence::CodexIdle => {
            // The notify hook's Idle matches the screen's and is refused; this daemon saw the hook.
            assert!(!agents.observe(&crew.launch, Activity::Idle, "agent-turn-complete".into(), EngineCrew::at(after)).expect("idle hook"));
            crew.daemon.crew_progress().hook("agent", after);
        }
    }
    delivery.pass().await.expect("delivery pass");
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "a confirmed turn is never retyped");
    let confirmed = crew.delivered("held-turn").await;
    if confirmed {
        assert_eq!(crew.session_status().delivery_unconfirmed_since, None, "a confirmed turn is no longer shown held");
    }
    confirmed
}

// A session's pending messages are typed as one turn, each framed with its own header, in arrival
// order, and one confirmation records every receipt.
#[tokio::test(start_paused = true)]
async fn a_sessions_pending_messages_are_typed_as_one_turn() {
    let pool = lossy_pool();
    let crew = engine_crew("claude-code", pool.clone()).await;
    let agents = Arc::clone(crew.primitives.sessions());
    agents.observe_screen(&crew.launch, Activity::Idle, EngineCrew::at(Utc::now())).expect("idle before the turn");
    let mut delivery = crew.delivery();
    let sent = [
        ("first", "system:turn-rules", MessageRelation::System, "Complete this turn."),
        ("second", "principal:alice", MessageRelation::Supervisor, "Continue the work."),
        ("third", "system:nudge", MessageRelation::System, "Please continue."),
    ];
    for (name, sender, relation, body) in sent {
        crew.send(name, sender, relation, body).await;
    }
    passes(&mut delivery, 20).await;
    let inputs = pool.inputs.lock().expect("typed inputs").clone();
    assert_eq!(inputs.len(), 1, "one turn: {inputs:?}");
    // Each message framed as the engine holds it: a person sends as the role they hold (#3938).
    let mut expected = Vec::new();
    for (name, ..) in &sent {
        let view = crew.primitives.view_to(NAMESPACE, &crew.receiver(), name).await.expect("message view").expect("admitted");
        expected.push(flotilla_messaging::frame_message(&view.object.spec));
    }
    assert_eq!(inputs[0], expected.join("\n\n"));
    agents
        .observe_screen(&crew.launch, Activity::Working, EngineCrew::at(Utc::now() + chrono::Duration::milliseconds(1)))
        .expect("working");
    delivery.pass().await.expect("delivery pass");
    for (name, ..) in sent {
        assert!(crew.delivered(name).await, "{name} is received with its turn");
    }
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
}

/// The Message rows loaded and converted while `count` turns due to one session are typed as one.
async fn read_typing_turns(count: usize) -> (u64, u64) {
    let pool = lossy_pool();
    let crew = engine_crew("claude-code", pool.clone()).await;
    crew.primitives.sessions().observe_screen(&crew.launch, Activity::Idle, EngineCrew::at(Utc::now())).expect("idle before the turn");
    let mut delivery = crew.delivery();
    for index in 0..count {
        let (sender, relation) = match index % 2 {
            0 => ("system:turn-rules", MessageRelation::System),
            _ => ("principal:alice", MessageRelation::Supervisor),
        };
        crew.send(&format!("turn-{index}"), sender, relation, "Continue the work.").await;
    }
    let ((), reads) = flotilla_store::load_metrics::measure(passes(&mut delivery, 20)).await;
    assert_eq!(pool.inputs.lock().expect("typed inputs").len(), 1, "the due turns are typed as one turn");
    reads.get("Message").map_or((0, 0), |counts| (counts.decoded, counts.converted))
}

// Behaviour: typing a session's due turns loads each once and frames it as the engine holds it,
// converting none to the old shape.
#[tokio::test(start_paused = true)]
async fn typing_a_turn_loads_each_due_message_once_and_converts_none() {
    assert_eq!(read_typing_turns(5).await, (5, 0), "five due turns");
    assert_eq!(read_typing_turns(60).await, (60, 0), "sixty due turns");
}

// A confirmed turn whose receipt is refused stays held: later passes retry the receipt and never
// type the turn again while the hold lasts. The refusal here is the agent's record ending between
// the turn's confirmation and its receipt.
#[tokio::test(start_paused = true)]
async fn a_confirmed_turn_whose_receipt_is_refused_is_not_retyped() {
    let (dispatch, logs) = json_logs();
    let _logs = tracing::dispatcher::set_default(&dispatch);
    let pool = lossy_pool();
    let crew = engine_crew("claude-code", pool.clone()).await;
    let agents = Arc::clone(crew.primitives.sessions());
    agents.observe_screen(&crew.launch, Activity::Idle, EngineCrew::at(Utc::now())).expect("idle before the turn");
    let mut delivery = crew.delivery();
    crew.send("refused-receipt", "system:turn-rules", MessageRelation::System, "wake").await;
    passes(&mut delivery, 20).await;
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "a held turn is typed once");
    let after = Utc::now() + chrono::Duration::milliseconds(1);
    agents
        .end(&crew.launch, flotilla_state_machines::machines::agent_session::EndReason::Lost, EngineCrew::at(after))
        .expect("end the agent's record");
    // This daemon saw the agent take the turn up: the turn is confirmed.
    crew.daemon.crew_progress().prompt_submit("agent", after);
    passes(&mut delivery, 20).await;
    assert!(!crew.delivered("refused-receipt").await, "the receipt is refused");
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "a confirmed turn is never retyped");
    // The daemon's jsonl keeps the WARN's sentence, and names the message in its own field.
    let refusals = warnings_about(&logs, "refused-receipt");
    assert!(!refusals.is_empty(), "{}", logs.text());
    for line in refusals {
        assert_eq!(line["fields"]["message"], "a confirmed turn's receipt was refused; it is retried, never retyped", "{line}");
        assert!(line["fields"]["message_id"].as_str().is_some_and(|id| id.ends_with("refused-receipt")), "{line}");
    }
}

// A held turn whose message ends before its receipt (an operator withdraws it here; on b3 a newer
// turn superseded it) can never take the receipt: once the turn is confirmed it is released, not
// retried on every pass until its hold runs out.
#[tokio::test(start_paused = true)]
async fn a_confirmed_turn_whose_message_ended_is_not_retried() {
    let (dispatch, logs) = json_logs();
    let _logs = tracing::dispatcher::set_default(&dispatch);
    let pool = lossy_pool();
    let crew = engine_crew("claude-code", pool.clone()).await;
    crew.primitives.sessions().observe_screen(&crew.launch, Activity::Idle, EngineCrew::at(Utc::now())).expect("idle before the turn");
    let mut delivery = crew.delivery();
    crew.send("ended-turn", "system:turn-rules", MessageRelation::System, "wake").await;
    passes(&mut delivery, 20).await;
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "a held turn is typed once");
    crew.primitives.withdraw(NAMESPACE, &crew.receiver(), "ended-turn", &crew.convoy).await.expect("withdraw the held turn");
    // This daemon saw the agent take the turn up: the turn is confirmed.
    crew.daemon.crew_progress().prompt_submit("agent", Utc::now() + chrono::Duration::milliseconds(1));
    passes(&mut delivery, 20).await;
    assert_eq!(crew.phase("ended-turn").await, Some(MessagePhase::Expired), "withdrawn reads as expired");
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "a confirmed turn is never retyped");
    assert_eq!(warnings_about(&logs, "ended-turn"), Vec::<serde_json::Value>::new(), "an ended message's receipt is not retried");
}

/// A capture of this test's logs as the daemon writes its jsonl.
fn json_logs() -> (tracing::Dispatch, flotilla_log_testkit::Logs) {
    let logs = flotilla_log_testkit::Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt().json().with_ansi(false).with_writer(move || writer.clone()).finish();
    (flotilla_log_testkit::dispatch(subscriber), logs)
}

/// The captured WARN lines whose own fields name message record `record` (not merely their span).
fn warnings_about(logs: &flotilla_log_testkit::Logs, record: &str) -> Vec<serde_json::Value> {
    logs.lines()
        .iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a JSON log line"))
        .filter(|line| line["level"] == "WARN" && line["fields"].to_string().contains(record))
        .collect()
}

// #3455: an operator's turn to a session holding an unconfirmed system turn drops that turn as
// superseded, rather than typing it again once its hold runs out, and is typed on its own.
#[tokio::test(start_paused = true)]
async fn an_operator_turn_supersedes_the_sessions_held_system_turns() {
    let pool = lossy_pool();
    let crew = engine_crew("claude-code", pool.clone()).await;
    crew.primitives.sessions().observe_screen(&crew.launch, Activity::Idle, EngineCrew::at(Utc::now())).expect("idle before the turn");
    let mut delivery = crew.delivery();
    crew.send("rule-turn", "system:turn-rules", MessageRelation::System, "Complete this turn.").await;
    passes(&mut delivery, 20).await;
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
    assert!(!crew.delivered("rule-turn").await, "the system turn is held");
    crew.send("operator-turn", "principal:alice", MessageRelation::Supervisor, "Stop and rebase.").await;
    passes(&mut delivery, 20).await;
    assert_eq!(crew.phase("rule-turn").await, Some(MessagePhase::Superseded), "the held system turn is dropped");
    let inputs = pool.inputs.lock().expect("typed inputs").clone();
    assert_eq!(inputs.len(), 2, "{inputs:?}");
    assert!(inputs[1].contains("Stop and rebase.") && !inputs[1].contains("Complete this turn."), "{}", inputs[1]);
}

// A native-hook Codex crew: a session start, a child agent's stop and a stop before the turn is
// taken up do not confirm a held turn (#3028); its prompt submit does, and so does a tool call.
#[tokio::test(start_paused = true)]
async fn native_codex_hooks_confirm_a_held_turn_only_once_the_agent_takes_it_up() {
    for taken_up_by in [&["user-prompt-submit"][..], &["pre-tool-use", "post-tool-use"]] {
        let pool = lossy_pool();
        let crew = engine_crew("codex", pool.clone()).await;
        crew.primitives.sessions().observe_screen(&crew.launch, Activity::Idle, EngineCrew::at(Utc::now())).expect("idle before the turn");
        let mut delivery = crew.delivery();
        crew.send("held-turn", "system:turn-rules", MessageRelation::System, "wake").await;
        passes(&mut delivery, 20).await;
        assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
        let store = flotilla_agent::shared_in_memory_agent_state_store();
        for event in ["session-start", "subagent-stop", "stop"] {
            codex_hook(&crew.daemon, &store, event).await;
            delivery.pass().await.expect("delivery pass");
            assert!(!crew.delivered("held-turn").await, "{event} leaves the turn held");
        }
        for event in taken_up_by {
            codex_hook(&crew.daemon, &store, event).await;
        }
        delivery.pass().await.expect("delivery pass");
        assert!(crew.delivered("held-turn").await, "{taken_up_by:?} confirms the held turn");
        assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "a confirmed turn is never retyped");
        assert_eq!(pool.retries.load(Ordering::SeqCst), 0);
    }
}

/// Sends one native Codex hook for session `agent` through the daemon's request dispatch.
async fn codex_hook(daemon: &Arc<InProcessDaemon>, store: &flotilla_agent::SharedAgentStateStore, event: &str) {
    let (_, parser) = flotilla_agent::parser_for_harness("codex").expect("parser");
    let native = match event {
        "session-start" => "SessionStart",
        "subagent-stop" => "SubagentStop",
        "stop" => "Stop",
        "user-prompt-submit" => "UserPromptSubmit",
        "pre-tool-use" => "PreToolUse",
        "post-tool-use" => "PostToolUse",
        other => panic!("unexpected hook {other}"),
    };
    let mut payload = serde_json::json!({"session_id":"held-thread","turn_id":"turn-1","cwd":"/workspace",
        "hook_event_name":native,"transcript_path":"/workspace/transcript.jsonl","stop_hook_active":false,"last_assistant_message":null});
    if event == "subagent-stop" {
        payload["agent_id"] = "child".into();
        payload["agent_type"] = "worker".into();
    }
    let parsed = parser.parse_event(event, &serde_json::to_vec(&payload).expect("payload")).expect("hook");
    tokio::time::advance(Duration::from_millis(1)).await;
    let response = crate::server::dispatch_request_with_state(
        daemon,
        store,
        3028,
        flotilla_protocol::Request::AgentHook {
            event: flotilla_protocol::AgentHookEvent::builder()
                .attachable_id(flotilla_agent::allocate_attachable_id())
                .harness(flotilla_protocol::AgentHarness::Codex)
                .event_type(parsed.event_type.clone())
                .maybe_session_id(parsed.session_id.clone())
                .maybe_transcript_path(parsed.transcript_path.clone())
                .native_codex_hook(parsed.native_codex_hook)
                .maybe_hook_event(flotilla_agent::hooks::hook_event_name(&flotilla_protocol::AgentHarness::Codex, event).map(str::to_owned))
                .terminal(flotilla_protocol::AgentHookTerminalRef { namespace: NAMESPACE.into(), session_name: "agent".into() })
                .build(),
        },
    )
    .await;
    let flotilla_protocol::Message::Response { response, .. } = response else { panic!("hook response") };
    assert!(matches!(*response, flotilla_protocol::ResponseResult::Ok { .. }), "{event}: {response:?}");
}

// Cleat refuses the guarded write while the operator's controller is attached or typing: no input
// is sent and the turn stays queued. Once the controller detaches, a later pass types it once,
// with no relaunch or restart. A draft the person abandoned in the composer is held in memory,
// cleared under the guard, and typed back unsubmitted after the turn.
#[tokio::test(start_paused = true)]
async fn a_turn_cleat_refuses_for_controller_input_is_typed_once_the_controller_is_idle() {
    const DRAFT: &str = "--half a thought\nwith 'quotes' and \\";
    for (attached, abandoned) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut probe = DeliveryProbePool::new(vec![ScreenActivity::Stable]);
        probe.working_after_delivery = true;
        probe.recent_controller_input.store(true, Ordering::SeqCst);
        probe.controller_attached.store(attached, Ordering::SeqCst);
        if abandoned {
            *probe.draft.lock().expect("composer") = Some(DRAFT.into());
        }
        probe.screens = vec!["› Ask Codex to do anything".into()];
        let probe = Arc::new(probe);
        let runner = Arc::new(ControllerCleatRunner { probe: probe.clone(), recognizer_installed: AtomicBool::new(false) });
        let cleat = Arc::new(flotilla_terminal::cleat::CleatTerminalPool::new(runner, "cleat", &EnvironmentBag::new()));
        let crew = engine_crew("codex", cleat).await;
        // The TerminalSession reconciler's observation attaches the crew's recognizer.
        let runtime = TerminalControllerRuntime { state: Arc::clone(&crew.state) };
        refresh_message_test_attention(&runtime, "agent").await;
        let mut delivery = crew.delivery();
        crew.send("queued-turn", "system:turn-rules", MessageRelation::System, "recover this turn").await;
        passes(&mut delivery, 4).await;
        assert!(probe.inputs.lock().expect("typed inputs").is_empty(), "a refusal writes no input");
        assert!(probe.submission_attempts.load(Ordering::SeqCst) > 0, "cleat was asked");
        assert!(!crew.delivered("queued-turn").await);
        probe.recent_controller_input.store(false, Ordering::SeqCst);
        probe.controller_attached.store(false, Ordering::SeqCst);
        for _ in 0..40 {
            passes(&mut delivery, 1).await;
            if crew.delivered("queued-turn").await {
                break;
            }
        }
        assert!(crew.delivered("queued-turn").await, "attached={attached}: typed once the controller is idle");
        assert_eq!(probe.inputs.lock().expect("typed inputs").len(), 1);
        assert_eq!(probe.lifecycle_changes.load(Ordering::SeqCst), 0, "no child launch or termination");
        assert_eq!(probe.retries.load(Ordering::SeqCst), 0, "no harness restart");
        assert_eq!(probe.draft.lock().expect("composer").as_deref(), abandoned.then_some(DRAFT), "the draft is typed back");
        assert!(crew.state.composer_drafts.held("agent").is_empty(), "nothing left held in memory");
        let sessions = crew.daemon.resource_backend().using::<TerminalSession>(NAMESPACE);
        assert_eq!(sessions.get("agent").await.expect("session").status.expect("status").crew.expect("crew").id, crew.launch);
    }
}

// Delivery passes run on events, never on a timer: an idle adapter runs no pass however long it
// waits, a held turn waits for evidence rather than a tick, and an admission and a prompt submit
// each wake it.
#[tokio::test(start_paused = true)]
async fn message_delivery_runs_on_events_not_a_timer() {
    async fn wait_until(what: &str, mut done: impl AsyncFnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(1), async {
            // Paused time advances only once runnable tasks drain, so this also lets the
            // adapter's follow-up writes and wakes reach quiescence.
            while !done().await {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what} within one virtual second"));
    }
    async fn idle_passes(metrics: &ControllerLoadMetrics, wait: Duration) -> u64 {
        let before = metrics.snapshot().reconcile.count();
        tokio::time::advance(wait).await;
        tokio::time::timeout(Duration::from_secs(1), std::future::pending::<()>()).await.ok();
        metrics.snapshot().reconcile.count() - before
    }
    /// Lets the adapter's follow-up wakes run out: a short span passes with no pass.
    async fn settle(metrics: &ControllerLoadMetrics) {
        for _ in 0..10 {
            if idle_passes(metrics, Duration::from_millis(10)).await == 0 {
                return;
            }
        }
        panic!("delivery keeps running passes with nothing to wake it");
    }
    let pool = lossy_pool();
    let crew = engine_crew("claude-code", pool.clone()).await;
    crew.primitives.sessions().observe_screen(&crew.launch, Activity::Idle, EngineCrew::at(Utc::now())).expect("idle before the turn");
    let metrics = Arc::new(ControllerLoadMetrics::default());
    let task = tokio::spawn(crew.delivery().run(Arc::clone(&metrics)));
    wait_until("the first pass", async || metrics.snapshot().reconcile.count() >= 1).await;
    settle(&metrics).await;
    assert_eq!(idle_passes(&metrics, Duration::from_secs(60)).await, 0, "an idle adapter runs no pass");
    crew.send("held-turn", "system:turn-rules", MessageRelation::System, "wake").await;
    wait_until("the admission wakes a pass that types the turn", async || pool.deliveries.load(Ordering::SeqCst) == 1).await;
    settle(&metrics).await;
    let hold = flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR.to_std().expect("hold");
    assert_eq!(idle_passes(&metrics, hold / 2).await, 0, "a held turn waits for evidence, not a tick");
    crew.daemon.crew_progress().prompt_submit("agent", Utc::now());
    wait_until("the prompt submit wakes a pass that confirms the turn", async || crew.delivered("held-turn").await).await;
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
    assert!(!task.is_finished(), "delivery stays live");
    task.abort();
}

// A launch in an environment this daemon has not registered (as after a restart, before the
// environment's adoption) waits for it: its try is retried once the registration returns, rather
// than giving up and leaving an ended session beside the vessel's next launch.
#[tokio::test(start_paused = true)]
async fn a_launch_waits_for_its_environments_registration() {
    use flotilla_provisioning::engine::terminal_actuator::{LaunchFailure, TerminalProvider};

    let crew = engine_crew("codex", lossy_pool()).await;
    let runtime = TerminalControllerRuntime { state: Arc::clone(&crew.state) };
    let reads = crew.daemon.lifecycle_reads();
    let record = |launch: &str| reads.terminals().expect("terminals").get(launch, None).expect("read launch").expect("launch held");
    assert_eq!(runtime.launch_waits_for(&record(&crew.launch)).await, Ok(None), "the host's own environment is registered");

    let held = record(&crew.launch);
    let unadopted = engine(&crew.daemon).terminals.request(&flotilla_resources::terminal_records::TerminalLaunchRequest {
        session: "unadopted".into(),
        environment: "env-unadopted".into(),
        vessel: held.vessel.clone(),
        role: "coder".into(),
        pool: "turn-probe".into(),
        cwd: "/workspace".into(),
        env: Default::default(),
        source: flotilla_resources::terminal_records::TerminalLaunchSource::Tool { command: "sleep infinity".into() },
    });
    let waits = runtime.launch_waits_for(&record(&unadopted)).await.expect("readiness");
    assert_eq!(waits.as_deref(), Some("environment env-unadopted is not registered on this host yet"));
    // A launch that reaches its pool before the registration fails as waiting, not refused.
    let Err(failure) = runtime.launch(&record(&unadopted)).await else { panic!("no environment to launch in") };
    assert_eq!(failure, LaunchFailure::waits("environment env-unadopted is not registered on this host yet"));
}

// Behaviour (d8): a turn typed just before a restart, which its agent took up and finished before
// the restarted daemon looked, is received after the restart. The restarted daemon finds the turn
// typed and holds it; the Idle the agent's hooks recorded after the submission confirms it, though
// the agent is no longer working and this daemon saw nothing of the turn.
#[tokio::test(start_paused = true)]
async fn a_turn_its_agent_finished_before_a_restart_is_received_after_it() {
    let pool = Arc::new(KeyedPool { inner: FakeTerminalPool::new(), accepted: Default::default() });
    pool.inner
        .add_sessions(vec![ProviderTerminalSession::builder()
            .session_name("agent".into())
            .status(TerminalStatus::Running)
            .screen_activity(ScreenActivity::Stable)
            .build()])
        .await;
    let crew = engine_crew("claude-code", pool.clone()).await;
    let agents = Arc::clone(crew.primitives.sessions());
    agents.observe(&crew.launch, Activity::Idle, "Stop".into(), EngineCrew::at(Utc::now())).expect("idle before the turn");
    crew.send("restart-turn", "system:turn-rules", MessageRelation::System, "wake").await;
    // One pass types the turn; the daemon stops before any pass collects it.
    crew.delivery().pass().await.expect("delivery pass");
    tokio::time::timeout(Duration::from_secs(1), async {
        while pool.inner.delivered.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the turn is typed");
    // The agent takes the turn up and finishes it, as its hooks record.
    let typed = Utc::now();
    agents
        .observe(&crew.launch, Activity::Working, "UserPromptSubmit".into(), EngineCrew::at(typed + chrono::Duration::seconds(1)))
        .expect("working");
    agents.observe(&crew.launch, Activity::Idle, "Stop".into(), EngineCrew::at(typed + chrono::Duration::seconds(2))).expect("idle");

    let mut restarted = crew.restarted_delivery();
    passes(&mut restarted, 20).await;
    assert_eq!(pool.inner.delivered.lock().await.len(), 1, "the restarted daemon does not type the turn again");
    assert!(crew.delivered("restart-turn").await, "the agent's recorded Idle after the turn confirms it");
}

// Behaviour (d8): a graceful stop settles the turn being typed and records its receipt before
// delivery ends, rather than leaving the turn to a restarted daemon's hold.
#[tokio::test(start_paused = true)]
async fn a_graceful_stop_records_the_receipt_of_a_turn_in_flight() {
    let mut probe = DeliveryProbePool::new(vec![ScreenActivity::Stable]);
    probe.working_after_delivery = true;
    let pool = Arc::new(probe);
    // Codex's Working line is what the probe shows once the turn is typed.
    let crew = engine_crew("codex", pool.clone()).await;
    crew.primitives.sessions().observe_screen(&crew.launch, Activity::Idle, EngineCrew::at(Utc::now())).expect("idle before the turn");
    let task = tokio::spawn(crew.delivery().run(Arc::new(ControllerLoadMetrics::default())));
    crew.send("stop-turn", "system:turn-rules", MessageRelation::System, "wake").await;
    // Waits for the turn to be typed without letting virtual time run: its confirmation is still
    // under way when the stop is asked for.
    pool.submission_attempted.notified().await;
    assert!(!crew.delivered("stop-turn").await, "the turn is in flight");

    assert!(crew.state.delivery_stop.settle(Duration::from_secs(10)).await, "delivery answers the stop");
    assert!(crew.delivered("stop-turn").await, "the stop recorded the turn's receipt");
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(1), task).await.expect("delivery ends at the stop").expect("delivery task");
}
