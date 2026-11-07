//! Vessel reach (#4299) through the public daemon harness: each vessel's reach
//! set is composed at admission and frozen; at placement each candidate answers
//! for every destination, on its own host, from the ways its vendor declares;
//! a cloud sandbox reports how it met each one.

use std::collections::BTreeSet;

use flotilla_config::dummy_fleet::DEFAULT_DUMMY_FORGE_SERVICE_URL;
use flotilla_daemon_testkit::daemon::TestDaemon;
use flotilla_dummy_fleet::catalog::{self, CrewMix, DUMMY_CLOUD_FULFILMENT_KIND, DUMMY_FULFILMENT_KIND};
use flotilla_protocol::{commands::CommandValue, CommandAction};
use flotilla_resources::{
    reach::{ReachDestination, ReachMeans},
    Convoy, ConvoyPhase, Environment, EnvironmentPhase, FleetDesignation, FleetDesignationSpec, ImageCacheBinding, Project,
    ProjectRepositorySpec, ProjectSpec, ResourceObject, WorkflowTemplate, FLEET_DESIGNATION_NAME, VESSEL_REF_LABEL,
};

/// The dummy forge: the repository's remote, so in every vessel's reach set.
const FORGE: &str = "forge.dummy.invalid";

fn at(host: &str, port: u16) -> ReachDestination {
    ReachDestination::new(host, port).unwrap()
}

/// Create a convoy of `workflow` as an operator does, on `kind`, in `project`
/// or else on the dummy repository.
async fn create(daemon: &TestDaemon, name: &str, workflow: &str, kind: &str, project: Option<&str>) -> CommandValue {
    let forge = catalog::forge(DEFAULT_DUMMY_FORGE_SERVICE_URL);
    daemon
        .execute(CommandAction::ConvoyCreate {
            name: name.into(),
            workflow_ref: workflow.into(),
            inputs: Vec::new(),
            repository_url: project.is_none().then(|| catalog::repository_url(&forge)),
            r#ref: Some("main".into()),
            project_ref: project.map(Into::into),
            placement_policy: Some(kind.into()),
            adopted_checkout: None,
        })
        .await
}

fn convoy_of_role<'a>(role: &'a str, phase: ConvoyPhase) -> impl Fn(&ResourceObject<Convoy>) -> bool + 'a {
    move |convoy| convoy.spec.role == role && convoy.status.as_ref().is_some_and(|status| status.phase == phase)
}

// A vessel's reach set is its repository's remote and what its template, the
// fleet and its project declare, frozen into the convoy's workflow snapshot. The fleet's
// image cache is not in it: the guest does not pull from the cache. A vendor
// on the host's network meets every destination, so the convoy lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vessels_reach_is_its_remote_and_the_fleet_and_project_declarations() {
    let daemon = TestDaemon::builder().start().await;
    let fleet = ProjectSpec::builder()
        .display_name("Fleet".to_string())
        .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
        .repositories(Vec::new())
        .build();
    daemon.apply::<Project>("fleet", fleet).await;
    let designation = FleetDesignationSpec {
        fleet: Some("lab".into()),
        project: "fleet".into(),
        image_cache: Some(ImageCacheBinding {
            repository: "registry.lab.test:5000/cache".into(),
            pull_credential: "pull".into(),
            push_credential: "push".into(),
        }),
        reach: vec![at("artifacts.lab.test", 443)],
    };
    daemon.apply::<FleetDesignation>(FLEET_DESIGNATION_NAME, designation).await;
    let repository = catalog::repository(&catalog::forge(DEFAULT_DUMMY_FORGE_SERVICE_URL)).unwrap().key();
    let mut project = ProjectSpec::builder()
        .display_name("App".to_string())
        .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
        .repositories(vec![ProjectRepositorySpec::builder().repo(repository).build()])
        .build();
    project.reach = vec![at("Objects.Lab.Test", 9000)];
    daemon.apply::<Project>("app", project).await;
    let mut workflow = CrewMix::Coder.workflow(2);
    workflow.vessels[0].reach = [at("declared.test", 8443)].into();
    daemon.apply::<WorkflowTemplate>("reaching-coder", workflow).await;
    let created = create(&daemon, "composed", "reaching-coder", DUMMY_FULFILMENT_KIND, Some("app")).await;
    assert!(matches!(created, CommandValue::ConvoyCreated { .. }), "{created:?}");
    let landed = daemon.wait("Landed", convoy_of_role("composed", ConvoyPhase::Landed)).await;
    let snapshot = landed.status.and_then(|status| status.workflow_snapshot).expect("a frozen workflow");
    let expected = BTreeSet::from([at(FORGE, 443), at("declared.test", 8443), at("artifacts.lab.test", 443), at("objects.lab.test", 9000)]);
    assert_eq!(snapshot.vessels.iter().map(|vessel| &vessel.reach).collect::<Vec<_>>(), vec![&expected]);
}

// A candidate whose vendor declares no way to reach a destination that
// resolves privately on its host is refused at placement, naming the
// destination; nothing is provisioned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_candidate_that_cannot_meet_a_destination_is_refused_naming_it() {
    let daemon = TestDaemon::builder().environment(&format!("reach_ways = []\nprivate_destinations = [\"{FORGE}\"]")).start().await;
    for kind in [DUMMY_FULFILMENT_KIND, DUMMY_CLOUD_FULFILMENT_KIND] {
        let refused = create(&daemon, &format!("refused-{kind}"), CrewMix::Coder.workflow_name(), kind, None).await;
        let CommandValue::Error { message } = refused else { panic!("{kind} placed: {refused:?}") };
        assert!(message.contains(&format!("{FORGE}:443")) && message.contains("no declared way"), "{kind}: {message}");
    }
    assert_eq!(daemon.counts().environments_provisioned, 0);
}

// A vendor that carries a private destination by an outward exposure places
// and provisions; the cloud sandbox reports how it met each destination,
// which is where a vessel's view reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_carried_destination_places_and_the_sandbox_reports_how_it_is_met() {
    let daemon = TestDaemon::builder()
        .environment(&format!("reach_ways = [\"outward_exposure\"]\nprivate_destinations = [\"{FORGE}\"]"))
        .start()
        .await;
    let convoy = daemon.dispatch(CrewMix::Coder, DUMMY_CLOUD_FULFILMENT_KIND).await;
    let ready = |environment: &ResourceObject<Environment>| {
        environment.metadata.labels.get(VESSEL_REF_LABEL).is_some_and(|vessel| vessel.starts_with(&format!("{convoy}-")))
            && environment.status.as_ref().is_some_and(|status| status.phase == EnvironmentPhase::Ready)
    };
    let environment = daemon.wait("a Ready cloud sandbox", ready).await;
    let routes = environment.status.and_then(|status| status.provider_descriptor).expect("a described sandbox").reach;
    let met = routes.into_iter().map(|route| (route.destination, route.met)).collect::<Vec<_>>();
    assert_eq!(met, vec![(at(FORGE, 443), ReachMeans::OutwardExposure)]);
}
