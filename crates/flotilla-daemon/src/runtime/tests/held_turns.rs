//! A turn typed without a busy signal on the engine's delivery path (#3108) is held, never
//! retyped, until the crew's own record confirms the agent took it up: its AgentSession began
//! working after the submission, or this daemon saw a prompt submitted or a tool called after it,
//! or, for a notify-only Codex crew, the turn's end.

use flotilla_resources::{MessageRelation, MessageSpec, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
use flotilla_schema::Timestamp;
use flotilla_state_machines::machines::agent_session::Activity;

use super::*;
use crate::runtime::primitives::MessageDelivery;

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
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"held-turn-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let (daemon, _) = crew_daemon(config.clone()).await;
    let backend = daemon.resource_backend();
    // The PTY accepts the text, but its reply is lost: the turn is typed without confirmation.
    let mut probe = DeliveryProbePool::new(vec![ScreenActivity::Stable]);
    probe.submission_error = true;
    probe.screens = vec!["› Ask Codex to do anything".into()];
    let pool = Arc::new(probe);
    let mut registry = probe_local_provider_registry(&daemon, &config).await.expect("registry");
    Arc::get_mut(&mut registry).expect("exclusive registry").terminal_pools.insert(
        "held-probe",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "held-probe"),
        pool.clone(),
    );
    let profile = build_local_profile(&daemon, &registry).await.expect("profile");
    ensure_host_direct_environment_exists(engine_environments(&daemon).reads.clone(), &profile).await.expect("environment");
    backend
        .using::<Convoy>(NAMESPACE)
        .create(&empty_meta("held"), &ConvoySpec::builder().workflow_ref("test".into()).project_ref(NAMESPACE.into()).build())
        .await
        .expect("convoy");
    create_credential_test_session(&backend, "agent", "held", "held-work", &profile.host_direct_environment_name()).await;
    let sessions = backend.using::<TerminalSession>(NAMESPACE);
    let terminal = sessions.get("agent").await.expect("session");
    let mut spec = terminal.spec.clone();
    spec.pool = "held-probe".into();
    let TerminalSessionSource::Agent { selector, .. } = &mut spec.source else { unreachable!() };
    selector.adapter = Some(adapter.into());
    let mut meta = InputMeta::from(&terminal.metadata);
    meta.labels.extend([(CONVOY_LABEL.into(), "held".into()), (VESSEL_LABEL.into(), "work".into()), (ROLE_LABEL.into(), "coder".into())]);
    let terminal = sessions.update(&meta, &terminal.metadata.resource_version, &spec).await.expect("crew session");
    let mut status = terminal.status.clone().expect("status");
    status.session_id = Some("agent".into());
    status.crew = Some(flotilla_resources::CrewSessionStatus {
        id: "held-crew".into(),
        adapter: adapter.into(),
        model: None,
        stance: "trusted-implicit".into(),
    });
    status.started_at = Some(Utc::now() - chrono::Duration::minutes(20));
    sessions.update_status("agent", &terminal.metadata.resource_version, &status).await.expect("running session");
    // The engine's Message path, as the runtime always opens it, with its fleet designated.
    designate_fleet(&daemon).await;
    super::super::open_message_primitives(&daemon).await.expect("open the Message path");
    let primitives = daemon.message_primitives().expect("Message path");
    let agents = Arc::clone(primitives.sessions());
    let at = |time: chrono::DateTime<Utc>| Timestamp::from_datetime(&time);
    // Before the turn, the agent is Idle on its screen, or already Working.
    match evidence {
        Evidence::PromptSubmit => agents.observe("agent", Activity::Working, "UserPromptSubmit".into(), at(Utc::now())),
        Evidence::Working | Evidence::CodexIdle => agents.observe_screen("agent", Activity::Idle, at(Utc::now())),
    }
    .expect("activity before the turn");
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        registry,
        None,
        profile.host_id.clone(),
        profile.host_direct_environment_name(),
    ));
    let mut delivery = MessageDelivery::new(Arc::clone(&state), Arc::clone(&primitives), NAMESPACE.into(), RuntimeHealth::default());
    let intent = MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver(format!("{NAMESPACE}/held/work/coder"))
        .relation(MessageRelation::System)
        .body("wake".into())
        .build();
    daemon.publish_message(NAMESPACE, "held-turn", &intent).await.expect("send the turn");
    let delivered = || async {
        primitives
            .view(NAMESPACE, "held-turn")
            .await
            .expect("message view")
            .is_some_and(|message| message.object.status.as_ref().is_some_and(|status| status.phase.has_delivery_evidence()))
    };
    // Passes type the turn once, then hold it unconfirmed.
    for _ in 0..20 {
        delivery.pass().await.expect("delivery pass");
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "a held turn is typed once");
    assert!(!delivered().await, "nothing yet confirms the held turn");
    let after = Utc::now() + chrono::Duration::milliseconds(1);
    match evidence {
        Evidence::Working => {
            agents.observe_screen("agent", Activity::Working, at(after)).expect("working");
        }
        Evidence::PromptSubmit => daemon.crew_progress().prompt_submit("agent", after),
        Evidence::CodexIdle => {
            // The notify hook's Idle matches the screen's and is refused; this daemon saw the hook.
            assert!(!agents.observe("agent", Activity::Idle, "agent-turn-complete".into(), at(after)).expect("idle hook"));
            daemon.crew_progress().hook("agent", after);
        }
    }
    delivery.pass().await.expect("delivery pass");
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "a confirmed turn is never retyped");
    delivered().await
}
