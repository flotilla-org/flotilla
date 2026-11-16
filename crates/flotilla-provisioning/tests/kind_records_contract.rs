//! The engine's real ports keep the engine port contract that the in-memory port keeps for the
//! crates below them: a plain kind, a kind with a status, and a kind the engine holds, read through
//! its lifecycle views. A port that refuses what the in-memory one takes fails here.

use std::sync::Arc;

use flotilla_kind_records_testkit::{
    contract::{self, Samples, Subject},
    profile_of,
    samples::{Artifacts, Environments, Repositories},
    Profile,
};
use flotilla_provisioning::engine::{artifacts::EngineArtifacts, repositories::EngineRepositories};
use flotilla_provisioning_testkit::{environments::EnvironmentFixture, other_home::OtherHome, EngineFixture};
use flotilla_resources::{
    engine_records::KindRecords, environment_records::LaunchFacts, Artifact, Clock, Environment, Repository, Resource, SystemClock,
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
        theirs.apply(&name, &Default::default(), &Default::default(), &self.samples.edited(index)).await.expect("written at its home");
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
