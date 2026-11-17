//! The engine's real ports keep the engine port contract that the in-memory port keeps for the
//! crates below them: a plain kind, a kind with a status, and a kind the engine holds, read through
//! its lifecycle views. A port that refuses what the in-memory one takes fails here. Host, Project
//! and the forge observations keep its refusal of stale writes.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use flotilla_kind_records_testkit::{
    contract::{self, Samples, Subject},
    profile_of,
    samples::{Artifacts, Environments, Repositories},
    Profile,
};
use flotilla_provisioning::engine::{
    artifacts::EngineArtifacts, hosts::EngineHosts, observations::EngineObservations, projects::EngineProjects,
    repositories::EngineRepositories,
};
use flotilla_provisioning_testkit::{environments::EnvironmentFixture, other_home::OtherHome, EngineFixture};
use flotilla_resources::{
    address::record_name, engine_records::KindRecords, environment_records::LaunchFacts, Artifact, ChangeRequest,
    ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, Clock, DeclarationRefusedCondition, Environment, Host,
    HostConnection, HostSpec, HostStatus, Issue, IssueSpec, IssueStatus, Observation, ObservedChangeRequestState, ObservedIssueState,
    Project, ProjectSpec, ProjectStatus, Repository, Resource, SystemClock,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::LifecycleViews;

fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

/// A decided store at home `alpha`, with the port `open` reads over it.
struct Decided<R: Resource, S> {
    _directory: tempfile::TempDir,
    service: Arc<Service>,
    port: Arc<dyn KindRecords<R>>,
    open: fn(Arc<Service>) -> Arc<dyn KindRecords<R>>,
    samples: S,
    elsewhere: std::sync::Mutex<Vec<OtherHome>>,
}

impl<R: Resource, S> Decided<R, S> {
    fn open(samples: S, open: fn(Arc<Service>) -> Arc<dyn KindRecords<R>>) -> Self {
        let directory = tempfile::tempdir().expect("temp dir");
        let registry = flotilla_provisioning::stored::decided_registry().expect("decided registry");
        let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", registry).expect("store"));
        let port = open(Arc::clone(&service));
        Self { _directory: directory, service, port, open, samples, elsewhere: Default::default() }
    }
}

#[async_trait::async_trait]
impl<R: Resource, S: Samples<R>> Subject<R> for Decided<R, S> {
    fn port(&self) -> Arc<dyn KindRecords<R>> {
        Arc::clone(&self.port)
    }
    fn profile(&self) -> Profile {
        profile_of(R::API_PATHS.kind)
    }
    fn samples(&self) -> &dyn Samples<R> {
        &self.samples
    }
    async fn hold_elsewhere(&self, home: &str, index: usize) -> Option<()> {
        let other = OtherHome::open(home);
        let (name, _) = self.samples.sample(index);
        let theirs = (self.open)(Arc::clone(other.service()));
        theirs
            .apply(&name, None, &Default::default(), &Default::default(), &self.samples.edited(index))
            .await
            .expect("written at its home");
        other.replicate_to(&self.service);
        self.elsewhere.lock().expect("homes").push(other);
        Some(())
    }
}

#[tokio::test]
async fn the_plain_artifact_port_keeps_the_contract() {
    let subject = Decided::<Artifact, _>::open(Artifacts, |service| Arc::new(EngineArtifacts::open(service, clock()).expect("artifacts")));
    contract::check(&subject).await;
}

#[tokio::test]
async fn the_repository_port_keeps_the_contract() {
    let subject =
        Decided::<Repository, _>::open(Repositories, |service| Arc::new(EngineRepositories::open(service, clock()).expect("repositories")));
    contract::check(&subject).await;
}

/// Environments at `alpha`, requested and realised as their controller would, read through the
/// lifecycle views.
struct Held {
    fixture: EnvironmentFixture,
    port: Arc<LifecycleViews>,
}

#[async_trait::async_trait]
impl Subject<Environment> for Held {
    fn port(&self) -> Arc<dyn KindRecords<Environment>> {
        self.port.clone()
    }
    fn profile(&self) -> Profile {
        profile_of(Environment::API_PATHS.kind)
    }
    fn samples(&self) -> &dyn Samples<Environment> {
        &Environments
    }
    async fn hold(&self, index: usize) {
        let (name, spec) = Environments.sample(index);
        self.fixture.request(&name, &spec, None);
    }
    async fn change(&self, index: usize) -> Option<()> {
        self.fixture.ready(&Environments.sample(index).0, "backing", LaunchFacts::default());
        Some(())
    }
}

#[tokio::test]
async fn the_environment_views_keep_the_contract() {
    // Every lifecycle kind's readings over a daemon's decided store, its environments' changes
    // followed from the log, as a daemon composes them.
    let directory = tempfile::tempdir().expect("temp dir");
    let registry = flotilla_provisioning::stored::decided_registry().expect("decided registry");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", registry).expect("store"));
    let engine = EngineFixture::over_service(&service);
    let following = engine.environments.environments.follow(service).expect("follow the environments");
    let port = Arc::new(LifecycleViews::new(engine.reads()));
    contract::check(&Held { fixture: engine.environments, port }).await;
    following.abort();
}

/// A port written through, whose status keeps more than the rest of the contract describes: held to
/// its refusal of stale writes alone.
struct Written<R: Resource, S> {
    _directory: tempfile::TempDir,
    port: Arc<dyn KindRecords<R>>,
    samples: S,
}

impl<R: Resource, S> Written<R, S> {
    fn open(samples: S, open: impl FnOnce(Arc<Service>) -> Arc<dyn KindRecords<R>>) -> Self {
        let directory = tempfile::tempdir().expect("temp dir");
        let registry = flotilla_provisioning::stored::decided_registry().expect("decided registry");
        let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", registry).expect("store"));
        Self { port: open(service), _directory: directory, samples }
    }
}

#[async_trait::async_trait]
impl<R: Resource, S: Samples<R>> Subject<R> for Written<R, S> {
    fn port(&self) -> Arc<dyn KindRecords<R>> {
        Arc::clone(&self.port)
    }
    fn profile(&self) -> Profile {
        Profile { writes: true, status: true, labels: false, annotations: false, status_moves_version: true }
    }
    fn samples(&self) -> &dyn Samples<R> {
        &self.samples
    }
}

fn observed_at() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000, 0).single().expect("time")
}

fn seen<T>(value: Option<T>) -> Observation<T> {
    Observation { value, observed_at: observed_at() }
}

/// SSH hosts `alpha` owns, so this home writes them.
struct Hosts;

fn host(index: usize, display_name: &str) -> HostSpec {
    HostSpec {
        display_name: display_name.into(),
        connection: HostConnection::SshHost { owning_daemon: "alpha".into(), destination: format!("ssh-{index}") },
        ..HostSpec::default()
    }
}

impl Samples<Host> for Hosts {
    fn sample(&self, index: usize) -> (String, HostSpec) {
        (format!("ssh-{index}"), host(index, "first"))
    }
    fn edited(&self, index: usize) -> HostSpec {
        host(index, "second")
    }
    fn status(&self) -> Option<HostStatus> {
        Some(HostStatus { heartbeat_at: Some(observed_at()), ready: true, ..HostStatus::default() })
    }
}

/// Projects an operator declares, with project operations' conditions as their status.
struct Projects;

fn project(display_name: &str) -> ProjectSpec {
    ProjectSpec::builder().display_name(display_name.into()).repositories(Vec::new()).build()
}

impl Samples<Project> for Projects {
    fn sample(&self, index: usize) -> (String, ProjectSpec) {
        (format!("project-{index}"), project("first"))
    }
    fn edited(&self, _index: usize) -> ProjectSpec {
        project("second")
    }
    fn status(&self) -> Option<ProjectStatus> {
        let refused = DeclarationRefusedCondition {
            entry_path: "ops.yaml".into(),
            message: "duplicate".into(),
            since: observed_at(),
            observed_at: observed_at(),
        };
        Some(ProjectStatus { declaration_refused: Some(refused), ..ProjectStatus::default() })
    }
}

/// Change requests this home observes, each named for its subject, which its spec only names.
struct ChangeRequests;

fn change_request(index: usize) -> ChangeRequestSpec {
    ChangeRequestSpec {
        service: "github.com".into(),
        scope: "flotilla-org/flotilla".into(),
        number: index as u64 + 1,
        observing_authority: "alpha".into(),
        subject_of: Vec::new(),
    }
}

impl Samples<ChangeRequest> for ChangeRequests {
    fn sample(&self, index: usize) -> (String, ChangeRequestSpec) {
        let spec = change_request(index);
        (record_name(&spec.address(&[]).expect("address")).expect("record name"), spec)
    }
    fn edited(&self, index: usize) -> ChangeRequestSpec {
        change_request(index)
    }
    fn status(&self) -> Option<ChangeRequestStatus> {
        Some(ChangeRequestStatus {
            title: seen(Some("Stale writes".to_owned())),
            author: seen(Some("coder".to_owned())),
            review_decision: seen(None),
            review_requested_from_owner: seen(Some(false)),
            state: seen(Some(ObservedChangeRequestState::Open)),
            head_sha: seen(Some("abc".to_owned())),
            checks: seen(None),
            review: ChangeRequestReviewObservation { actionable_at_head: seen(None) },
            mergeable: seen(None),
        })
    }
}

/// Issues this home observes.
struct Issues;

fn issue(index: usize) -> IssueSpec {
    IssueSpec {
        service: "github.com".into(),
        scope: "flotilla-org/flotilla".into(),
        number: index as u64 + 1,
        observing_authority: "alpha".into(),
    }
}

impl Samples<Issue> for Issues {
    fn sample(&self, index: usize) -> (String, IssueSpec) {
        (format!("issue-{index}"), issue(index))
    }
    fn edited(&self, index: usize) -> IssueSpec {
        issue(index)
    }
    fn status(&self) -> Option<IssueStatus> {
        Some(IssueStatus {
            title: seen(Some("Stale writes".to_owned())),
            assignees: seen(Some(vec!["coder".to_owned()])),
            state: seen(Some(ObservedIssueState::Open)),
            labels: seen(Some(Vec::new())),
            updated_at: seen(Some(observed_at())),
        })
    }
}

#[tokio::test]
async fn the_host_port_refuses_stale_writes() {
    let subject = Written::<Host, _>::open(Hosts, |service| Arc::new(EngineHosts::open(service, "alpha".into(), clock()).expect("hosts")));
    contract::refuses_stale_writes(&subject).await;
}

#[tokio::test]
async fn the_project_port_refuses_stale_writes() {
    let subject = Written::<Project, _>::open(Projects, |service| Arc::new(EngineProjects::open(service, clock()).expect("projects")));
    contract::refuses_stale_writes(&subject).await;
}

#[tokio::test]
async fn the_observation_ports_refuse_stale_writes() {
    let observations = |service| Arc::new(EngineObservations::open(service, clock()).expect("observations"));
    contract::refuses_stale_writes(&Written::<ChangeRequest, _>::open(ChangeRequests, |service| observations(service))).await;
    contract::refuses_stale_writes(&Written::<Issue, _>::open(Issues, |service| observations(service))).await;
}
