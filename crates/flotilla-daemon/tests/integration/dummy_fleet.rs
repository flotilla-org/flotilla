//! The dummy fleet (#3206) drives convoys end to end through the real daemon
//! runtime: admission, placement, the environment, checkout, terminal-session,
//! message and convoy controllers, and change-request observation. Only the
//! providers at the seams are dummies.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_daemon::runtime::RuntimeOptions;
use flotilla_daemon_testkit::daemon::{TestDaemon, NAMESPACE};
use flotilla_dummy_fleet::{
    catalog::{self, CrewMix, DUMMY_FULFILMENT_KIND},
    DummyFleetSnapshot,
};
use flotilla_protocol::{
    commands::{CommandValue, CrewSupervisionAction},
    CommandAction,
};
use flotilla_resources::{
    Convoy, ConvoyPhase, ConvoyStatus, CrewWorkPhase, FleetDesignation, FleetDesignationSpec, Host, Resource, ResourceObject, StatusPatch,
    TerminalSession, WorkflowTemplate, FLEET_DESIGNATION_NAME,
};

/// Long runners' turns in the harness's `[dummy_fleet.agent]`.
const LONG_RUNNER_TURNS: u32 = 2;
const PRIMITIVES: &str = "[primitives]\n";

/// The convoy named `name`, once `done` holds for its status.
fn convoy<'a>(name: &'a str, done: impl Fn(&ConvoyStatus) -> bool + 'a) -> impl Fn(&ResourceObject<Convoy>) -> bool + 'a {
    move |convoy| convoy.metadata.name == name && convoy.status.as_ref().is_some_and(&done)
}

fn in_phase(name: &str, phase: ConvoyPhase) -> impl Fn(&ResourceObject<Convoy>) -> bool + '_ {
    convoy(name, move |status| status.phase == phase)
}

/// A crew stall a supervisor can act on: the crew's work and the convoy's
/// stall condition, both published by the crew and maintained by the leaf engine.
fn stalled(name: &str) -> impl Fn(&ResourceObject<Convoy>) -> bool + '_ {
    convoy(name, |status| {
        status.stalled.is_some()
            && status
                .crew_work
                .get("implement")
                .and_then(|roles| roles.get("coder"))
                .is_some_and(|work| work.phase == CrewWorkPhase::Stalled)
    })
}

/// Dispatch a convoy of `mix` onto the dummy kind and wait for it to land.
async fn deliver(fleet: &TestDaemon, mix: CrewMix) -> (ResourceObject<Convoy>, DummyFleetSnapshot) {
    let name = fleet.dispatch(mix, DUMMY_FULFILMENT_KIND).await;
    let landed = fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    (landed, fleet.counts())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lone_coder_keeps_its_change_request_promise_and_lands() {
    let fleet = TestDaemon::builder().start().await;
    let (convoy, snapshot) = deliver(&fleet, CrewMix::Coder).await;
    assert_eq!((snapshot.crews_completed, snapshot.change_requests_opened, snapshot.crews_failed), (1, 1, 0));
    let status = convoy.status.expect("status");
    let promises = &status.promises["implement"]["coder"];
    let promise = promises.iter().find(|p| p.kind == flotilla_resources::promises::PromiseKind::Pr).expect("pr promise");
    assert_eq!(promise.state, flotilla_resources::promises::PromiseState::Kept);
    assert_eq!(promise.id, "template/pr");
    assert_eq!(promise.source, flotilla_resources::promises::PromiseSource::Template);
    let done = &status.crew_work["implement"]["coder"];
    assert_eq!(done.completion_authority, flotilla_resources::WorkCompletionAuthority::Promises);
    assert!(done.finished_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reviewed_coder_lands_after_its_reviewer_answers() {
    let fleet = TestDaemon::builder().start().await;
    let (convoy, snapshot) = deliver(&fleet, CrewMix::CoderReviewer).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, 2, "the review request and its answer are delivered turns: {snapshot:?}");
    let status = convoy.status.unwrap();
    let pr = status.promises["implement"]["coder"].iter().find(|p| p.kind == flotilla_resources::promises::PromiseKind::Pr).unwrap();
    assert_eq!(pr.id, "template/pr");
    assert_eq!(pr.state, flotilla_resources::promises::PromiseState::Kept);
    assert!(status.crew_work.values().flat_map(|crew| crew.values()).all(|work| work.phase == CrewWorkPhase::Done));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_runner_exchanges_its_turns_before_landing() {
    let fleet = TestDaemon::builder().start().await;
    let (_, snapshot) = deliver(&fleet, CrewMix::LongRunner).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, u64::from(2 * LONG_RUNNER_TURNS), "every task and reply is a delivered turn: {snapshot:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environment_that_fails_to_realise_fails_its_convoy() {
    let fleet = TestDaemon::builder().environment("failure_rate = 1.0").start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    fleet.wait("Failed", in_phase(&name, ConvoyPhase::Failed)).await;
    assert_eq!(fleet.counts().sessions_launched, 0);
}

fn supervise(convoy: &str, operation: CrewSupervisionAction) -> CommandAction {
    CommandAction::CrewSupervise {
        namespace: Some(NAMESPACE.into()),
        convoy: convoy.into(),
        vessel: "implement".into(),
        role: "coder".into(),
        operation,
        message: "Supervisor decision.".into(),
        actor_crew_id: None,
    }
}

// A crew cannot fail its own work; it stalls proposing failure, and a
// supervisor's decision fails the convoy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_crew_proposes_failure_and_its_supervisor_fails_the_convoy() {
    let fleet = TestDaemon::builder().agent("fail_rate = 1.0").start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    // The crew publishes its stall before the script records its failure. Wait for
    // accounting before supervision can tear down the crew session.
    fleet.wait("failure accounting", |_: &ResourceObject<Convoy>| fleet.counts().crews_failed > 0).await;
    assert_eq!(fleet.counts().crews_failed, 1);
    fleet.wait("a supervisable stall", stalled(&name)).await;
    let failed = fleet.execute(supervise(&name, CrewSupervisionAction::Fail)).await;
    assert!(!matches!(failed, CommandValue::Error { .. }), "{failed:?}");
    let crew_failed = |status: &ConvoyStatus| status.crew_work["implement"]["coder"].phase == CrewWorkPhase::Failed;
    fleet.wait("coder Failed", convoy(&name, crew_failed)).await;
    // A supervisor's failure decision ends the convoy, not just the crew.
    let ended = fleet.wait("Failed", in_phase(&name, ConvoyPhase::Failed)).await;
    assert!(ended.status.as_ref().expect("status").stalled.is_none(), "terminal convoys have no stall");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_crew_resumes_on_supervision_and_lands() {
    let fleet = TestDaemon::builder().agent("stall_rate = 1.0").start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    // The crew publishes its stall before recording accounting; wait for both
    // before supervision resumes the script.
    fleet.wait("stall accounting", |_: &ResourceObject<Convoy>| fleet.counts().crews_stalled > 0).await;
    fleet.wait("a supervisable stall", stalled(&name)).await;
    assert_eq!(fleet.counts().crews_stalled, 1);
    let resumed = fleet.execute(supervise(&name, CrewSupervisionAction::Resume)).await;
    assert!(!matches!(resumed, CommandValue::Error { .. }), "{resumed:?}");
    fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
}

// #3439: force the handoff-after-Done ordering without racing the scripts.
// A zero-task long runner finishes first; its aide then hands back work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_long_runner_handles_a_late_handoff_and_lands() {
    late_handoff_scenario("").await;
}

// A late handoff reopens the recipient on the engine path too; the recipient
// delivers its convoy after handling that additional turn.
#[tokio::test(start_paused = true)]
async fn on_the_engine_a_finished_long_runner_handles_a_late_handoff() {
    late_handoff_scenario(PRIMITIVES).await;
}

async fn late_handoff_scenario(settings: &str) {
    let fleet = TestDaemon::builder().settings(settings).start().await;
    // Isolate the legacy same-vessel hand-back path. Normal peer tasks retain
    // their separate responsibilities on distinct vessels in the load catalog.
    let mut same_vessel = CrewMix::LongRunner.workflow(0);
    let mut aide = same_vessel.vessels.pop().unwrap();
    same_vessel.vessels[0].crew.append(&mut aide.crew);
    for crew in &mut same_vessel.vessels[0].crew {
        if let flotilla_resources::CrewSource::Agent { prompt: Some(prompt), .. } = &mut crew.source {
            *prompt = prompt.replace("assist:aide", "aide").replace("govern:governor", "governor");
        }
    }
    fleet.apply::<WorkflowTemplate>(CrewMix::LongRunner.workflow_name(), same_vessel).await;
    let name = fleet.dispatch(CrewMix::LongRunner, DUMMY_FULFILMENT_KIND).await;
    let ready = |status: &ConvoyStatus| {
        status.crew_work.get("govern").is_some_and(|crew| {
            crew.get("governor").is_some_and(|work| work.phase == CrewWorkPhase::Done)
                && crew.get("aide").is_some_and(|work| work.phase == CrewWorkPhase::Pending)
        })
    };
    fleet.wait("finished governor and available aide", convoy(&name, ready)).await;
    let session = fleet.wait("the governor's session", |session: &ResourceObject<TerminalSession>| session.spec.role == "governor").await;
    let flotilla_resources::TerminalSessionSource::Agent { context: session_context, .. } = session.spec.source else { panic!("agent") };
    let context = flotilla_protocol::query::CrewCommandContext {
        namespace: Some(NAMESPACE.into()),
        convoy: Some(name.clone()),
        vessel_ref: Some(session_context.vessel_ref),
        role: Some("aide".into()),
        ..Default::default()
    };
    let before = fleet.counts();
    let handed_off = fleet
        .execute(CommandAction::CrewHandoff {
            context: context.clone(),
            target: "governor".into(),
            message: "Done.".into(),
            carries: Vec::new(),
        })
        .await;
    assert_eq!(handed_off, CommandValue::Ok);
    // The artificial sender is HandedBack, a terminal phase excluded from
    // derived Done. Keep this legacy handoff fixture's explicit operator override.
    let completed = fleet
        .execute(CommandAction::CrewComplete { context, message: None, disposition: None, decision_ledger_ref: None, force: true })
        .await;
    assert_eq!(completed, CommandValue::Ok);
    let landed = fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    assert_eq!(fleet.counts().turns_received, before.turns_received + 1, "the reopened governor receives the handoff turn");
    assert_eq!(
        landed.status.expect("status").crew_work["govern"]["governor"].completion_authority,
        flotilla_resources::WorkCompletionAuthority::Promises
    );
    if !settings.is_empty() {
        assert_no_old_messages(&fleet).await;
    }
}

// The supervisor is eagerly launched with no claim, waits for a delivered
// review task, answers it across vessels, and reports so the convoy can land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_eager_supervisor_waits_for_work_and_lands_after_answering() {
    let fleet = TestDaemon::builder().start().await;
    let (_, snapshot) = deliver(&fleet, CrewMix::CoderSupervisor).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, 2, "the task and answer are delivered: {snapshot:?}");
    assert_eq!(snapshot.crews_stalled, 0, "{snapshot:?}");
}

// Behaviour (#3317): with `[primitives]` on, crew turns run end to end on the
// engine path, and the old store holds no Message at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_a_reviewed_coder_lands_after_its_reviewer_answers() {
    let fleet = TestDaemon::builder().settings(PRIMITIVES).start().await;
    let (_, snapshot) = deliver(&fleet, CrewMix::CoderReviewer).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, 2, "the review request and its answer are delivered turns: {snapshot:?}");
    assert_no_old_messages(&fleet).await;
}

// Behaviour (#3317): a long runner's turns all travel the engine path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_a_long_runner_exchanges_its_turns_before_landing() {
    let fleet = TestDaemon::builder().settings(PRIMITIVES).start().await;
    let (_, snapshot) = deliver(&fleet, CrewMix::LongRunner).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, u64::from(2 * LONG_RUNNER_TURNS), "every task and reply is a delivered turn: {snapshot:?}");
    assert_no_old_messages(&fleet).await;
}

/// On the engine path every Message is the engine's; the old store holds none.
async fn assert_no_old_messages(fleet: &TestDaemon) {
    let old = fleet.list::<flotilla_resources::Message>().await;
    assert!(old.is_empty(), "the old store holds Messages: {old:?}");
}

// Behaviour: with no FleetDesignation the controllers keep running and explain names the missing
// record; once it is recorded the waiting convoy delivers without a restart. On the engine the
// Host keeps only conditions that can block, so health's diagnostic naming it is not recorded there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_fleet_designation_waits_and_delivery_resumes_when_it_arrives() {
    let fleet = TestDaemon::builder().settings(PRIMITIVES).start().await;
    let removed = fleet
        .execute(CommandAction::ResourceDelete {
            namespace: NAMESPACE.into(),
            kind: "FleetDesignation".into(),
            name: FLEET_DESIGNATION_NAME.into(),
            replica_origin: None,
        })
        .await;
    assert!(!matches!(removed, CommandValue::Error { .. }), "{removed:?}");
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let explain = CommandAction::QueryExplainConvoy { namespace: Some(NAMESPACE.into()), name: name.clone() };
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let CommandValue::ConvoyExplanation(explained) = fleet.execute(explain.clone()).await else { panic!("an explanation") };
            if explained.warnings.iter().any(|warning| warning.contains("FleetDesignation `fleet` is missing")) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("explain names the missing FleetDesignation");
    let conditions = host_conditions(&fleet).await;
    assert!(conditions.iter().all(|c| c.condition_type != "Message/FleetDesignation"), "a diagnostic is not recorded: {conditions:?}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let conditions = host_conditions(&fleet).await;
    assert!(conditions.iter().all(|c| !c.condition_type.starts_with("Controller/")), "no controller exhausted its budget: {conditions:?}");
    let designation = FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None };
    fleet.apply::<FleetDesignation>(FLEET_DESIGNATION_NAME, designation).await;
    fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while host_conditions(&fleet).await.iter().any(|c| c.condition_type == "Message/FleetDesignation") {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("health clears the wait");
}

async fn host_conditions(fleet: &TestDaemon) -> Vec<flotilla_resources::HostCondition> {
    fleet.list::<Host>().await.into_iter().flat_map(|h| h.status.map(|s| s.conditions).unwrap_or_default()).collect()
}

// Behaviour (#3938): an operator sends as the operator role of the convoy's
// project. On the engine path a CLI message to a stalled crew and the
// operator's ruling each reach the crew's session, and the resumed convoy
// delivers. Each goes to its own stalled crew in its own fleet: whichever turn
// reaches a stalled crew first resumes it, and a later one may find its
// session already ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_an_operators_message_reaches_the_crew_from_the_projects_operator() {
    operator_turn_scenario(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_an_operators_ruling_reaches_the_crew_from_the_projects_operator() {
    operator_turn_scenario(false).await;
}

async fn operator_turn_scenario(noted: bool) {
    let fleet = TestDaemon::builder().agent("stall_rate = 1.0").settings(PRIMITIVES).start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    fleet.wait("a supervisable stall", stalled(&name)).await;
    let turns = fleet.counts().turns_received;
    let sent = if noted {
        fleet
            .execute(CommandAction::ConvoyResume {
                namespace: Some(NAMESPACE.into()),
                name: name.clone(),
                prompt: "Operator note: the API is stable now.".into(),
                vessel: None,
                role: None,
                reopen: false,
            })
            .await
    } else {
        fleet.execute(supervise(&name, CrewSupervisionAction::Resume)).await
    };
    assert!(!matches!(sent, CommandValue::Error { .. }), "{sent:?}");
    fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    assert_eq!(fleet.counts().turns_received, turns + 1, "the operator's turn reached the crew");
    assert_no_old_messages(&fleet).await;
}

// A standing Ensure binds an admitted convoy. Abandoning that convoy must let
// the same Ensure admit a fresh generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_a_standing_ensure_readmits_after_abandon() {
    use flotilla_resources::ConvoyEnsure;

    let clock = Arc::new(SkippingClock::default());
    let fleet = TestDaemon::builder().settings(PRIMITIVES).clock(clock.clone() as Arc<dyn flotilla_resources::Clock>).start().await;
    let mut standing = CrewMix::Coder.workflow(LONG_RUNNER_TURNS);
    standing.exit = None;
    fleet.apply::<WorkflowTemplate>("engine-standing-workflow", standing).await;
    let forge = catalog::forge(&catalog_forge_url(&fleet).await);
    let repository = catalog::repository(&forge).expect("repository");
    let mut project = fleet.get::<flotilla_resources::Project>("fleet").await.expect("project").spec;
    project.repositories = vec![flotilla_resources::ProjectRepositorySpec::builder().repo(repository.key()).build()];
    fleet.apply::<flotilla_resources::Project>("fleet", project).await;
    let ensure = flotilla_resources::ConvoyEnsureSpec::builder()
        .project_ref("fleet".into())
        .role("governor".into())
        .workflow_ref("engine-standing-workflow".into())
        .placement_policy(DUMMY_FULFILMENT_KIND.into())
        .repositories(vec![repository.key()])
        .build();
    apply_annotated::<ConvoyEnsure>(
        &fleet,
        "engine-standing",
        &[(flotilla_charter::ops_entry::SOURCE_COMMIT_ANNOTATION, "fixture")],
        ensure,
    )
    .await;
    let generation = |ensure: &ResourceObject<ConvoyEnsure>| {
        (ensure.metadata.name == "engine-standing").then(|| ensure.status.as_ref()?.convoy_ref.clone()).flatten()
    };
    let first = fleet.wait("a first generation", |ensure| generation(ensure).is_some()).await;
    let first = generation(&first).expect("first generation");
    let open = |status: &ConvoyStatus| !status.phase.is_terminal();
    // Abandon refuses a convoy still moving out of Pending.
    fleet.wait("the first generation Active", in_phase(&first, ConvoyPhase::Active)).await;
    let abandoned = fleet
        .execute(CommandAction::ConvoyAbandon {
            namespace: Some(NAMESPACE.into()),
            name: first.clone(),
            reason: "exercise standing re-admission".into(),
        })
        .await;
    assert!(!matches!(abandoned, CommandValue::Error { .. }), "{abandoned:?}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let second = loop {
        let ensure = fleet.get::<ConvoyEnsure>("engine-standing").await.expect("ensure");
        if let Some(next) = generation(&ensure).filter(|next| next != &first) {
            break next;
        }
        assert!(tokio::time::Instant::now() < deadline, "Ensure admits a fresh generation after abandon: {:?}", ensure.status);
        // The ended generation waits out its backoff; skip just that.
        skip_to_retry(&clock, ensure.status.and_then(|status| status.retry_at));
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    fleet.wait("the next generation admitted", convoy(&second, open)).await;
}

/// Apply `spec` as the `T` named `name`, with `annotations`, as `flotilla apply -f` does.
async fn apply_annotated<T: flotilla_resources::Resource>(fleet: &TestDaemon, name: &str, annotations: &[(&str, &str)], spec: T::Spec) {
    let annotations: serde_json::Map<_, _> = annotations.iter().map(|(key, value)| ((*key).to_string(), (*value).into())).collect();
    let document = serde_json::json!({
        "apiVersion": format!("{}/{}", T::API_PATHS.group, T::API_PATHS.version),
        "kind": T::API_PATHS.kind,
        "metadata": { "name": name, "namespace": NAMESPACE, "annotations": annotations },
        "spec": spec,
    });
    let applied = fleet.execute(CommandAction::ResourceApply { namespace: NAMESPACE.into(), document }).await;
    assert!(matches!(applied, CommandValue::ResourceObject(_)), "apply {} {name}: {applied:?}", T::API_PATHS.kind);
}

/// The dummy forge's service URL, as the seeded Forge records it.
async fn catalog_forge_url(fleet: &TestDaemon) -> String {
    fleet.get::<flotilla_resources::Forge>(catalog::DUMMY_FORGE_ID).await.expect("dummy forge").spec.https_url
}

fn skip_to_retry(clock: &SkippingClock, retry_at: Option<chrono::DateTime<chrono::Utc>>) {
    let now = flotilla_resources::Clock::now(clock);
    if let Some(retry_at) = retry_at.filter(|retry_at| *retry_at > now) {
        clock.skip(retry_at - now + chrono::Duration::seconds(1));
    }
}

// A fixed admitted identity makes the dummy provider's seeded failure draw
// reproducible while the runtime still bootstraps and reconciles the convoy.
async fn create_named(fleet: &TestDaemon, name: &str, workflow: &str) {
    let forge = catalog::forge(&catalog_forge_url(fleet).await);
    let repository = catalog::repository(&forge).expect("repository");
    let spec = flotilla_resources::ConvoySpec::builder()
        .workflow_ref(workflow.into())
        .role(name.into())
        .r#ref("main".into())
        .project_ref("fleet".into())
        .placement_policy(DUMMY_FULFILMENT_KIND.into())
        .repositories(vec![flotilla_resources::ConvoyRepositorySpec {
            url: catalog::repository_url(&forge),
            repo_ref: repository.key(),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "dummy".into(),
            subpaths: Vec::new(),
        }])
        .build();
    fleet.apply::<Convoy>(name, spec).await;
}

// A real governor must resume a stalled coder and deliver its convoy.
#[tokio::test(start_paused = true)]
async fn on_the_engine_crew_supervision_resumes_a_stalled_coder() {
    supervised_coder_scenario(CrewSupervisionAction::Resume).await;
}

// A failing crew proposes failure; its named governor must fail it.
#[tokio::test(start_paused = true)]
async fn on_the_engine_crew_supervision_fails_a_failing_coder() {
    supervised_coder_scenario(CrewSupervisionAction::Fail).await;
}

async fn supervised_coder_scenario(action: CrewSupervisionAction) {
    use flotilla_environment::environment::dummy::dummy_draw;
    use flotilla_resources::SupervisionTarget;

    let (domain, agent) =
        if action == CrewSupervisionAction::Resume { ("crew-stall", "stall_rate = 0.5\n") } else { ("crew-failure", "fail_rate = 0.5\n") };
    // Explicitly cover a faulting coder and a healthy supervisor, without
    // random UUIDs changing the dummy provider's deterministic draws.
    let name = (0..128)
        .map(|n| format!("engine-supervised-{n}"))
        .find(|name| {
            dummy_draw(0, domain, &format!("{name}/{name}-implement/coder")) < 0.5
                && dummy_draw(0, domain, &format!("{name}/{name}-supervise/governor")) >= 0.5
        })
        .expect("seeded case");
    let fleet = TestDaemon::builder().agent(agent).settings(PRIMITIVES).start().await;
    let mut workflow = CrewMix::CoderSupervisor.workflow(LONG_RUNNER_TURNS);
    workflow.supervision =
        Some(vec![SupervisionTarget::ConvoyCrew { vessel: "supervise".into(), role: "governor".into() }, SupervisionTarget::Operator]);
    fleet.apply::<WorkflowTemplate>("engine-supervised", workflow).await;
    create_named(&fleet, &name, "engine-supervised").await;
    let supervised = |status: &ConvoyStatus| status.stalled.as_ref().is_some_and(|stall| stall.supervisor.is_some());
    fleet.wait("crew-supervised stall", convoy(&name, supervised)).await;
    let supervisor =
        fleet.wait("the governor's session", |session: &ResourceObject<TerminalSession>| session.spec.role == "governor").await;
    let actor = supervisor.status.expect("status").crew.expect("crew identity").id;
    let mut ruling = supervise(&name, action);
    if let CommandAction::CrewSupervise { actor_crew_id, .. } = &mut ruling {
        *actor_crew_id = Some(actor);
    }
    assert_eq!(fleet.execute(ruling).await, CommandValue::Ok, "the named crew supervisor can rule");
    let phase = if action == CrewSupervisionAction::Resume { ConvoyPhase::Landed } else { ConvoyPhase::Failed };
    fleet.wait("the ruling's outcome", in_phase(&name, phase)).await;
}

// A daemon restart restores its active convoy while the crew process survives.
// Its next submissions deliver the convoy without re-admission or a duplicate
// process launch.
#[tokio::test(start_paused = true)]
async fn on_the_engine_a_daemon_restart_restores_an_active_convoy_and_lands() {
    let fleet =
        TestDaemon::builder().agent("turn_delay_ms = 10000").settings("host_name = \"restart-proof\"\n[primitives]\n").start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let before = fleet
        .wait("a launched coder before its first submission", |found: &ResourceObject<Convoy>| {
            found.metadata.name == name && fleet.counts().sessions_launched == 1
        })
        .await;
    let fleet = fleet.restart().await;
    let restored = fleet.get::<Convoy>(&name).await.expect("restored convoy");
    assert_eq!(restored.spec, before.spec, "the convoy's promises and seats survive restart");
    fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    assert_eq!(fleet.counts().sessions_launched, 1, "the surviving process is adopted");
}

/// Wall time moved forward by however much a test skips, so retry backoff passes at once.
#[derive(Default)]
struct SkippingClock(std::sync::Mutex<chrono::Duration>);

impl SkippingClock {
    fn skip(&self, by: chrono::Duration) {
        *self.0.lock().expect("clock") += by;
    }
}

impl flotilla_resources::Clock for SkippingClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + *self.0.lock().expect("clock")
    }
}

// Behaviour (#3824 layer 8b): with `[primitives]` on, a standing convoy whose
// generation ends three times is held, and the hold is an attention from the standing
// role in its project's operator inbox. The person sees it in `message inbox`;
// acknowledging it releases the hold, and the next generation is admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_acknowledging_a_standing_convoys_hold_admits_its_next_generation() {
    use flotilla_charter::ops_entry::{
        ENSURED_FROM_ANNOTATION, MATERIALIZED_PROJECT_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION,
        SOURCE_REPOSITORY_ANNOTATION,
    };
    use flotilla_resources::{
        ConvoyEnsure, ConvoyEnsureHoldReason, ConvoyEnsureSpec, Project, ProjectRepositoryRole, ProjectRepositorySpec,
    };

    let clock = Arc::new(SkippingClock::default());
    let fleet = TestDaemon::builder().settings(PRIMITIVES).clock(clock.clone() as Arc<dyn flotilla_resources::Clock>).start().await;
    let forge = catalog::forge(&catalog_forge_url(&fleet).await);
    let repository = catalog::repository(&forge).expect("repository").key();
    // A standing role runs until it is stopped: its workflow declares no exit.
    let mut workflow = CrewMix::Coder.workflow(LONG_RUNNER_TURNS);
    workflow.exit = None;
    fleet.apply::<WorkflowTemplate>("standing-coder", workflow).await;
    let project = flotilla_resources::ProjectSpec::builder()
        .display_name("Standing".to_string())
        .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
        .repositories(vec![ProjectRepositorySpec {
            charter_store: None,
            repo: repository.clone(),
            alias: Some("app".into()),
            roles: std::collections::BTreeSet::from([ProjectRepositoryRole::Code]),
            subpath: None,
            default_branch: Some("main".into()),
        }])
        .build();
    fleet.apply::<Project>("standing", project).await;
    let source = repository.to_string();
    let annotations = [
        (MATERIALIZED_PROJECT_ANNOTATION, "standing"),
        (SOURCE_REPOSITORY_ANNOTATION, source.as_str()),
        (SOURCE_COMMIT_ANNOTATION, "abc123"),
        (SOURCE_ENTRY_PATH_ANNOTATION, "ops/quartermaster.md"),
    ];
    let ensure = ConvoyEnsureSpec {
        project_ref: "standing".into(),
        role: "quartermaster".into(),
        driver_ref: None,
        workflow_ref: "standing-coder".into(),
        placement_policy: Some(DUMMY_FULFILMENT_KIND.into()),
        escalation_reason: None,
        repositories: vec![repository.clone()],
        presents_as: None,
        agent_overrides: Vec::new(),
    };
    apply_annotated::<ConvoyEnsure>(&fleet, "quartermaster", &annotations, ensure).await;

    let generations = || async {
        fleet
            .list::<Convoy>()
            .await
            .into_iter()
            .filter(|convoy| convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION).map(String::as_str) == Some("quartermaster"))
            .collect::<Vec<_>>()
    };
    let ensure = || async { fleet.get::<ConvoyEnsure>("quartermaster").await.and_then(|ensure| ensure.status) };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !ensure().await.is_some_and(|status| status.hold_reason == Some(ConvoyEnsureHoldReason::RestartLimit)) {
        let latest = generations().await.into_iter().max_by_key(|convoy| convoy.spec.generation);
        assert!(
            tokio::time::Instant::now() < deadline,
            "never held: {:?}, ensure {:?}",
            latest.as_ref().map(|convoy| (convoy.metadata.name.clone(), convoy.status.as_ref().map(|status| status.phase))),
            ensure().await
        );
        match latest.as_ref().and_then(|convoy| Some((convoy, convoy.status.as_ref()?))) {
            // An operator abandons each generation; the standing role counts it a failure.
            Some((convoy, status)) if !status.phase.is_terminal() => {
                let abandon = CommandAction::ConvoyAbandon {
                    namespace: Some(NAMESPACE.into()),
                    name: convoy.metadata.name.clone(),
                    reason: "the generation failed".into(),
                };
                fleet.execute(abandon).await;
            }
            _ => {}
        }
        // Each ended generation waits out its backoff; skip just that.
        skip_to_retry(&clock, ensure().await.and_then(|status| status.retry_at));
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(generations().await.len(), 3, "three ended generations, then the hold");

    let inbox = CommandAction::PersonInbox { namespace: Some(NAMESPACE.into()) };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let item = loop {
        let CommandValue::PersonInbox(listing) = fleet.execute(inbox.clone()).await else { panic!("a person inbox") };
        // The standing role may also report config drift on a generation; the hold is the one
        // that gives the restart limit's reason.
        let reason = flotilla_admission::reconcilers::convoy_ensure::restart_limit_reason(3);
        if let Some(item) = listing.items.into_iter().find(|item| item.sender == "fleet:lab/role:standing" && item.body == reason) {
            break item;
        }
        assert!(tokio::time::Instant::now() < deadline, "the hold never reached a person");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(item.inbox, "project:standing/role:operator");
    assert_eq!(item.expects, "nothing");
    clock.skip(chrono::Duration::hours(1));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(generations().await.len(), 3, "the hold holds until a person acknowledges it");

    let acknowledged =
        fleet.execute(CommandAction::PersonAcknowledge { namespace: Some(NAMESPACE.into()), message: item.message.clone() }).await;
    assert_eq!(acknowledged, CommandValue::Ok);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while generations().await.len() < 4 {
        assert!(tokio::time::Instant::now() < deadline, "the next generation was never admitted");
        // Released, the role restarts after its ordinary backoff.
        clock.skip(chrono::Duration::minutes(1));
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let CommandValue::PersonInbox(listing) = fleet.execute(inbox).await else { panic!("a person inbox") };
    assert!(listing.items.iter().all(|left| left.message != item.message), "an acknowledged hold waits no more");
}

// Behaviour (#3938): a crew stall escalated past every supervisor reaches the
// operator role of the convoy's project, where the person consumer is bound.
// Listing it takes nothing; the person's acknowledgement is the receipt, and
// it settles the escalation, which expects nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#4323: an escalation to principal:operator is lost while an operator client is connected"]
async fn on_the_engine_a_stall_past_every_supervisor_reaches_the_projects_operator_and_the_person() {
    use flotilla_resources::SupervisionTarget;

    let fleet = TestDaemon::builder().agent("stall_rate = 1.0").settings(PRIMITIVES).start().await;
    let mut workflow = CrewMix::Coder.workflow(LONG_RUNNER_TURNS);
    workflow.supervision = Some(vec![
        SupervisionTarget::ConvoyCrew { vessel: String::new(), role: "bosun".into() },
        SupervisionTarget::ProjectCrew { convoy_role: "governor".into(), vessel: String::new(), role: "governor".into() },
        SupervisionTarget::Address { address: "principal:operator".into() },
    ]);
    fleet.apply::<WorkflowTemplate>("engine-escalated", workflow).await;
    create_named(&fleet, "engine-escalated", "engine-escalated").await;
    fleet.wait("a supervisable stall", stalled("engine-escalated")).await;
    let inbox = CommandAction::PersonInbox { namespace: Some(NAMESPACE.into()) };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let item = loop {
        let CommandValue::PersonInbox(listing) = fleet.execute(inbox.clone()).await else { panic!("a person inbox") };
        if let Some(item) = listing.items.into_iter().find(|item| item.inbox == "project:fleet/role:operator") {
            break item;
        }
        assert!(tokio::time::Instant::now() < deadline, "the escalation never reached the operator role");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(item.sender, "project:fleet/convoy:engine-escalated/role:coder");
    assert!(item.body.starts_with("Escalated from coder@implement"), "{item:?}");
    let CommandValue::PersonInbox(listing) = fleet.execute(inbox.clone()).await else { panic!("a person inbox") };
    assert!(listing.items.iter().any(|left| left.message == item.message), "listing it is no receipt: {listing:?}");

    let acknowledged =
        fleet.execute(CommandAction::PersonAcknowledge { namespace: Some(NAMESPACE.into()), message: item.message.clone() }).await;
    assert_eq!(acknowledged, CommandValue::Ok);
    let CommandValue::PersonInbox(listing) = fleet.execute(inbox).await else { panic!("a person inbox") };
    assert!(listing.items.iter().all(|left| left.message != item.message), "an acknowledged message waits no more: {listing:?}");

    // Answering it after acknowledging takes nothing more and replies to the
    // crew as the role it was addressed to.
    let answer = CommandAction::PersonAnswer {
        namespace: Some(NAMESPACE.into()),
        message: item.message.clone(),
        body: "Carry on with the stable API.".into(),
    };
    assert_eq!(fleet.execute(answer.clone()).await, CommandValue::Ok);
    assert_eq!(fleet.execute(answer).await, CommandValue::Ok, "the same answer again is the reply already sent");
    let CommandValue::Error { message } = fleet
        .execute(CommandAction::PersonAnswer {
            namespace: Some(NAMESPACE.into()),
            message: item.message.clone(),
            body: "A different answer.".into(),
        })
        .await
    else {
        panic!("a different answer to the same message is refused")
    };
    assert!(message.contains("already answered"), "{message}");
}

// A lone coder delivers its convoy after the daemon runtime realises its
// environment and launches its crew, and tears the environment down after.
#[tokio::test(start_paused = true)]
async fn on_the_engine_a_lone_coder_realises_its_resources_and_delivers() {
    let fleet = TestDaemon::builder().settings(PRIMITIVES).start().await;
    let (_, landed) = deliver(&fleet, CrewMix::Coder).await;
    assert_eq!((landed.environments_provisioned, landed.sessions_launched, landed.crews_completed), (1, 1, 1), "{landed:?}");
    fleet.wait("the environment torn down", |_: &ResourceObject<Convoy>| fleet.counts().live_environments == 0).await;
    assert_eq!(fleet.counts().environments_destroyed, 1);
}

// An environment that cannot realise fails its convoy and launches no terminal session.
#[tokio::test(start_paused = true)]
async fn on_the_engine_an_unrealised_environment_records_failure_without_a_terminal() {
    let fleet = TestDaemon::builder().environment("failure_rate = 1.0").settings(PRIMITIVES).start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    fleet.wait("Failed", in_phase(&name, ConvoyPhase::Failed)).await;
    assert!(fleet.list::<TerminalSession>().await.is_empty(), "no terminal launches");
    assert_eq!(fleet.counts().sessions_launched, 0);
}

// A missed event must not be hidden by the harness's 200ms resync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_coder_lands_at_the_production_resync_interval() {
    let fleet = TestDaemon::builder().resync(RuntimeOptions::default().controller_resync_interval).start().await;
    let (_, snapshot) = deliver(&fleet, CrewMix::Coder).await;
    assert_eq!(snapshot.command_errors, 0, "{snapshot:?}");
}

/// Replace `name`'s status with `patch` applied to what `get` read, as an
/// operator's `flotilla` status patch does.
async fn patch_status<T: Resource>(fleet: &TestDaemon, name: &str, patch: &impl StatusPatch<T::Status>)
where
    T::Status: Default,
{
    let current = fleet.get::<T>(name).await.unwrap_or_else(|| panic!("{} {name}", T::API_PATHS.kind));
    let mut status = current.status.unwrap_or_default();
    patch.apply(&mut status);
    let patched = fleet
        .execute(CommandAction::ResourceStatusPatch {
            expected_resource_version: None,
            namespace: NAMESPACE.into(),
            kind: T::API_PATHS.kind.into(),
            name: name.into(),
            status: serde_json::to_value(status).expect("status"),
        })
        .await;
    assert!(matches!(patched, CommandValue::ResourceObject(_)), "patch {} {name}: {patched:?}", T::API_PATHS.kind);
}

// Behaviour (#3824 layer 8b): the input-needed and declaration-refused Demands are gone; their
// facts are read from the records that hold them, with the switch off and on. A crew session that
// needs input raises its vessel's salience, and a refused declaration is listed by `fleet list`.
async fn judgements_scenario(settings: &str) {
    use flotilla_protocol::{AwarenessGrouping, AwarenessKind, DeclarationAttentionKind, QueryId, ResultSet, Salience};
    use flotilla_resources::{
        DeclarationRefusedCondition, Project, ProjectSpec, ProjectStatusPatch, TerminalAttention, TerminalAttentionSource,
        TerminalAttentionState, TerminalSessionStatusPatch, CONVOY_LABEL,
    };

    let fleet = TestDaemon::builder().agent("stall_rate = 1.0").settings(settings).start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    fleet.wait("a stall", stalled(&name)).await;
    // The stalled crew's hook reports it idle; the person's input is asked after that.
    let attention =
        |session: &ResourceObject<TerminalSession>| session.status.as_ref()?.attention.as_ref().map(|attention| attention.state);
    let session = fleet
        .wait("the crew's session idle", |session: &ResourceObject<TerminalSession>| {
            session.metadata.labels.get(CONVOY_LABEL) == Some(&name) && attention(session) == Some(TerminalAttentionState::Idle)
        })
        .await;
    let query = QueryId::Awareness { scope: None, grouping: AwarenessGrouping::Convoy, limit: Default::default() };
    let salience = |result: &ResultSet| {
        result.rows.as_awareness().and_then(|nodes| {
            nodes.iter().flat_map(|node| &node.entries).find(|entry| entry.kind == AwarenessKind::Vessel).map(|entry| entry.salience)
        })
    };
    let before = fleet.wait_query(query.clone(), |_| true).await;
    assert!(salience(&before) < Some(Salience::Attention), "the stalled vessel is not yet raised: {:?}", salience(&before));
    let needs_input = TerminalSessionStatusPatch::ObserveAttention {
        attention: TerminalAttention {
            state: TerminalAttentionState::NeedsInput,
            as_of: chrono::Utc::now(),
            source: TerminalAttentionSource::Hook,
        },
    };
    patch_status::<TerminalSession>(&fleet, &session.metadata.name, &needs_input).await;
    fleet.wait_query(query, |result| salience(result) == Some(Salience::Attention)).await;

    let refusing = ProjectSpec::builder()
        .display_name("Refusing".to_string())
        .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
        .repositories(Vec::new())
        .build();
    fleet.apply::<Project>("refusing", refusing).await;
    let now = chrono::Utc::now();
    let refused = ProjectStatusPatch::DeclarationRefused {
        condition: Some(DeclarationRefusedCondition {
            entry_path: "ops/broken.md".into(),
            message: "unknown_option".into(),
            since: now,
            observed_at: now,
        }),
    };
    patch_status::<Project>(&fleet, "refusing", &refused).await;
    let listed = fleet.execute(CommandAction::QueryFleetList { project: None, crew_id: None, convoy: None }).await;
    let CommandValue::FleetList(listed) = listed else { panic!("fleet list: {listed:?}") };
    assert!(
        listed.declaration_attention.iter().any(|row| row.resource.name == "refusing"
            && row.condition == DeclarationAttentionKind::DeclarationRefused
            && row.message.contains("unknown_option")),
        "{:?}",
        listed.declaration_attention
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_needed_and_declaration_refusals_are_read_from_their_records() {
    judgements_scenario("").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_input_needed_and_declaration_refusals_are_read_from_their_records() {
    judgements_scenario(PRIMITIVES).await;
}

/// Each listed record's name and spec, with how many times it is listed.
type Inventory = BTreeMap<(String, String), usize>;

/// What `fleet` lists of `T`.
async fn inventory<T: Resource>(fleet: &TestDaemon) -> Inventory {
    let mut inventory = BTreeMap::new();
    for object in fleet.list::<T>().await {
        *inventory.entry((object.metadata.name, serde_json::to_string(&object.spec).expect("spec"))).or_default() += 1;
    }
    inventory
}

/// Every `T` the source held before peering is listed at the destination
/// beside its own. A definition is read by name, so one that both hosts define
/// alike is listed once.
async fn assert_replicated<T: Resource>(source: &Inventory, destination_before: &Inventory, destination: &TestDaemon) {
    let by_name = T::REPLICATION_CLASS == flotilla_resources::ReplicationClass::Definitions;
    let missing = |listed: &Inventory| -> Vec<String> {
        let wanted = |record, count| if by_name { 1 } else { destination_before.get(record).copied().unwrap_or(0) + count };
        let missing = source.iter().filter(|(record, count)| listed.get(*record).copied().unwrap_or(0) < wanted(*record, **count));
        missing.map(|((name, _), _)| name.clone()).collect()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let missing = missing(&inventory::<T>(destination).await);
        if missing.is_empty() {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "{} not replicated: {missing:?}", T::API_PATHS.kind);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// No `T` the source held is listed at the destination.
async fn assert_home_local<T: Resource>(source: &Inventory, destination: &TestDaemon) {
    let listed = inventory::<T>(destination).await;
    let leaked: Vec<_> = source.keys().filter(|record| listed.contains_key(*record)).map(|(name, _)| name).collect();
    assert!(leaked.is_empty(), "home-local {} listed at its peer: {leaked:?}", T::API_PATHS.kind);
}

/// Apply the `T` named `name` from its spec's JSON.
async fn apply_json<T: Resource>(fleet: &TestDaemon, name: &str, spec: serde_json::Value)
where
    T::Spec: serde::de::DeserializeOwned,
{
    let spec = serde_json::from_value(spec).unwrap_or_else(|error| panic!("{} {name}: {error}", T::API_PATHS.kind));
    fleet.apply::<T>(name, spec).await;
}

/// The first built-in `T`'s spec.
fn builtin<T: Resource>() -> serde_json::Value {
    flotilla_resources::builtins::builtin_definitions(T::API_PATHS.kind).expect("built-ins")[0].spec.clone()
}

macro_rules! replication_kinds {
    ($($kind:ident),* $(,)?) => {
        /// Every kind's inventory on `fleet`, by kind.
        async fn inventories(fleet: &TestDaemon) -> BTreeMap<&'static str, Inventory> {
            let mut inventories = BTreeMap::new();
            $(inventories.insert(<flotilla_resources::$kind as Resource>::API_PATHS.kind, inventory::<flotilla_resources::$kind>(fleet).await);)*
            inventories
        }

        async fn assert_every_kind_replicated(
            source: &BTreeMap<&'static str, Inventory>,
            destination_before: &BTreeMap<&'static str, Inventory>,
            destination: &TestDaemon,
        ) {
            let home_local = |class| class == flotilla_resources::ReplicationClass::None;
            $(if !home_local(<flotilla_resources::$kind as Resource>::REPLICATION_CLASS) {
                let kind = <flotilla_resources::$kind as Resource>::API_PATHS.kind;
                assert_replicated::<flotilla_resources::$kind>(&source[kind], &destination_before[kind], destination).await;
            })*
            // Checked once everything else has replicated, so a leak would have arrived.
            $(if home_local(<flotilla_resources::$kind as Resource>::REPLICATION_CLASS) {
                let kind = <flotilla_resources::$kind as Resource>::API_PATHS.kind;
                assert_home_local::<flotilla_resources::$kind>(&source[kind], destination).await;
            })*
        }
    };
}

replication_kinds!(
    AccessDelivery,
    AccountBinding,
    AccountProvision,
    Artifact,
    ChangeRequest,
    Checkout,
    Clone,
    Convoy,
    ConvoyEnsure,
    CredentialGrant,
    CredentialSpec,
    CrewDefaults,
    Environment,
    FleetDesignation,
    Forge,
    ForgeRead,
    FulfilmentKind,
    Host,
    ImageBuild,
    ImageLayer,
    Issue,
    ManifestRoot,
    Message,
    PlacementPolicy,
    Project,
    Repository,
    TerminalSession,
    Vessel,
    WorkflowTemplate,
);

/// One record of each kind the stalled convoy does not make, applied as an
/// operator applies them; the controllers then keep them as they keep any.
async fn apply_transport_fixtures(fleet: &TestDaemon, host: &str) {
    use flotilla_resources::{
        account_access::{requester_address, AccessName, BindingState, CredentialName, ProvisionPlan, Selection, UpdateMode},
        AccessDelivery, AccessDeliverySpec, AccountBinding, AccountBindingSpec, AccountProvision, AccountProvisionSpec, ChangeRequest,
        ConvoyEnsure, CredentialGrant, CredentialSpec, CrewDefaults, ImageBuild, ImageLayer, Issue, ManifestRoot, PlacementPolicy,
    };
    let local = fleet.list::<Host>().await.into_iter().next().expect("local host").metadata.name;
    let place = flotilla_resources::host_direct_environment_name(&local);
    let at = flotilla_schema::Timestamp::from_datetime(&chrono::Utc::now());
    let credential = format!("{host}-credential");
    let spec = serde_json::json!({
        "consumer": {"adapter": "claude-oauth", "account_email": format!("{host}@example.invalid")},
        "lifecycle": "static", "source": {"kind": "file", "path": "/transport/token"}
    });
    apply_json::<CredentialSpec>(fleet, &credential, spec).await;
    let grant = serde_json::json!({"credentials": [credential], "selector": {"projects": ["fleet"]}});
    apply_json::<CredentialGrant>(fleet, &format!("{host}-grant"), grant).await;
    apply_json::<CrewDefaults>(fleet, &format!("{host}-defaults"), serde_json::json!({})).await;
    let layer = builtin::<ImageLayer>();
    apply_json::<ImageLayer>(fleet, &format!("{host}-layer"), layer.clone()).await;
    apply_json::<WorkflowTemplate>(fleet, &format!("{host}-template"), builtin::<WorkflowTemplate>()).await;
    let binding = format!("{host}-binding");
    let spec = AccountBindingSpec {
        requester: requester_address(Some("fleet"), &format!("{host}-convoy"), "coder").expect("requester"),
        item: "claude-code".into(),
        requirement: AccessName("claude-code:account".into()),
        credential: CredentialName(credential.clone()),
        account: None,
        grants: Default::default(),
        candidates: Default::default(),
        exclusions: Default::default(),
        selection: Selection::Draw { index: 0 },
        epochs: flotilla_schema::Records::new(),
        opened_at: at,
        state: flotilla_schema::State::new(BindingState::Open, at),
    };
    fleet.apply::<AccountBinding>(&binding, spec).await;
    let provision = AccountProvisionSpec {
        binding: flotilla_schema::Ref::new(&binding, &local),
        place: place.clone(),
        environment_kind: "host-direct".into(),
        plan: ProvisionPlan {
            implementation: "claude-direct".into(),
            effective_access: "account".into(),
            mechanism: "oauth token".into(),
            update_mode: UpdateMode::Push,
        },
        home: "/transport".into(),
        delivered: None,
    };
    fleet.apply::<AccountProvision>(&format!("{host}-provision"), provision).await;
    let access = format!("{host}-access");
    let delivery = AccessDeliverySpec {
        access: access.clone(),
        place,
        harness: "claude-code".into(),
        home: "/transport".into(),
        needs: Default::default(),
        retry: None,
    };
    fleet.apply::<AccessDelivery>(&access, delivery).await;
    let observed = |number: u64| serde_json::json!({"service": "https://github.com", "scope": format!("{host}/app"), "number": number, "observing_authority": local});
    apply_json::<ChangeRequest>(fleet, &format!("{host}-change-request"), observed(1)).await;
    apply_json::<Issue>(fleet, &format!("{host}-issue"), observed(2)).await;
    let root = serde_json::json!({"host": local, "path": "/transport/manifests", "source": "transport"});
    apply_json::<ManifestRoot>(fleet, &format!("manifest-{host}"), root).await;
    apply_json::<PlacementPolicy>(fleet, &format!("{host}-policy"), serde_json::json!({"pool": "transport"})).await;
    let ensure = serde_json::json!({"project_ref": "fleet", "role": format!("{host}-standing"), "workflow_ref": CrewMix::Coder.workflow_name(), "repositories": []});
    apply_json::<ConvoyEnsure>(fleet, &format!("{host}-ensure"), ensure).await;
    let layers: Vec<flotilla_resources::ImageLayerSpec> = vec![serde_json::from_value(layer).expect("a current layer")];
    let identity = flotilla_resources::image_identity(&layers);
    let name = flotilla_resources::image_build_name(&identity, "amd64", &flotilla_resources::ImageDelivery::Local, &local);
    let build = serde_json::json!({
        "identity": identity,
        "architecture": "amd64",
        "layers": layers,
        "host_ref": local,
        "delivery": {"kind": "local"},
        "reservation": {"cpu": 1, "disk_bytes": 1},
    });
    apply_json::<ImageBuild>(fleet, &name, build).await;
}

// Both peer runtimes consume the real change stream. What each host made is
// listed at the other, as `flotilla get` shows it there, and home-local kinds
// stay home.
#[tokio::test(start_paused = true)]
async fn on_the_engine_two_daemons_replicate_every_kind() {
    // Replication is under test, not missed events: the production resync
    // keeps the paused clock's controller passes, which cost real time, few.
    let resync = RuntimeOptions::default().controller_resync_interval;
    let (settings_a, settings_b) = (format!("host_name = \"engine-a\"\n{PRIMITIVES}"), format!("host_name = \"engine-b\"\n{PRIMITIVES}"));
    let (a, b) = tokio::join!(
        TestDaemon::builder().agent("stall_rate = 1.0").settings(&settings_a).resync(resync).start(),
        TestDaemon::builder().agent("stall_rate = 1.0").settings(&settings_b).resync(resync).start()
    );
    // A stalled crew keeps its vessel, environment, checkout, session and messages.
    let (stalled_a, stalled_b) =
        tokio::join!(a.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND), b.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND));
    tokio::join!(a.wait("a stall", stalled(&stalled_a)), b.wait("a stall", stalled(&stalled_b)));
    apply_transport_fixtures(&a, "engine-a").await;
    apply_transport_fixtures(&b, "engine-b").await;
    let (before_a, before_b) = tokio::join!(inventories(&a), inventories(&b));
    // Under `[primitives]` a Message lives only on the engine, which `get`
    // does not read (see `assert_no_old_messages`), so there is none to compare.
    let unmade: Vec<_> = before_a.iter().filter(|(_, records)| records.is_empty()).map(|(kind, _)| *kind).collect();
    assert_eq!(unmade, ["Message"], "every other kind has a record to replicate");
    let _peering = TestDaemon::peer(&a, &b).await;
    assert_every_kind_replicated(&before_a, &before_b, &b).await;
    assert_every_kind_replicated(&before_b, &before_a, &a).await;
}
