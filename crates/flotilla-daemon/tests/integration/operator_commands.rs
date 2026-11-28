//! Operator commands on a daemon composed as `flotillad` composes one, ported from the
//! retired `flotilla-controllers` in-process suite: each scenario here proves a behaviour no
//! other suite covers, driven only as an operator.

use std::time::Duration;

use flotilla_daemon_testkit::daemon::{TestDaemon, NAMESPACE};
use flotilla_dummy_fleet::catalog::{self, CrewMix, DUMMY_FULFILMENT_KIND};
use flotilla_protocol::{commands::CommandValue, CommandAction, ConvoyAutoAttach};
use flotilla_resources::{
    Convoy, ConvoyPhase, ConvoyStatus, CrewWorkPhase, Repository, RepositoryRelation, ResourceObject, TerminalSession,
};

fn in_phase(name: &str, phase: ConvoyPhase) -> impl Fn(&ResourceObject<Convoy>) -> bool + '_ {
    move |convoy| convoy.metadata.name == name && convoy.status.as_ref().is_some_and(|status: &ConvoyStatus| status.phase == phase)
}

/// The start command of a convoy of `mix` onto the dummy kind, for a test to change before it runs.
fn start_of(role: &str, mix: CrewMix) -> Box<flotilla_protocol::ConvoyStartIntent> {
    let CommandAction::ConvoyStart { intent } = catalog::dispatch(role, mix.workflow_name(), DUMMY_FULFILMENT_KIND) else {
        panic!("dispatch starts a convoy");
    };
    intent
}

fn error_message(value: &CommandValue) -> &str {
    match value {
        CommandValue::Error { message } => message,
        other => panic!("expected an error, got {other:?}"),
    }
}

// A need that names the platform matrix belongs to a workflow's roles or a Project's role needs;
// an ad hoc one on the start command is refused before anything is placed.
#[tokio::test(start_paused = true)]
async fn an_ad_hoc_matrix_need_is_refused_before_placement() {
    let fleet = TestDaemon::builder().start().await;
    let mut intent = start_of("ad-hoc-matrix", CrewMix::Coder);
    intent.needs.push("platform:$matrix".to_string());
    let refused = fleet.execute(CommandAction::ConvoyStart { intent }).await;
    assert!(error_message(&refused).contains("platform:$matrix is only valid on workflow roles or Project role needs"), "{refused:?}");
    assert!(fleet.list::<Convoy>().await.is_empty(), "nothing was admitted");
}

// A repository declared as a fork refuses a workflow with no review inside the crew, until its
// record explicitly allows reviewless workflows; a workflow that reviews in its crew is admitted.
#[tokio::test(start_paused = true)]
async fn a_fork_stance_repository_refuses_reviewless_dispatch_and_admits_a_reviewed_one() {
    let fleet = TestDaemon::builder().start().await;
    let forge = catalog::forge(&fleet.get::<flotilla_resources::Forge>(catalog::DUMMY_FORGE_ID).await.expect("dummy forge").spec.https_url);
    let repository = catalog::repository(&forge).expect("repository");
    let fork = repository.with_upstream("https://github.com/upstream-org/dummy", RepositoryRelation::Fork).expect("upstream");
    fleet.apply::<Repository>(&fork.key().to_string(), fork.clone()).await;

    let refused = fleet.execute(CommandAction::ConvoyStart { intent: start_of("reviewless", CrewMix::Coder) }).await;
    assert_eq!(
        error_message(&refused),
        format!("workflow {} not permitted for fork-stance repository — use implement-review", CrewMix::Coder.workflow_name())
    );

    // A workflow whose one vessel crews a coder with a code reviewer reviews inside its crew.
    let mut reviewed = CrewMix::CoderReviewer.workflow(2);
    let mut crew = std::mem::take(&mut reviewed.roles);
    for role in &mut crew {
        if let flotilla_resources::CrewSource::Agent { selector, .. } = &mut role.source {
            if role.role == "reviewer" {
                selector.capability = "code-review".into();
            }
        }
    }
    reviewed.vessels = vec![flotilla_resources::VesselRequirement::builder().name("implement".into()).crew(crew).build()];
    fleet.apply::<flotilla_resources::WorkflowTemplate>("in-crew-review", reviewed).await;
    let mut intent = start_of("reviewed", CrewMix::CoderReviewer);
    intent.workflow_ref = Some("in-crew-review".into());
    let admitted = fleet.execute(CommandAction::ConvoyStart { intent }).await;
    assert!(matches!(admitted, CommandValue::ConvoyStarted { .. }), "{admitted:?}");

    fleet.apply::<Repository>(&fork.key().to_string(), fork.with_allow_reviewless_workflows(true)).await;
    let overridden = fleet.execute(CommandAction::ConvoyStart { intent: start_of("overridden", CrewMix::Coder) }).await;
    assert!(matches!(overridden, CommandValue::ConvoyStarted { .. }), "an explicit override admits it: {overridden:?}");
}

// A start that waits to attach to its work learns at once that the work failed, rather than
// waiting out the attach timeout.
#[tokio::test(start_paused = true)]
async fn a_start_reports_failed_work_without_waiting_for_the_attach_timeout() {
    let fleet = TestDaemon::builder().environment("failure_rate = 1.0").start().await;
    let mut intent = start_of("bootstrap-failure", CrewMix::Coder);
    intent.auto_attach = ConvoyAutoAttach::Always;
    let reported = tokio::time::timeout(Duration::from_secs(20), fleet.execute(CommandAction::ConvoyStart { intent }))
        .await
        .expect("a failed convoy stops the attach wait promptly");
    assert!(matches!(reported, CommandValue::Error { .. }), "{reported:?}");
}

// A landed convoy has nothing left to end: deleting it succeeds and the convoy is kept, landed.
#[tokio::test(start_paused = true)]
async fn deleting_a_landed_convoy_succeeds_and_keeps_it_landed() {
    let fleet = TestDaemon::builder().start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    fleet.wait("Landed", in_phase(&name, ConvoyPhase::Landed)).await;
    let deleted = fleet.execute(CommandAction::ConvoyDelete { namespace: Some(NAMESPACE.into()), name: name.clone(), force: false }).await;
    assert_eq!(deleted, CommandValue::Ok);
    let kept = fleet.every::<Convoy>().await.into_iter().find(|convoy| convoy.metadata.name == name).expect("the landed convoy is kept");
    assert_eq!(kept.status.as_ref().map(|status| status.phase), Some(ConvoyPhase::Landed));
}

// An operator removes a wrong declared remote from a repository without deleting its record.
#[tokio::test(start_paused = true)]
async fn an_operator_removes_a_wrong_repository_remote_and_the_record_remains() {
    let fleet = TestDaemon::builder().start().await;
    let stable = "https://github.com/flotilla-org/flotilla";
    let wrong = "https://github.com/wrong/flotilla";
    let spec =
        flotilla_resources::RepositorySpec::remote(stable).expect("repository").with_declared_remotes([wrong, stable]).expect("remotes");
    let name = spec.key().to_string();
    fleet.apply::<Repository>(&name, spec).await;

    let removed = fleet
        .execute(CommandAction::RepositoryRemoteRemove { namespace: NAMESPACE.into(), name: name.clone(), remote: wrong.to_string() })
        .await;
    assert_eq!(removed, CommandValue::Ok);
    let repository = fleet.get::<Repository>(&name).await.expect("the repository remains");
    assert_eq!(repository.spec.remotes(), [stable]);
}

// A delete of a name this home does not hold, or no longer holds, is NotFound.
#[tokio::test(start_paused = true)]
async fn deleting_a_name_this_home_does_not_hold_is_not_found() {
    let fleet = TestDaemon::builder().start().await;
    let spec = flotilla_resources::RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository");
    let name = spec.key().to_string();
    fleet.apply::<Repository>(&name, spec).await;
    let delete = |name: &str| CommandAction::ResourceDelete {
        namespace: NAMESPACE.into(),
        kind: "repositories".into(),
        name: name.into(),
        replica_origin: None,
    };
    let deleted = fleet.execute(delete(&name)).await;
    assert!(matches!(deleted, CommandValue::ResourceDeleted(_)), "a held record deletes: {deleted:?}");
    for name in ["never-held", name.as_str()] {
        let refused = fleet.execute(delete(name)).await;
        assert!(
            error_message(&refused).contains("not found"),
            "deleting {name}, which this home does not hold, must be NotFound: {refused:?}"
        );
    }
}

// An operator completes a crew that submitted nothing: the convoy lands as the operator's verdict
// keeps each of the crew's promises with nothing submitted.
#[tokio::test(start_paused = true)]
async fn an_operators_forced_completion_settles_a_crew_that_submitted_nothing() {
    let fleet = TestDaemon::builder().agent("stall_rate = 1.0").start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    fleet
        .wait("a supervisable stall", |convoy: &ResourceObject<Convoy>| {
            convoy.metadata.name == name && convoy.status.as_ref().is_some_and(|status| status.stalled.is_some())
        })
        .await;
    let session = fleet.wait("the coder's session", |session: &ResourceObject<TerminalSession>| session.spec.role == "coder").await;
    let flotilla_resources::TerminalSessionSource::Agent { context, .. } = session.spec.source else { panic!("an agent session") };
    let completed = fleet
        .execute(CommandAction::CrewComplete {
            context: flotilla_protocol::query::CrewCommandContext {
                namespace: Some(NAMESPACE.into()),
                convoy: Some(name.clone()),
                vessel_ref: Some(context.vessel_ref),
                role: Some("coder".into()),
                ..Default::default()
            },
            message: Some("operator accepted the ledger-less claim".into()),
            disposition: None,
        })
        .await;
    assert_eq!(completed, CommandValue::Ok);
    let convoy = fleet
        .wait("Landing or Landed", |convoy: &ResourceObject<Convoy>| {
            convoy.metadata.name == name
                && convoy.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Landing | ConvoyPhase::Landed))
        })
        .await;
    let status = convoy.status.expect("status");
    assert_eq!(status.crew_work["coder"]["coder"].phase, CrewWorkPhase::Done);
}

// A forced delete of an ended convoy whose checkout's work is not shown safe does what it says: the
// convoy goes, or the delete is refused naming the checkout that needs a ruling. It never answers
// ok while the reclaim still keeps the convoy.
#[tokio::test(start_paused = true)]
async fn a_forced_delete_of_an_ended_convoy_names_the_checkout_its_reclaim_waits_on() {
    use flotilla_protocol::commands::CrewSupervisionAction;
    use flotilla_resources::Checkout;
    // Its crew fails, leaving work in its checkout the reclaim keeps.
    let fleet = TestDaemon::builder().agent("fail_rate = 1.0").vcs("dirty_rate = 1.0").start().await;
    let name = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let stalled =
        |convoy: &ResourceObject<Convoy>| convoy.metadata.name == name && convoy.status.as_ref().is_some_and(|s| s.stalled.is_some());
    fleet.wait("a supervisable stall", stalled).await;
    let fail = CommandAction::CrewSupervise {
        namespace: Some(NAMESPACE.into()),
        convoy: name.clone(),
        vessel: "coder".into(),
        role: "coder".into(),
        operation: CrewSupervisionAction::Fail,
        message: "Supervisor decision.".into(),
        actor_crew_id: None,
    };
    assert!(!matches!(fleet.execute(fail).await, CommandValue::Error { .. }));
    fleet.wait("Failed", in_phase(&name, ConvoyPhase::Failed)).await;
    let checkout = fleet
        // A checkout is named for the launch it is held for.
        .wait("its checkout", |checkout: &ResourceObject<Checkout>| checkout.metadata.name.contains(&format!("{name}-")))
        .await
        .metadata
        .name;

    let deleted = fleet.execute(CommandAction::ConvoyDelete { namespace: Some(NAMESPACE.into()), name: name.clone(), force: true }).await;
    match &deleted {
        CommandValue::Error { message } => assert!(message.contains(&checkout), "the refusal names {checkout}: {message}"),
        _ => fleet.wait_gone::<Convoy>(&name).await,
    }
}
