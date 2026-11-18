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
    CommandAction, ConvoyNaming,
};
use flotilla_resources::{
    Convoy, ConvoyPhase, ConvoyStatus, CrewWorkPhase, FleetDesignation, FleetDesignationSpec, Host, Resource, ResourceObject, StatusPatch,
    TerminalSession, WorkflowTemplate, FLEET_DESIGNATION_NAME,
};

/// Long runners' turns in the harness's `[dummy_fleet.agent]`.
const LONG_RUNNER_TURNS: u32 = 2;

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
    convoy(name, |status| status.stalled.is_some())
}

/// The promises of `vessel`'s `role` in convoy `name`, as explain reads them: the engine's.
async fn promises(fleet: &TestDaemon, name: &str, vessel: &str, role: &str) -> Vec<flotilla_resources::promises::Promise> {
    let explain = CommandAction::QueryExplainConvoy { namespace: Some(NAMESPACE.into()), name: name.into() };
    let CommandValue::ConvoyExplanation(explained) = fleet.execute(explain).await else { panic!("an explanation") };
    explained
        .promises
        .into_iter()
        .map(|promise| serde_json::from_value::<flotilla_resources::promises::Promise>(promise).expect("promise"))
        .filter(|promise| promise.vessel == vessel && promise.role == role)
        .collect()
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
    let promises = promises(&fleet, &convoy.metadata.name, "coder", "coder").await;
    let status = convoy.status.expect("status");
    let promise = promises.iter().find(|p| p.kind == flotilla_resources::promises::PromiseKind::Pr).expect("pr promise");
    assert_eq!(promise.state, flotilla_resources::promises::PromiseState::Kept);
    assert_eq!(promise.id, "template/pr");
    assert_eq!(promise.source, flotilla_resources::promises::PromiseSource::Template);
    let done = &status.crew_work["coder"]["coder"];
    assert_eq!(done.completion_authority, flotilla_resources::WorkCompletionAuthority::Promises);
    // A seat whose promises are kept is Done, finished when they were kept.
    assert_eq!(done.phase, flotilla_resources::CrewWorkPhase::Done);
    assert!(done.finished_at.is_some());
}

// A convoy dispatched into the Fleet project has its project, so explain scores its dispatch, and
// both it and one created from a template that authors its vessels record that their operator
// named them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_named_convoy_records_its_naming_and_has_its_project() {
    let fleet = TestDaemon::builder().start().await;
    let dispatched = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    // A create does not allocate, so its template authors its vessel.
    let mut authored = CrewMix::Coder.workflow(LONG_RUNNER_TURNS);
    let crew = std::mem::take(&mut authored.roles);
    authored.vessels = vec![flotilla_resources::VesselRequirement::builder().name("implement".into()).crew(crew).build()];
    fleet.apply::<WorkflowTemplate>("authored-coder", authored).await;
    let created = fleet
        .execute(CommandAction::ConvoyCreate {
            name: "created-from-a-template".into(),
            workflow_ref: "authored-coder".into(),
            inputs: Vec::new(),
            r#ref: Some("main".into()),
            project_ref: catalog::FLEET_PROJECT.into(),
            placement_policy: Some(DUMMY_FULFILMENT_KIND.into()),
            adopted_checkout: None,
        })
        .await;
    assert!(matches!(created, CommandValue::ConvoyCreated { .. }), "{created:?}");
    let created = convoy_named(&fleet, "created-from-a-template").await;
    let explain = |name: &str| CommandAction::QueryExplainConvoy { namespace: Some(NAMESPACE.into()), name: name.into() };
    let CommandValue::ConvoyExplanation(explained) = fleet.execute(explain(&dispatched)).await else { panic!("an explanation") };
    assert_eq!(explained.naming, Some(ConvoyNaming::Explicit));
    assert_eq!(explained.dispatch_score_error, None);
    let found = fleet.get::<Convoy>(&dispatched).await.expect("the dispatched convoy");
    assert_eq!(found.spec.project_ref, catalog::FLEET_PROJECT);
    let CommandValue::ConvoyExplanation(explained) = fleet.execute(explain(&created)).await else { panic!("an explanation") };
    assert_eq!(explained.naming, Some(ConvoyNaming::Explicit));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reviewed_coder_lands_after_its_reviewer_answers() {
    let fleet = TestDaemon::builder().start().await;
    let (convoy, snapshot) = deliver(&fleet, CrewMix::CoderReviewer).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, 2, "the review request and its answer are delivered turns: {snapshot:?}");
    let promises = promises(&fleet, &convoy.metadata.name, "coder", "coder").await;
    let status = convoy.status.unwrap();
    let pr = promises.iter().find(|p| p.kind == flotilla_resources::promises::PromiseKind::Pr).unwrap();
    assert_eq!(pr.id, "template/pr");
    assert_eq!(pr.state, flotilla_resources::promises::PromiseState::Kept);
    assert!(status.crew_work.values().flat_map(|crew| crew.values()).all(|work| work.phase == CrewWorkPhase::Done));
    assert_no_old_messages(&fleet).await;
}

// Behaviour: a crew keeps its own role's promises through a handoff (#3574 decision 3). A
// co-located coder and reviewer, with change requests merged at once, each settle their own
// promises after the review exchange: none is refused, each delivers its two turns, and every
// convoy lands. Many convoys at once give the merge many chances to come before the handoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn co_located_reviewed_coders_settle_their_own_promises() {
    use flotilla_resources::promises::{PromiseKind, PromiseState};

    const CONVOYS: usize = 24;
    let fleet = TestDaemon::builder().start().await;
    let mut names = Vec::new();
    for _ in 0..CONVOYS {
        names.push(fleet.dispatch(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await);
    }
    for name in &names {
        fleet.wait("Landed", in_phase(name, ConvoyPhase::Landed)).await;
        // Each role's promises are its own and kept: the change request stays with the coder.
        for (role, holds_pr) in [("coder", true), ("reviewer", false)] {
            let held = promises(&fleet, name, "coder", role).await;
            assert!(held.iter().all(|promise| promise.state == PromiseState::Kept), "{name} {role}: {held:?}");
            assert!(held.iter().any(|promise| promise.kind == PromiseKind::DecisionLedger), "{name} {role}: {held:?}");
            assert_eq!(held.iter().any(|promise| promise.kind == PromiseKind::Pr), holds_pr, "{name} {role}: {held:?}");
        }
    }
    let snapshot = fleet.counts();
    assert_eq!(snapshot.command_errors, 0, "{snapshot:?}");
    assert_eq!((snapshot.crews_completed, snapshot.turns_received), (2 * CONVOYS as u64, 2 * CONVOYS as u64), "{snapshot:?}");
}

// Behaviour: a handoff reassigns only promises its sender names, each its own and open; naming one
// that is not the sender's refuses the handoff, naming the promise.
#[tokio::test(start_paused = true)]
async fn a_handoff_refuses_to_reassign_a_promise_not_the_senders() {
    let fleet = TestDaemon::builder().agent("turn_delay_ms = 600000").start().await;
    let name = fleet.dispatch(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await;
    let session = fleet.wait("the coder's session", |session: &ResourceObject<TerminalSession>| session.spec.role == "coder").await;
    let flotilla_resources::TerminalSessionSource::Agent { context: session_context, .. } = session.spec.source else { panic!("agent") };
    let context = flotilla_protocol::query::CrewCommandContext {
        namespace: Some(NAMESPACE.into()),
        convoy: Some(name.clone()),
        vessel_ref: Some(session_context.vessel_ref),
        role: Some("coder".into()),
        ..Default::default()
    };
    let refused = fleet
        .execute(CommandAction::CrewHandoff {
            context,
            target: "reviewer".into(),
            message: "Review this.".into(),
            carries: Vec::new(),
            reassign: vec!["template/pr".into(), "the-reviewers".into()],
        })
        .await;
    let CommandValue::Error { message } = refused else { panic!("refused: {refused:?}") };
    assert!(message.contains("`the-reviewers`"), "{message}");
    assert_eq!(fleet.counts().turns_received, 0, "a refused handoff delivers nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_runner_exchanges_its_turns_before_landing() {
    let fleet = TestDaemon::builder().start().await;
    let (_, snapshot) = deliver(&fleet, CrewMix::LongRunner).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, u64::from(2 * LONG_RUNNER_TURNS), "every task and reply is a delivered turn: {snapshot:?}");
    assert_no_old_messages(&fleet).await;
}

// A long runner's governor and aide sit in separate vessels: `crew list` shows the governor the
// aide with its vessel, beside its own crew. (The brief's rendering of the same members is
// covered in flotilla-agent; the dummy environment holds the written brief out of reach.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crew_sees_the_convoys_other_vessels_crew() {
    let fleet = TestDaemon::builder().agent("turn_delay_ms = 600000").start().await;
    let name = fleet.dispatch(CrewMix::LongRunner, DUMMY_FULFILMENT_KIND).await;
    let session = fleet
        .wait("the governor's session", |session: &ResourceObject<TerminalSession>| {
            session.spec.role == "governor" && session.metadata.labels.get("flotilla.work/convoy") == Some(&name)
        })
        .await;
    let flotilla_resources::TerminalSessionSource::Agent { context: session_context, .. } = session.spec.source else { panic!("agent") };
    let vessels = fleet.get::<Convoy>(&name).await.expect("convoy").status.expect("status").workflow_snapshot.expect("workflow").vessels;
    let aide = vessels.iter().find(|vessel| vessel.crew.iter().any(|crew| crew.role == "aide")).expect("the aide's vessel").name.clone();
    let governor = vessels.iter().find(|vessel| vessel.crew.iter().any(|crew| crew.role == "governor")).expect("governor's").name.clone();
    assert_ne!(aide, governor, "the aide sits in another vessel");
    let context = flotilla_protocol::query::CrewCommandContext {
        namespace: Some(NAMESPACE.into()),
        convoy: Some(name.clone()),
        vessel_ref: Some(session_context.vessel_ref),
        role: Some("governor".into()),
        ..Default::default()
    };
    let CommandValue::CrewList(list) = fleet.execute(CommandAction::QueryCrewList { context }).await else { panic!("a crew list") };
    let members = list.members.iter().map(|member| (member.role.as_str(), member.vessel.as_deref())).collect::<Vec<_>>();
    assert_eq!(members, [("governor", None), ("aide", Some(aide.as_str()))]);
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
        vessel: "coder".into(),
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
    let crew_failed = |status: &ConvoyStatus| status.crew_work["coder"]["coder"].phase == CrewWorkPhase::Failed;
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
// A zero-task long runner finishes first; its aide then hands back work, which
// reopens the recipient; it delivers its convoy after handling that turn.
#[tokio::test(start_paused = true)]
async fn a_finished_long_runner_handles_a_late_handoff_and_lands() {
    let fleet = TestDaemon::builder().start().await;
    // Isolate the legacy same-vessel hand-back path. Normal peer tasks retain
    // their separate responsibilities on distinct vessels in the load catalog:
    // with the aide's needs equal to the governor's, allocation puts both in the
    // governor's vessel.
    let mut same_vessel = CrewMix::LongRunner.workflow(0);
    let needs = same_vessel.roles[0].needs.clone();
    same_vessel.roles[1].needs = needs;
    fleet.apply::<WorkflowTemplate>(CrewMix::LongRunner.workflow_name(), same_vessel).await;
    let name = fleet.dispatch(CrewMix::LongRunner, DUMMY_FULFILMENT_KIND).await;
    let ready = |status: &ConvoyStatus| {
        status.crew_work.get("governor").is_some_and(|crew| {
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
            reassign: Vec::new(),
        })
        .await;
    assert_eq!(handed_off, CommandValue::Ok);
    // The artificial sender is HandedBack, a terminal phase excluded from
    // derived Done. Keep this legacy handoff fixture's explicit operator override.
    let completed = fleet.execute(CommandAction::CrewComplete { context, message: None, disposition: None }).await;
    assert_eq!(completed, CommandValue::Ok);
    let landed = fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    assert_eq!(fleet.counts().turns_received, before.turns_received + 1, "the reopened governor receives the handoff turn");
    assert_eq!(
        landed.status.expect("status").crew_work["governor"]["governor"].completion_authority,
        flotilla_resources::WorkCompletionAuthority::Promises
    );
    assert_no_old_messages(&fleet).await;
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

/// On the engine path every Message is the engine's; the old store holds none.
async fn assert_no_old_messages(fleet: &TestDaemon) {
    let old = fleet.old_store_list::<flotilla_resources::Message>().await;
    assert!(old.is_empty(), "the old store holds Messages: {old:?}");
}

// Behaviour: with no FleetDesignation the controllers keep running and explain names the missing
// record; once it is recorded the waiting convoy delivers without a restart. On the engine the
// Host keeps only conditions that can block, so health's diagnostic naming it is not recorded there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_fleet_designation_waits_and_delivery_resumes_when_it_arrives() {
    let fleet = TestDaemon::builder().start().await;
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
async fn an_operators_message_reaches_the_crew_from_the_projects_operator() {
    operator_turn_scenario(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_operators_ruling_reaches_the_crew_from_the_projects_operator() {
    operator_turn_scenario(false).await;
}

async fn operator_turn_scenario(noted: bool) {
    let fleet = TestDaemon::builder().agent("stall_rate = 1.0").start().await;
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
async fn a_standing_ensure_readmits_after_abandon() {
    use flotilla_resources::ConvoyEnsure;

    let clock = Arc::new(SkippingClock::default());
    let fleet = TestDaemon::builder().clock(clock.clone() as Arc<dyn flotilla_resources::Clock>).start().await;
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
    // Project operations' stamps, as they materialise an ensure: on the engine a definition keeps
    // only the stamps of its origin, so a source commit alone reads back as an operator's.
    let source = repository.key().to_string();
    apply_annotated::<ConvoyEnsure>(
        &fleet,
        "engine-standing",
        &[
            (flotilla_charter::ops_entry::MATERIALIZED_PROJECT_ANNOTATION, "fleet"),
            (flotilla_charter::ops_entry::SOURCE_REPOSITORY_ANNOTATION, source.as_str()),
            (flotilla_charter::ops_entry::SOURCE_COMMIT_ANNOTATION, "fixture"),
        ],
        ensure,
    )
    .await;
    let first = fleet
        .wait("a first generation", |convoy: &ResourceObject<Convoy>| {
            convoy.metadata.annotations.get(flotilla_resources::ENSURED_FROM_ANNOTATION).map(String::as_str) == Some("engine-standing")
        })
        .await
        .metadata
        .name;
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
        if let Some(next) = generations_of(&fleet, "engine-standing").await.into_iter().find(|next| next.metadata.name != first) {
            break next.metadata.name;
        }
        let role = standing_role(&fleet, "engine-standing").await;
        assert!(tokio::time::Instant::now() < deadline, "Ensure admits a fresh generation after abandon: {role:?}");
        // The ended generation waits out its backoff; skip just that.
        skip_to_retry(&clock, role.and_then(|role| role.next_attempt));
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

/// The standing role ensure `ensure` declares, as an operator's standing roles query shows it.
async fn standing_role(fleet: &TestDaemon, ensure: &str) -> Option<flotilla_protocol::StandingRoleRow> {
    let set = fleet.wait_query(flotilla_protocol::QueryId::StandingRoles { scope: None }, |_| true).await;
    set.rows.as_standing_roles()?.iter().find(|row| row.resource.name == ensure).cloned()
}

/// The generations of the standing role ensure `ensure` declares.
async fn generations_of(fleet: &TestDaemon, ensure: &str) -> Vec<ResourceObject<Convoy>> {
    fleet
        .every::<Convoy>()
        .await
        .into_iter()
        .filter(|convoy| convoy.metadata.annotations.get(flotilla_resources::ENSURED_FROM_ANNOTATION).map(String::as_str) == Some(ensure))
        .collect()
}

fn skip_to_retry(clock: &SkippingClock, retry_at: Option<chrono::DateTime<chrono::Utc>>) {
    let now = flotilla_resources::Clock::now(clock);
    if let Some(retry_at) = retry_at.filter(|retry_at| *retry_at > now) {
        clock.skip(retry_at - now + chrono::Duration::seconds(1));
    }
}

// A fixed admitted identity makes the dummy provider's seeded failure draw
// reproducible while the runtime still bootstraps and reconciles the convoy.
/// Requests a convoy of `workflow` with the role `role` in the Fleet project, as `flotilla
/// convoy create --project fleet` does, and returns the record name admission gave it.
async fn create_through_admission(fleet: &TestDaemon, role: &str, workflow: &str) -> String {
    let created = fleet.execute(catalog::dispatch(role, workflow, DUMMY_FULFILMENT_KIND)).await;
    assert!(matches!(created, CommandValue::ConvoyStarted { .. }), "create {role}: {created:?}");
    convoy_named(fleet, role).await
}

/// The record name of the convoy whose role is `role`.
async fn convoy_named(fleet: &TestDaemon, role: &str) -> String {
    fleet
        .every::<Convoy>()
        .await
        .into_iter()
        .find(|convoy| convoy.spec.role == role)
        .unwrap_or_else(|| panic!("no convoy {role}"))
        .metadata
        .name
}

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
async fn crew_supervision_resumes_a_stalled_coder() {
    supervised_coder_scenario(CrewSupervisionAction::Resume).await;
}

// A failing crew proposes failure; its named governor must fail it.
#[tokio::test(start_paused = true)]
async fn crew_supervision_fails_a_failing_coder() {
    supervised_coder_scenario(CrewSupervisionAction::Fail).await;
}

async fn supervised_coder_scenario(action: CrewSupervisionAction) {
    use flotilla_environment::environment::dummy::dummy_draw;
    use flotilla_resources::SupervisionTarget;

    let (domain, agent) =
        if action == CrewSupervisionAction::Resume { ("crew-stall", "stall_rate = 0.5\n") } else { ("crew-failure", "fail_rate = 0.5\n") };
    // Explicitly cover a faulting coder and a healthy supervisor: the dummy crews draw on the
    // convoy's role, which the test chooses, not on the record name admission generates.
    let role = (0..128)
        .map(|n| format!("engine-supervised-{n}"))
        .find(|role| {
            dummy_draw(0, domain, &format!("{role}/coder/coder")) < 0.5
                && dummy_draw(0, domain, &format!("{role}/governor/governor")) >= 0.5
        })
        .expect("seeded case");
    let fleet = TestDaemon::builder().agent(agent).start().await;
    let mut workflow = CrewMix::CoderSupervisor.workflow(LONG_RUNNER_TURNS);
    workflow.supervision =
        Some(vec![SupervisionTarget::ConvoyCrew { vessel: "governor".into(), role: "governor".into() }, SupervisionTarget::Operator]);
    fleet.apply::<WorkflowTemplate>("engine-supervised", workflow).await;
    let name = create_through_admission(&fleet, &role, "engine-supervised").await;
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

// Behaviour: the load fleet's standing governor, kept by its ConvoyEnsure, receives a stalled
// coder's supervision turn through the default project supervision, and resumes it with the
// command the turn gives; the coder's convoy lands with no operator ruling.
#[tokio::test(start_paused = true)]
async fn the_standing_governor_resumes_a_stalled_coder() {
    use flotilla_environment::environment::dummy::dummy_draw;

    // The governor's own draw sets the stall rate, so it never stalls; the role is one whose
    // coder does.
    let rate = dummy_draw(0, "crew-stall", &format!("{0}/{0}/{0}", catalog::GOVERNOR_ROLE));
    let role = (0..256)
        .map(|n| format!("governed-{n}"))
        .find(|role| dummy_draw(0, "crew-stall", &format!("{role}/coder/coder")) < rate)
        .expect("seeded case");
    let fleet = TestDaemon::builder().agent(&format!("stall_rate = {rate}\n")).start().await;
    for catalog::CatalogDocument { kind, name, document } in
        catalog::standing_governor_documents(&catalog_forge_url(&fleet).await).expect("governor documents")
    {
        let applied = fleet.execute(CommandAction::ResourceApply { namespace: NAMESPACE.into(), document }).await;
        assert!(matches!(applied, CommandValue::ResourceObject(_)), "apply {kind} {name}: {applied:?}");
    }
    let governor = fleet
        .wait("the standing governor's generation", |convoy: &ResourceObject<Convoy>| {
            convoy.metadata.annotations.get(flotilla_resources::ENSURED_FROM_ANNOTATION).map(String::as_str) == Some(catalog::GOVERNOR_ROLE)
        })
        .await
        .metadata
        .name;
    fleet.wait("the governor Active", in_phase(&governor, ConvoyPhase::Active)).await;
    let name = create_through_admission(&fleet, &role, CrewMix::Coder.workflow_name()).await;
    let governed = |status: &ConvoyStatus| {
        status.stalls.iter().filter_map(|stall| stall.supervisor.as_ref()).any(|supervisor| supervisor.convoy == governor)
    };
    fleet.wait("a stall the governor supervises", convoy(&name, governed)).await;
    fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    assert_eq!(fleet.counts().crews_stalled, 1, "only the coder stalled");
    assert_eq!(fleet.counts().command_errors, 0);
}

// A daemon restart restores its active convoy while the crew process survives.
// Its next submissions deliver the convoy without re-admission or a duplicate
// process launch.
#[tokio::test(start_paused = true)]
async fn a_daemon_restart_restores_an_active_convoy_and_lands() {
    let fleet = TestDaemon::builder().agent("turn_delay_ms = 10000").settings("host_name = \"restart-proof\"\n").start().await;
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

// A daemon restart in the middle of a long runner's exchange, with the crews' processes
// surviving as a real harness's do, keeps one holder of each role: the crews' turns to each other
// deliver after it without a refused command.
#[tokio::test(start_paused = true)]
async fn a_long_runner_exchange_survives_a_daemon_restart() {
    let fleet = TestDaemon::builder().settings("host_name = \"restart-proof\"\n").start().await;
    let name = fleet.dispatch(CrewMix::LongRunner, DUMMY_FULFILMENT_KIND).await;
    fleet
        .wait("both crews launched and exchanging turns", |found: &ResourceObject<TerminalSession>| {
            found.spec.role == "aide" && fleet.counts().sessions_launched == 2 && fleet.counts().turns_received >= 2
        })
        .await;
    let fleet = fleet.restart().await;
    fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    let counts = fleet.counts();
    assert_eq!(counts.command_errors, 0, "{counts:?}: {:?}", fleet.command_error_samples());
}

// A daemon restart between a coder's change request and its reviewer's answer, with the crews'
// processes surviving, acts on each turn once: one change request, one launch of each crew, and
// the convoy lands.
#[tokio::test(start_paused = true)]
async fn a_restart_mid_review_acts_on_each_turn_once_and_lands() {
    let fleet = TestDaemon::builder().agent("turn_delay_ms = 10000").settings("host_name = \"restart-proof\"\n").start().await;
    let name = fleet.dispatch(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await;
    mid_review(&fleet, &name).await;
    let fleet = fleet.restart().await;
    acted_on_each_turn_once(&fleet, &name).await;
}

// As above, but the daemon's process ends, and the dummy fleet with it: the next process's fleet
// restores the vendor's backing, the sessions and their crews, and the forge from its state file,
// as a real host's Docker, cleat and forge outlive flotillad. The crews resume where they were.
#[tokio::test(start_paused = true)]
async fn a_process_restart_mid_review_acts_on_each_turn_once_and_lands() {
    let fleet = TestDaemon::builder().agent("turn_delay_ms = 10000").settings("host_name = \"restart-proof\"\n").start().await;
    let name = fleet.dispatch(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await;
    mid_review(&fleet, &name).await;
    let fleet = fleet.restart_process().await;
    acted_on_each_turn_once(&fleet, &name).await;
    assert_eq!(fleet.counts().environments_provisioned, 1, "the vendor's backing is adopted, not replaced");
}

// Behaviour: a crew relaunched into a role that has kept promises is told what its role
// delivered, and does not deliver it again. The coder's process ends after its change request is
// kept, while it waits on its review; its relaunch, briefed with the kept change request, opens
// no second one and asks for no second review, and the convoy lands on the reviewer's answer.
// The dummy forge opens one change request per branch, so the review turns tell the two apart.
#[tokio::test(start_paused = true)]
async fn a_coder_relaunched_after_its_change_request_is_kept_does_not_deliver_it_again() {
    use flotilla_resources::promises::{PromiseKind, PromiseState};

    let fleet = TestDaemon::builder().agent("turn_delay_ms = 10000").start().await;
    let name = fleet.dispatch(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await;
    let kept = async {
        loop {
            let prs = promises(&fleet, &name, "coder", "coder").await;
            if prs.iter().any(|promise| promise.kind == PromiseKind::Pr && promise.state == PromiseState::Kept) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(600), kept).await.expect("the coder's change request is kept");
    assert_eq!(fleet.counts().crews_completed, 0, "the review is not yet answered: {:?}", fleet.counts());
    assert!(fleet.end_crew(&name, "coder"), "the coder's process ends");
    fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    let counts = fleet.counts();
    assert_eq!(
        (counts.change_requests_opened, counts.sessions_launched, counts.turns_received),
        (1, 3, 2),
        "one change request, the coder relaunched once, one review request and its answer: {counts:?}"
    );
    assert_eq!(counts.command_errors, 0, "{counts:?}: {:?}", fleet.command_error_samples());
}

async fn mid_review(fleet: &TestDaemon, name: &str) {
    fleet
        .wait("the coder's change request, before the review is answered", |found: &ResourceObject<Convoy>| {
            let counts = fleet.counts();
            found.metadata.name == name
                && counts.change_requests_opened == 1
                && counts.sessions_launched == 2
                && counts.crews_completed == 0
        })
        .await;
}

async fn acted_on_each_turn_once(fleet: &TestDaemon, name: &str) {
    fleet.wait("Landed", in_phase(name, ConvoyPhase::Landed)).await;
    let counts = fleet.counts();
    assert_eq!((counts.change_requests_opened, counts.sessions_launched, counts.turns_received), (1, 2, 2), "{counts:?}");
    assert_eq!(counts.command_errors, 0, "{counts:?}: {:?}", fleet.command_error_samples());
    let prs = promises(fleet, name, "coder", "coder").await;
    let kept = prs.iter().filter(|p| p.kind == flotilla_resources::promises::PromiseKind::Pr).collect::<Vec<_>>();
    assert_eq!(kept.len(), 1, "one change-request promise: {kept:?}");
}

/// Wall time moved forward by however much a test skips, so retry backoff passes at once.
#[derive(Default)]
struct SkippingClock(std::sync::Mutex<chrono::Duration>, tokio::sync::watch::Sender<()>);

impl SkippingClock {
    fn skip(&self, by: chrono::Duration) {
        *self.0.lock().expect("clock") += by;
        self.1.send_replace(());
    }
}

impl flotilla_resources::Clock for SkippingClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + *self.0.lock().expect("clock")
    }

    fn jumps(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.1.subscribe())
    }
}

// Behaviour (#3824 layer 8b): a standing convoy whose
// generation fails three times is held, and the hold is an attention from the standing
// role in its project's operator inbox. The person sees it in `message inbox`;
// acknowledging it releases the hold, and the next generation is admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledging_a_standing_convoys_hold_admits_its_next_generation() {
    use flotilla_charter::ops_entry::{
        MATERIALIZED_PROJECT_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION,
    };
    use flotilla_protocol::StandingRoleHold;
    use flotilla_resources::{ConvoyEnsure, ConvoyEnsureSpec, Project, ProjectRepositoryRole, ProjectRepositorySpec};

    let clock = Arc::new(SkippingClock::default());
    let fleet = TestDaemon::builder().clock(clock.clone() as Arc<dyn flotilla_resources::Clock>).start().await;
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
    // Each generation fails before provisioning, as the vendor refuses it: a strike. An operator's
    // abandon would not be one (decision 78(3)).
    fleet.vendor().refuse_provisioning(true);
    apply_annotated::<ConvoyEnsure>(&fleet, "quartermaster", &annotations, ensure).await;

    let generations = || generations_of(&fleet, "quartermaster");
    let role = || standing_role(&fleet, "quartermaster");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !role().await.is_some_and(|role| role.hold == Some(StandingRoleHold::RestartLimit)) {
        let latest = generations().await.into_iter().max_by_key(|convoy| convoy.spec.generation);
        assert!(
            tokio::time::Instant::now() < deadline,
            "never held: {:?}, role {:?}",
            latest.as_ref().map(|convoy| (convoy.metadata.name.clone(), convoy.status.as_ref().map(|status| status.phase))),
            role().await
        );
        // Each ended generation waits out its backoff; skip just that.
        skip_to_retry(&clock, role().await.and_then(|role| role.next_attempt));
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(generations().await.len(), 3, "three failed generations, then the hold");

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

    fleet.vendor().refuse_provisioning(false);
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
async fn a_stall_past_every_supervisor_reaches_the_projects_operator_and_the_person() {
    use flotilla_resources::SupervisionTarget;

    let fleet = TestDaemon::builder().agent("stall_rate = 1.0").start().await;
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

    fleet.vendor().refuse_provisioning(false);
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
async fn a_lone_coder_realises_its_resources_and_delivers() {
    let fleet = TestDaemon::builder().start().await;
    let (_, landed) = deliver(&fleet, CrewMix::Coder).await;
    assert_eq!((landed.environments_provisioned, landed.sessions_launched, landed.crews_completed), (1, 1, 1), "{landed:?}");
    fleet.wait("the environment torn down", |_: &ResourceObject<Convoy>| fleet.counts().live_environments == 0).await;
    assert_eq!(fleet.counts().environments_destroyed, 1);
}

// An environment that cannot realise fails its convoy and launches no terminal session.
#[tokio::test(start_paused = true)]
async fn an_unrealised_environment_records_failure_without_a_terminal() {
    let fleet = TestDaemon::builder().environment("failure_rate = 1.0").start().await;
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
            namespace: NAMESPACE.into(),
            kind: T::API_PATHS.kind.into(),
            name: name.into(),
            status: serde_json::to_value(status).expect("status"),
        })
        .await;
    assert!(matches!(patched, CommandValue::ResourceObject(_)), "patch {} {name}: {patched:?}", T::API_PATHS.kind);
}

// Behaviour (#3824 layer 8b): the input-needed and declaration-refused Demands are gone; their
// facts are read from the records that hold them. A crew session that
// needs input raises its vessel's salience, and a refused declaration is listed by `fleet list`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_needed_and_declaration_refusals_are_read_from_their_records() {
    use flotilla_protocol::{
        AgentEventType, AgentHarness, AgentHookEvent, AgentHookTerminalRef, AttachableId, AwarenessGrouping, AwarenessKind,
        DeclarationAttentionKind, QueryId, ResultSet, Salience,
    };
    use flotilla_resources::{DeclarationRefusedCondition, Project, ProjectSpec, ProjectStatusPatch, TerminalAttentionState, CONVOY_LABEL};

    let fleet = TestDaemon::builder().agent("stall_rate = 1.0").start().await;
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
    // The harness asks for permission: its hook records NeedsInput on the crew's AgentSession.
    let terminal = AgentHookTerminalRef { namespace: session.metadata.namespace.clone(), session_name: session.metadata.name.clone() };
    fleet
        .agent_hook(
            AgentHookEvent::builder()
                .attachable_id(AttachableId::new(format!("terminal:{}/{}", terminal.namespace, terminal.session_name)))
                .harness(AgentHarness::ClaudeCode)
                .hook_event("PermissionRequest".into())
                .event_type(AgentEventType::WaitingForPermission)
                .terminal(terminal)
                .build(),
        )
        .await;
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

// Each kind the stalled crew and the fixtures make. Clone and Environment are home-local by
// design (ReplicationClass::None): checked to stay home. Not here, as no stalled dummy crew makes
// them: AccountBinding and AccountProvision (written only by the account controllers),
// ManifestRoot (only by a home's manifest reconciler), and Checkout and ImageBuild, whose engine
// records the resource commands do not yet read. Message's log following is engine_only's
// `messages_reach_a_peer_by_log_following`; a build demanded at another home is provisioning's
// `a_build_demanded_at_another_home_is_built_and_read_at_its_builder`.
replication_kinds!(
    AccessDelivery,
    Artifact,
    ChangeRequest,
    Clone,
    Convoy,
    ConvoyEnsure,
    CredentialGrant,
    CredentialSpec,
    CrewDefaults,
    Environment,
    FleetDesignation,
    Forge,
    FulfilmentKind,
    Host,
    ImageLayer,
    Issue,
    Project,
    Repository,
    TerminalSession,
    Vessel,
    WorkflowTemplate,
);

/// One record of each kind the stalled convoy does not make that an operator
/// can apply; the controllers then keep them as they keep any. The account
/// controllers and a home's manifest reconciler alone write AccountBinding,
/// AccountProvision's binding and ManifestRoot, so none is applied here. An
/// ImageBuild is a demand admission writes, not a record applied here.
async fn apply_transport_fixtures(fleet: &TestDaemon, host: &str) {
    use flotilla_resources::{
        AccessDelivery, AccessDeliverySpec, ChangeRequest, ConvoyEnsure, CredentialGrant, CredentialSpec, CrewDefaults, ImageLayer, Issue,
    };
    let local = fleet.list::<Host>().await.into_iter().next().expect("local host").metadata.name;
    let place = flotilla_resources::host_direct_environment_name(&local);
    let credential = format!("{host}-credential");
    let spec = serde_json::json!({
        "consumer": {"adapter": "claude-oauth", "account_email": format!("{host}@example.invalid")},
        "lifecycle": "static", "source": {"kind": "file", "path": "/transport/token"}
    });
    apply_json::<CredentialSpec>(fleet, &credential, spec).await;
    let grant = serde_json::json!({"credentials": [credential], "selector": {"projects": ["fleet"]}});
    apply_json::<CredentialGrant>(fleet, &format!("{host}-grant"), grant).await;
    apply_json::<CrewDefaults>(fleet, &format!("{host}-defaults"), serde_json::json!({})).await;
    apply_json::<ImageLayer>(fleet, &format!("{host}-layer"), builtin::<ImageLayer>()).await;
    apply_json::<WorkflowTemplate>(fleet, &format!("{host}-template"), builtin::<WorkflowTemplate>()).await;
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
    // A change request is written under its subject's record name, the one readers get it by.
    let change_request = flotilla_resources::address::stored_change_request_address("https://github.com", &format!("{host}/app"), 1, &[])
        .ok()
        .and_then(|address| flotilla_resources::address::record_name(&address))
        .expect("change request record name");
    apply_json::<ChangeRequest>(fleet, &change_request, observed(1)).await;
    apply_json::<Issue>(fleet, &format!("{host}-issue"), observed(2)).await;
    let ensure = serde_json::json!({"project_ref": "fleet", "role": format!("{host}-standing"), "workflow_ref": CrewMix::Coder.workflow_name(), "repositories": []});
    apply_json::<ConvoyEnsure>(fleet, &format!("{host}-ensure"), ensure).await;
}

// Both peer runtimes consume the real change stream. What each host made is
// listed at the other, as `flotilla get` shows it there, and home-local kinds
// stay home.
#[tokio::test(start_paused = true)]
async fn two_daemons_replicate_every_kind() {
    // Replication is under test, not missed events: the production resync
    // keeps the paused clock's controller passes, which cost real time, few.
    let resync = RuntimeOptions::default().controller_resync_interval;
    let (a, b) = tokio::join!(
        TestDaemon::builder().agent("stall_rate = 1.0").settings("host_name = \"engine-a\"\n").resync(resync).start(),
        TestDaemon::builder().agent("stall_rate = 1.0").settings("host_name = \"engine-b\"\n").resync(resync).start()
    );
    // A stalled crew keeps its vessel, environment, checkout, session and messages.
    let (stalled_a, stalled_b) =
        tokio::join!(a.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND), b.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND));
    tokio::join!(a.wait("a stall", stalled(&stalled_a)), b.wait("a stall", stalled(&stalled_b)));
    apply_transport_fixtures(&a, "engine-a").await;
    apply_transport_fixtures(&b, "engine-b").await;
    let (before_a, before_b) = tokio::join!(inventories(&a), inventories(&b));
    let unmade: Vec<_> = before_a.iter().filter(|(_, records)| records.is_empty()).map(|(kind, _)| *kind).collect();
    assert!(unmade.is_empty(), "every kind has a record to replicate: {unmade:?}");
    let _peering = TestDaemon::peer(&a, &b).await;
    assert_every_kind_replicated(&before_a, &before_b, &b).await;
    assert_every_kind_replicated(&before_b, &before_a, &a).await;
}
