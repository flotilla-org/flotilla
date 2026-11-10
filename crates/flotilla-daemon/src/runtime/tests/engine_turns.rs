//! Crew turns on the engine's delivery path (#3108). A turn typed without a busy signal is held,
//! never retyped, until the crew's own record confirms the agent took it up: its AgentSession began
//! working after the submission, or this daemon saw a prompt submitted or a tool called after it,
//! or, for a notify-only Codex crew, the turn's end. A session's pending messages go in one turn;
//! an operator's turn supersedes held system turns; cleat's refusal leaves the turn queued; an
//! abandoned composer draft is set aside in memory and typed back after the turn; and delivery
//! passes run on events, never on a timer.

use flotilla_engine_messages::MessagePrimitives;
use flotilla_resources::{MessagePhase, MessageRelation, MessageSpec};
use flotilla_schema::Timestamp;
use flotilla_state_machines::machines::agent_session::Activity;
use flotilla_store::load_metrics::ControllerLoadMetrics;

use super::*;
use crate::runtime::primitives::MessageDelivery;
use crate::testkits::engine::engine;

/// A running crew session that holds role `coder` of convoy `turns`, typing through `pool`, with
/// the engine's Message path open, as the runtime always opens it, and its fleet designated.
struct EngineCrew {
    _temp: TempDir,
    daemon: Arc<InProcessDaemon>,
    state: Arc<ControllerRuntimeState>,
    primitives: Arc<MessagePrimitives>,
    /// The launch the crew's session runs as: its crew id.
    launch: String,
}

async fn engine_crew(adapter: &str, pool: Arc<dyn TerminalPool>) -> EngineCrew {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"engine-turn-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let (daemon, _) = crew_daemon(config.clone()).await;
    let backend = daemon.resource_backend();
    let mut registry = probe_local_provider_registry(&daemon, &config).await.expect("registry");
    Arc::get_mut(&mut registry).expect("exclusive registry").terminal_pools.insert(
        "turn-probe",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "turn-probe"),
        pool,
    );
    let profile = build_local_profile(&daemon, &registry).await.expect("profile");
    ensure_host_direct_environment_exists(engine_environments(&daemon).reads.clone(), &profile).await.expect("environment");
    backend
        .using::<Convoy>(NAMESPACE)
        .create(&empty_meta("turns"), &ConvoySpec::builder().workflow_ref("test".into()).project_ref(NAMESPACE.into()).build())
        .await
        .expect("convoy");
    // The crew's vessel holds its session, which runs its agent under pool session `agent`.
    let vessel = engine(&daemon).vessels.request("turns", "work", &profile.host_id);
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
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        registry,
        None,
        profile.host_id.clone(),
        profile.host_direct_environment_name(),
    ));
    EngineCrew { _temp: temp, daemon, state, primitives, launch }
}

impl EngineCrew {
    fn delivery(&self) -> MessageDelivery {
        MessageDelivery::new(Arc::clone(&self.state), Arc::clone(&self.primitives), NAMESPACE.into(), RuntimeHealth::default())
    }

    async fn send(&self, name: &str, sender: &str, relation: MessageRelation, body: &str) {
        let intent = MessageSpec::builder()
            .sender(sender.into())
            .receiver(format!("{NAMESPACE}/turns/work/coder"))
            .relation(relation)
            .body(body.into())
            .build();
        self.daemon.publish_message(NAMESPACE, name, &intent).await.expect("send the turn");
    }

    async fn phase(&self, name: &str) -> Option<MessagePhase> {
        let message = self.primitives.view(NAMESPACE, name).await.expect("message view")?;
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
        let view = crew.primitives.view(NAMESPACE, name).await.expect("message view").expect("admitted");
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
